//! Guarded child processes with captured output and cancellable startup.
//!
//! Both the `ChromeDriver` process and the Chrome Headless Shell process are a [`ManagedProcess`]:
//! spawned with the same output-stream configuration, guarded immediately (terminated gracefully
//! in the background when dropped), with output captured from spawn on. Both follow the same startup shape: scan one output stream
//! for a startup line, drive any further readiness checks, and terminate the child on cancellation
//! or startup failure. Startup errors carry the process's recent output.
//!
//! Termination never leaves an armed drop guard behind: if graceful termination fails, the process
//! is killed, and if that fails too (or the killed process does not exit in time), the failure is
//! accepted and reported instead of retrying termination when the handle is dropped. Dropping a
//! handle never blocks and never panics.
//! Output is drained before the handle is dropped, because dropping it aborts the readers of its
//! output pipes.

use crate::background::BackgroundTasks;
use crate::cache::CacheLease;
use crate::chromedriver::output::{DriverOutputLine, DriverOutputSubscription, OutputCapture};
use crate::error::{attach_child, operation_result_with_cleanup};
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use rootcause::{Report, prelude::ResultExt, report};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;
use tokio_process_tools::{
    BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, GracefulShutdown,
    LineParsingOptions, NumBytesExt, Process, ProcessHandle, ReliableWithBackpressure,
    ReplayEnabled, RunningState, WaitForLineResult,
};

/// How long to wait for the exit of a process that closed its startup output.
const EXIT_OBSERVATION_GRACE: Duration = Duration::from_millis(500);

/// How long to wait for the output consumers of an exited process to process its final lines.
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to wait for a killed process to exit. A process stuck in uninterruptible I/O may
/// never exit. It is then abandoned instead of blocking shutdown forever.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

type ManagedOutputStream = BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>;

/// A spawned process, together with everything terminating it requires.
///
/// Dropping it before [`Self::terminate`] ran kills the process: the last resort, used only when
/// graceful termination can no longer be awaited (no runtime is left to drive it).
#[derive(Debug)]
struct GuardedProcess {
    handle: ProcessHandle<ManagedOutputStream>,
    name: &'static str,
    artifact: ChromeForTestingArtifact,
    executable: PathBuf,
    shutdown: GracefulShutdown,
    /// Keeps the executable's cache entry from being removed while the process runs.
    _cache_lease: CacheLease,
    /// Whether termination ran. Its failure, if any, was accepted and reported.
    settled: bool,
}

impl GuardedProcess {
    /// Terminate the process with its shutdown policy, escalating to a kill on failure.
    ///
    /// Settles the process either way: if even the kill fails, the failure is accepted (and
    /// reported) rather than retrying termination when the process is dropped.
    async fn terminate(&mut self) -> Result<ExitStatus> {
        self.settled = true;
        let terminate_error = match self.handle.terminate(self.shutdown.clone()).await {
            Ok(status) => return Ok(status),
            Err(error) => error,
        };
        let mut error = Report::new_sendsync(terminate_error).context(
            ChromeForTestingError::TerminateProcess {
                artifact: self.artifact,
                path: self.executable.clone(),
            },
        );
        match tokio::time::timeout(KILL_TIMEOUT, self.handle.kill()).await {
            Ok(Ok(())) => {
                error = error.attach("the process was killed after graceful termination failed");
            }
            Ok(Err(kill_error)) => {
                self.handle.must_not_be_terminated();
                tracing::error!(
                    process = self.name,
                    error = %kill_error,
                    "failed to kill process after graceful termination failed; it may still be running"
                );
                attach_child(&mut error, Report::new_sendsync(kill_error));
            }
            Err(_) => {
                self.handle.must_not_be_terminated();
                tracing::error!(
                    process = self.name,
                    timeout = ?KILL_TIMEOUT,
                    "killed process did not exit in time; it may still be running"
                );
                error = error.attach(format!(
                    "the process was killed after graceful termination failed, but did not exit \
                     within {KILL_TIMEOUT:?}; it may still be running"
                ));
            }
        }
        Err(error)
    }
}

impl Drop for GuardedProcess {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        tracing::warn!(
            process = self.name,
            "no Tokio runtime is left to gracefully terminate a dropped process; killing it"
        );
        if let Err(error) = self.handle.start_kill() {
            tracing::error!(
                process = self.name,
                %error,
                "failed to kill a dropped process; it may still be running"
            );
            self.handle.must_not_be_terminated();
        }
    }
}

/// Owns a [`GuardedProcess`] and gracefully terminates it when dropped, without blocking.
///
/// Drop cannot await termination, so it hands the process to a background task on the current
/// Tokio runtime (of any flavor), which [`BackgroundTasks::wait`] waits for. Without a runtime,
/// or if the runtime drops that task before it ran, the process is killed instead.
#[derive(Debug)]
pub(crate) struct ManagedProcessHandle {
    /// Present until dropped.
    process: Option<GuardedProcess>,
    background: BackgroundTasks,
}

impl ManagedProcessHandle {
    fn process(&self) -> &GuardedProcess {
        self.process
            .as_ref()
            .expect("the process is present until the handle is dropped")
    }

