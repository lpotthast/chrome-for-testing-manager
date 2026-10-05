//! Shared scaffolding for guarded child processes with cancellable startup.
//!
//! Both the `ChromeDriver` process and the Chrome Headless Shell process are spawned with the same
//! output-stream configuration, are guarded immediately, and follow the same startup shape: drive
//! a readiness future, and terminate the child on cancellation or startup failure.

use crate::error::operation_result_with_cleanup;
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use rootcause::{prelude::ResultExt, report};
use std::path::Path;
use tokio::process::Command;
use tokio_process_tools::{
    BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_MAX_LINE_LENGTH,
    DEFAULT_READ_CHUNK_SIZE, GracefulShutdown, LineOverflowBehavior, LineParsingOptions,
    NumBytesExt, Process, ReliableWithBackpressure, ReplayEnabled, TerminateOnDrop,
};

pub(crate) type ManagedProcessOutput =
    BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>;
pub(crate) type ManagedProcessHandle = TerminateOnDrop<ManagedProcessOutput>;

/// Spawn a child process that is terminated when its handle is dropped.
pub(crate) fn spawn_guarded(
    name: &'static str,
    command: Command,
    artifact: ChromeForTestingArtifact,
    executable: &Path,
    shutdown: GracefulShutdown,
) -> Result<ManagedProcessHandle> {
    Ok(Process::new(command)
        .name(name)
        .stdout_and_stderr(|stream| {
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
        .terminate_on_drop(shutdown))
}

/// Line-parsing options for scanning process startup output.
pub(crate) fn startup_line_options() -> LineParsingOptions {
    LineParsingOptions::builder()
        .max_line_length(DEFAULT_MAX_LINE_LENGTH)
        .overflow_behavior(LineOverflowBehavior::DropAdditionalData)
        .buffer_compaction_threshold(None)
        .build()
}

/// Drive `startup` while giving cancellation deterministic precedence.
///
/// On cancellation or startup failure the spawned process is terminated before the error is
/// returned, with any termination failure attached beneath the primary error.
pub(crate) async fn drive_startup<T>(
    mut process: ManagedProcessHandle,
    artifact: ChromeForTestingArtifact,
    executable: &Path,
    shutdown: GracefulShutdown,
    cancellation: &CancellationToken,
    startup: impl AsyncFnOnce(&mut ManagedProcessHandle) -> Result<T>,
) -> Result<(ManagedProcessHandle, T)> {
    let startup_result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(report!(ChromeForTestingError::Cancelled)),
        result = startup(&mut process) => result,
    };
    match startup_result {
        Ok(value) => Ok((process, value)),
        Err(startup_error) => {
            let cleanup_result =
                terminate_during_startup(&mut process, shutdown, artifact, executable).await;
            operation_result_with_cleanup(Err(startup_error), cleanup_result)
        }
    }
}

async fn terminate_during_startup(
    process: &mut ManagedProcessHandle,
    shutdown: GracefulShutdown,
    artifact: ChromeForTestingArtifact,
    executable: &Path,
) -> Result<()> {
    process
        .terminate(shutdown)
        .await
        .map(|_exit_status| ())
        .context(ChromeForTestingError::TerminateDuringStartup {
            artifact,
            path: executable.to_owned(),
        })
}
