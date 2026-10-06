//! Guarded child processes with captured output and cancellable startup.
//!
//! Both the `ChromeDriver` process and the Chrome Headless Shell process are a [`ManagedProcess`]:
//! spawned with the same output-stream configuration, guarded immediately (terminated on drop),
//! with output captured from spawn on. Both follow the same startup shape: scan one output stream
//! for a startup line, drive any further readiness checks, and terminate the child on cancellation
//! or startup failure. Startup errors carry the process's recent output.
//!
//! Termination never leaves an armed drop guard behind: if graceful termination fails, the process
//! is killed, and if that fails too (or the killed process does not exit in time), the failure is
//! accepted and reported instead of retrying termination or panicking when the handle is dropped.
//! Output is drained before the handle is dropped, because dropping it aborts the readers of its
//! output pipes.

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
use tokio::runtime::RuntimeFlavor;
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
/// never exit; it is then abandoned instead of blocking shutdown forever.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

type ManagedOutputStream = BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>;

/// A process handle that terminates its process when dropped.
///
/// Like `tokio_process_tools::TerminateOnDrop`, but disarmable: once the process was terminated, or
/// termination failed and that failure was accepted, dropping the handle must neither block a
/// runtime worker on yet another termination attempt nor require a runtime. Dropping an armed
/// handle requires an active multithreaded Tokio runtime and panics otherwise, as does a failed
/// termination during drop.
#[derive(Debug)]
pub(crate) struct ManagedProcessHandle {
    inner: ProcessHandle<ManagedOutputStream>,
    name: &'static str,
    shutdown: GracefulShutdown,
    terminate_on_drop: bool,
}

impl ManagedProcessHandle {
    /// Accept that the process may still be running: dropping the handle neither retries
    /// termination nor panics.
    fn disarm(&mut self) {
        self.terminate_on_drop = false;
        self.inner.must_not_be_terminated();
    }
}

impl Deref for ManagedProcessHandle {
    type Target = ProcessHandle<ManagedOutputStream>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ManagedProcessHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for ManagedProcessHandle {
    fn drop(&mut self) {
        // Panicking again while unwinding would abort; the inner handle's own guard still sends
        // a kill signal when it drops.
        if !self.terminate_on_drop || std::thread::panicking() {
            return;
        }
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) if runtime.runtime_flavor() == RuntimeFlavor::MultiThread => runtime,
            _ => panic!(
                "the {} process handle was dropped outside of an active multi-threaded Tokio \
                 runtime, which is required to terminate the process",
                self.name
            ),
        };
        tokio::task::block_in_place(|| {
            runtime.block_on(async {
                if let RunningState::Terminated(_) = self.inner.is_running() {
                    self.inner.must_not_be_terminated();
                    return;
                }
                if let Err(error) = self.inner.terminate(self.shutdown.clone()).await {
                    tracing::error!(
                        process = self.name,
                        %error,
                        "failed to terminate process while dropping its handle"
                    );
                }
            });
        });
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
    /// Not the startup line; keep waiting.
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
    name: &'static str,
    artifact: ChromeForTestingArtifact,
    executable: PathBuf,
    shutdown: GracefulShutdown,
    /// Why the process state could last not be observed, if it could not.
    state_error: Option<std::io::Error>,
}

impl ManagedProcess {
    /// Spawn `command` and start capturing its output. `name` labels the process in tracing
    /// output; `artifact` and `executable` identify it in errors.
    pub(crate) fn spawn(
        name: &'static str,
        artifact: ChromeForTestingArtifact,
        executable: &Path,
        command: Command,
        shutdown: GracefulShutdown,
    ) -> Result<Self> {
        let inner = Process::new(command)
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
            inner,
            name,
            shutdown: shutdown.clone(),
            terminate_on_drop: true,
        };
        let output = OutputCapture::start(&handle, name);
        Ok(Self {
            handle,
            output,
            name,
            artifact,
            executable: executable.to_owned(),
            shutdown,
            state_error: None,
        })
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
        let Self {
            mut handle,
            output,
            name,
            artifact,
            executable,
            shutdown,
            state_error: _,
        } = self;
        let result =
            Self::terminate_handle(&mut handle, &shutdown, name, artifact, &executable).await;
        let recent_output = output.finish(OUTPUT_DRAIN_TIMEOUT).await;
        drop(handle);
        (result, recent_output)
    }

    /// The handle's drop guards are always settled afterwards: if even the kill fails, the failure
    /// is accepted (and reported) rather than leaving a guard armed that would block on a retry and
    /// then panic when the handle is dropped.
    async fn terminate_handle(
        handle: &mut ManagedProcessHandle,
        shutdown: &GracefulShutdown,
        name: &'static str,
        artifact: ChromeForTestingArtifact,
        executable: &Path,
    ) -> Result<ExitStatus> {
        let terminate_error = match handle.terminate(shutdown.clone()).await {
            Ok(status) => {
                // The process exited: dropping the handle must not require a runtime anymore.
                handle.disarm();
                return Ok(status);
            }
            Err(error) => error,
        };
        let mut error = Report::new_sendsync(terminate_error).context(
            ChromeForTestingError::TerminateProcess {
                artifact,
                path: executable.to_owned(),
            },
        );
        match tokio::time::timeout(KILL_TIMEOUT, handle.kill()).await {
            Ok(Ok(())) => {
                handle.disarm();
                error = error.attach("the process was killed after graceful termination failed");
            }
            Ok(Err(kill_error)) => {
                handle.disarm();
                tracing::error!(
                    process = name,
                    error = %kill_error,
                    "failed to kill process after graceful termination failed; it may still be running"
                );
                attach_child(&mut error, Report::new_sendsync(kill_error));
            }
            Err(_) => {
                handle.disarm();
                tracing::error!(
                    process = name,
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
                    artifact: self.artifact,
                    path: self.executable.clone(),
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
                        artifact: self.artifact,
                        path: self.executable.clone(),
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
                    self.ensure_running()?;
                    if Instant::now() >= deadline {
                        let mut error = report!(ChromeForTestingError::StartupOutputClosed {
                            artifact: self.artifact,
                            path: self.executable.clone(),
                        });
                        if let Some(state_error) = self.state_error.take() {
                            attach_child(&mut error, Report::new_sendsync(state_error));
                        }
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// Return [`ChromeForTestingError::ExitedDuringStartup`] if the process has exited.
    ///
    /// A process whose state cannot be observed counts as running; the failure is remembered for
    /// [`ChromeForTestingError::StartupOutputClosed`].
    pub(crate) fn ensure_running(&mut self) -> Result<()> {
        match self.handle.is_running() {
            RunningState::Running => Ok(()),
            RunningState::Terminated(status) => {
                Err(report!(ChromeForTestingError::ExitedDuringStartup {
                    artifact: self.artifact,
                    path: self.executable.clone(),
                    status,
                }))
            }
            RunningState::Uncertain(error) => {
                tracing::debug!(process = self.name, %error, "could not determine process state");
                self.state_error = Some(error);
                Ok(())
            }
        }
    }

    /// [`ChromeForTestingError::WaitForStartup`] for this process.
    pub(crate) fn startup_timeout_error(&self, timeout: Duration) -> Report<ChromeForTestingError> {
        report!(ChromeForTestingError::WaitForStartup {
            artifact: self.artifact,
            path: self.executable.clone(),
            timeout,
        })
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.executable
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