    fn process_mut(&mut self) -> &mut GuardedProcess {
        self.process
            .as_mut()
            .expect("the process is present until the handle is dropped")
    }
}

impl Deref for ManagedProcessHandle {
    type Target = ProcessHandle<ManagedOutputStream>;

    fn deref(&self) -> &Self::Target {
        &self.process().handle
    }
}

impl DerefMut for ManagedProcessHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process_mut().handle
    }
}

impl Drop for ManagedProcessHandle {
    fn drop(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        if process.settled {
            return;
        }
        // Without a runtime, dropping `process` kills it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            self.background
                .spawn_cleanup(async move { process.terminate().await.map(drop) }, &runtime);
        }
    }
}

/// The output stream carrying a process's startup line.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StartupStream {
    Stdout,
    /// Used by Chrome Headless Shell sessions only.
    #[cfg_attr(not(feature = "thirtyfour"), expect(dead_code))]
    Stderr,
}

/// How a startup-line classifier judged one output line.
pub(crate) enum StartupLine<T> {
    /// Not the startup line. Keep waiting.
    Ignore,

    /// The startup line, carrying the value parsed from it.
    Ready(T),

    /// The startup line, but in a format the value could not be parsed from.
    Unrecognized,
}

/// A spawned child process that is terminated when dropped, together with its captured output.
#[derive(Debug)]
pub(crate) struct ManagedProcess {
    handle: ManagedProcessHandle,
    output: OutputCapture,
}

