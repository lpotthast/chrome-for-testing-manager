//! Guarded child processes with captured output and cancellable startup.
//!
//! Both the `ChromeDriver` process and the Chrome Headless Shell process are a [`ManagedProcess`]:
//! spawned with the same output-stream configuration, guarded immediately (terminated on drop),
//! with output captured from spawn on. Both follow the same startup shape: scan one output stream
//! for a startup line, drive any further readiness checks, and terminate the child on cancellation
//! or startup failure. Startup errors carry the process's recent output.
//!
//! Termination never leaves an armed drop guard behind: if graceful termination fails, the process
//! is killed, and if that fails too, the failure is accepted and reported instead of panicking when
//! the handle is dropped. Output is drained before the handle is dropped, because dropping it
//! aborts the readers of its output pipes.

use crate::chromedriver::output::{DriverOutputLine, DriverOutputSubscription, OutputCapture};
use crate::error::{attach_child, operation_result_with_cleanup};
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use rootcause::{Report, bail, prelude::ResultExt, report};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;
use tokio_process_tools::{
    BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, GracefulShutdown,
    LineParsingOptions, NumBytesExt, Process, ReliableWithBackpressure, ReplayEnabled,
    RunningState, TerminateOnDrop, WaitForLineResult,
};

/// How long to wait for the exit of a process that closed its startup output.
const EXIT_OBSERVATION_GRACE: Duration = Duration::from_millis(500);

/// How long to wait for the output consumers of an exited process to process its final lines.
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) type ManagedProcessHandle =
    TerminateOnDrop<BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>>;

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
            })?
            .terminate_on_drop(shutdown.clone());
        let output = OutputCapture::start(&handle, name);
        Ok(Self {
            handle,
            output,
            name,
            artifact,
            executable: executable.to_owned(),
            shutdown,
        })
    }

    pub(crate) fn subscribe_output(&self) -> DriverOutputSubscription {
        self.output.subscribe()
    }

    pub(crate) fn recent_output(&self) -> Vec<DriverOutputLine> {
        self.output.recent_output()
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
            Ok(status) => return Ok(status),
            Err(error) => error,
        };
        let mut error = Report::new_sendsync(terminate_error).context(
            ChromeForTestingError::TerminateProcess {
                artifact,
                path: executable.to_owned(),
            },
        );
        match handle.kill().await {
            Ok(()) => {
                error = error.attach("the process was killed after graceful termination failed");
            }
            Err(kill_error) => {
                handle.must_not_be_terminated();
                tracing::error!(
                    process = name,
                    error = %kill_error,
                    "failed to kill process after graceful termination failed; it may still be running"
                );
                attach_child(&mut error, Report::new_sendsync(kill_error));
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
            Ok(WaitForLineResult::Timeout) => Err(self.startup_timeout_error(timeout)),
            Ok(WaitForLineResult::StreamClosed) => {
                let deadline = Instant::now() + EXIT_OBSERVATION_GRACE;
                loop {
                    self.ensure_running()?;
                    if Instant::now() >= deadline {
                        bail!(ChromeForTestingError::StartupOutputClosed {
                            artifact: self.artifact,
                            path: self.executable.clone(),
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// Return [`ChromeForTestingError::ExitedDuringStartup`] if the process has exited.
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

/// Render output lines as a report attachment, or `None` if there are none.
fn format_output(lines: &[DriverOutputLine]) -> Option<String> {
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