impl ManagedProcess {
    /// Spawn `command` and start capturing its output. `name` labels the process in tracing
    /// output, and `artifact` and `executable` identify it in errors. `cache_lease` is held until
    /// the process was terminated, and a dropped process is terminated as one of the `background`
    /// tasks.
    pub(crate) fn spawn(
        name: &'static str,
        artifact: ChromeForTestingArtifact,
        executable: &Path,
        command: Command,
        shutdown: GracefulShutdown,
        cache_lease: CacheLease,
        background: &BackgroundTasks,
    ) -> Result<Self> {
        crate::ensure_runtime()?;
        let handle = Process::new(command)
            .name(name)
            .stdout_and_stderr(|stream| {
                // Replay covers the gap between spawn and the subscriptions below; it is sealed
                // once startup completes.
                stream
                    .broadcast()
                    .reliable_with_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .context(ChromeForTestingError::SpawnProcess {
                artifact,
                path: executable.to_owned(),
            })?;
        let handle = ManagedProcessHandle {
            process: Some(GuardedProcess {
                handle,
                name,
                artifact,
                executable: executable.to_owned(),
                shutdown,
                _cache_lease: cache_lease,
                settled: false,
            }),
            background: background.clone(),
        };
        let output = OutputCapture::start(&handle, name);
        Ok(Self { handle, output })
    }

    pub(crate) fn subscribe_output(&self) -> DriverOutputSubscription {
        self.output.subscribe()
    }

    pub(crate) fn recent_output(&self) -> Vec<DriverOutputLine> {
        self.output.recent_output()
    }

    pub(crate) fn subscribe_output_with_history(
        &self,
    ) -> (Vec<DriverOutputLine>, DriverOutputSubscription) {
        self.output.subscribe_with_history()
    }

    /// Drive `startup` while giving cancellation deterministic precedence.
    ///
    /// On success, output replay is sealed: every consumer is attached by then. On cancellation
    /// or startup failure, the process is terminated before the error is returned, with any
    /// termination failure attached beneath the primary error. Startup failures also carry the
    /// process's recent output.
    pub(crate) async fn start<T>(
        mut self,
        cancellation: &CancellationToken,
        startup: impl AsyncFnOnce(&mut Self) -> Result<T>,
    ) -> Result<(Self, T)> {
        let startup_result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(report!(ChromeForTestingError::Cancelled)),
            result = startup(&mut self) => result,
        };
        let mut startup_error = match startup_result {
            Ok(value) => {
                self.handle.seal_output_replay();
                return Ok((self, value));
            }
            Err(error) => error,
        };
        let (cleanup_result, recent_output) = self.shut_down().await;
        if !matches!(
            startup_error.current_context(),
            ChromeForTestingError::Cancelled
        ) && let Some(attachment) = format_output(&recent_output)
        {
            startup_error = startup_error.attach(attachment);
        }
        operation_result_with_cleanup(Err(startup_error), cleanup_result)
    }

    /// Terminate the process with its shutdown policy, escalating to a kill on failure, and drain
    /// its remaining output. Output subscriptions observe `Closed` afterwards.
    pub(crate) async fn terminate(self) -> Result<ExitStatus> {
        self.shut_down().await.0
    }

    async fn shut_down(self) -> (Result<ExitStatus>, Vec<DriverOutputLine>) {
        let Self { mut handle, output } = self;
        let result = handle.process_mut().terminate().await;
        let recent_output = output.finish(OUTPUT_DRAIN_TIMEOUT).await;
        // Releases the cache lease, now that the process has exited.
        drop(handle);
        (result, recent_output)
    }

    /// Wait until `classify` recognizes the startup line on `stream`.
    ///
    /// A closed stream is reported as [`ChromeForTestingError::ExitedDuringStartup`] when the
    /// process exit can be observed shortly afterwards, and as
    /// [`ChromeForTestingError::StartupOutputClosed`] otherwise.
    pub(crate) async fn wait_for_startup_line<T: Send + 'static>(
        &mut self,
        stream: StartupStream,
        timeout: Duration,
        classify: impl Fn(&str) -> StartupLine<T> + Send + 'static,
    ) -> Result<T> {
        // `Err` carries the unrecognized startup line.
        let outcome = Arc::new(Mutex::new(None::<std::result::Result<T, String>>));
        let callback_outcome = Arc::clone(&outcome);
        let output = match stream {
            StartupStream::Stdout => self.handle.stdout(),
            StartupStream::Stderr => self.handle.stderr(),
        };
        let wait_result = output
            .wait_for_line(
                timeout,
                move |line| {
                    let judged = match classify(&line) {
                        StartupLine::Ignore => return false,
                        StartupLine::Ready(value) => Ok(value),
                        StartupLine::Unrecognized => Err(line.into_owned()),
                    };
                    *callback_outcome
                        .lock()
                        .expect("startup outcome mutex is not poisoned") = Some(judged);
                    true
                },
                LineParsingOptions::default(),
            )
            .await;

        match wait_result {
            Err(read_error) => Err(Report::new_sendsync(read_error).context(
                ChromeForTestingError::ReadStartupOutput {
                    artifact: self.artifact(),
                    path: self.executable().to_owned(),
                },
            )),
            Ok(WaitForLineResult::Matched) => {
                let judged = outcome
                    .lock()
                    .expect("startup outcome mutex is not poisoned")
                    .take()
                    .expect("a matched startup line records its outcome");
                judged.map_err(|line| {
                    report!(ChromeForTestingError::UnrecognizedStartupOutput {
                        artifact: self.artifact(),
                        path: self.executable().to_owned(),
                        line,
                    })
                })
            }
            Ok(WaitForLineResult::Timeout) => {
                // A child process inheriting the output pipe can keep it open after an exit.
                self.ensure_running()?;
                Err(self.startup_timeout_error(timeout))
            }
            Ok(WaitForLineResult::StreamClosed) => {
                let deadline = Instant::now() + EXIT_OBSERVATION_GRACE;
                loop {
                    // Unlike `ensure_running`, keep why the state could not be observed: it may
                    // explain the closed output.
                    let state_error = match self.handle.is_running() {
                        RunningState::Running => None,
                        RunningState::Terminated(status) => {
                            return Err(self.exited_during_startup(status));
                        }
                        RunningState::Uncertain(error) => Some(error),
                    };
                    if Instant::now() >= deadline {
                        let mut error = report!(ChromeForTestingError::StartupOutputClosed {
                            artifact: self.artifact(),
                            path: self.executable().to_owned(),
                        });
                        if let Some(state_error) = state_error {
                            attach_child(&mut error, Report::new_sendsync(state_error));
                        }
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// Return [`ChromeForTestingError::ExitedDuringStartup`] if the process has exited. A process
    /// whose state cannot be observed counts as running.
    pub(crate) fn ensure_running(&mut self) -> Result<()> {
        match self.handle.is_running() {
            RunningState::Running => Ok(()),
            RunningState::Terminated(status) => Err(self.exited_during_startup(status)),
            RunningState::Uncertain(error) => {
                tracing::debug!(process = self.handle.process().name, %error, "could not determine process state");
                Ok(())
            }
        }
    }

    /// [`ChromeForTestingError::ExitedDuringStartup`] for this process.
    fn exited_during_startup(&self, status: ExitStatus) -> Report<ChromeForTestingError> {
        report!(ChromeForTestingError::ExitedDuringStartup {
            artifact: self.artifact(),
            path: self.executable().to_owned(),
            status,
        })
    }

    /// [`ChromeForTestingError::WaitForStartup`] for this process.
    pub(crate) fn startup_timeout_error(&self, timeout: Duration) -> Report<ChromeForTestingError> {
        report!(ChromeForTestingError::WaitForStartup {
            artifact: self.artifact(),
            path: self.executable().to_owned(),
            timeout,
        })
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.handle.process().executable
    }

    fn artifact(&self) -> ChromeForTestingArtifact {
        self.handle.process().artifact
    }
}

/// The instant `timeout` from now, saturating far in the future for huge timeouts such as
/// [`Duration::MAX`], which callers use to disable a deadline.
pub(crate) fn deadline_after(timeout: Duration) -> Instant {
    const FAR_FUTURE: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);
    let now = Instant::now();
    now.checked_add(timeout).unwrap_or_else(|| now + FAR_FUTURE)
}

/// Render output lines as a report attachment, or `None` if there are none.
pub(crate) fn format_output(lines: &[DriverOutputLine]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut attachment = format!("last {} output lines:", lines.len());
    for line in lines {
        attachment.push('\n');
        attachment.push_str(&line.to_string());
    }
    Some(attachment)
}
