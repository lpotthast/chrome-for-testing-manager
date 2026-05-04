use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio_process_tools::{
    BroadcastOutputStream, Consumer, Delivery, LineParsingOptions, Next, ProcessHandle, Replay,
    ReliableDelivery, ReplayEnabled,
};

/// The browser-driver output stream source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverOutputSource {
    /// The browser-driver process stdout stream.
    Stdout,

    /// The browser-driver process stderr stream.
    Stderr,
}

/// One parsed line from the browser-driver process output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DriverOutputLine {
    /// The output stream this line came from.
    pub source: DriverOutputSource,

    /// Monotonic callback-order sequence number across stdout and stderr.
    ///
    /// This is useful for rendering one combined output tail. It does not guarantee the original
    /// operating-system write order across separate stdout and stderr pipes.
    pub sequence: u64,

    /// The parsed output line without its trailing newline character.
    pub line: String,
}

/// Callback invoked for each parsed browser-driver output line.
#[derive(Clone)]
pub struct DriverOutputListener {
    on_line: Arc<dyn Fn(DriverOutputLine) + Send + Sync + 'static>,
}

impl fmt::Debug for DriverOutputListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverOutputListener")
            .field("on_line", &"<callback>")
            .finish()
    }
}

impl DriverOutputListener {
    /// Create a new browser-driver output listener from a callback.
    #[must_use]
    pub fn new(on_line: impl Fn(DriverOutputLine) + Send + Sync + 'static) -> Self {
        Self {
            on_line: Arc::new(on_line),
        }
    }

    pub(crate) fn emit(&self, line: DriverOutputLine) {
        (self.on_line)(line);
    }
}

/// Long-lived line-inspecting [`Consumer`] handles for the chromedriver process's stdout and
/// stderr streams.
///
/// Returned from [`crate::ChromeForTestingManager::launch_chromedriver`] alongside the process
/// handle and the bound port. Keep this value alive for as long as you want the configured
/// [`DriverOutputListener`] to receive lines; dropping it stops the listeners. When using the
/// high-level [`crate::Chromedriver`] entry point the inspectors are owned for you and dropped
/// when the chromedriver process is terminated.
pub struct DriverOutputInspectors {
    stdout: Consumer<()>,
    stderr: Consumer<()>,
}

impl fmt::Debug for DriverOutputInspectors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverOutputInspectors")
            .field("stdout_finished", &self.stdout.is_finished())
            .field("stderr_finished", &self.stderr.is_finished())
            .finish()
    }
}

impl DriverOutputInspectors {
    pub(crate) fn start(
        process: &ProcessHandle<BroadcastOutputStream<ReliableDelivery, ReplayEnabled>>,
        listener: Option<DriverOutputListener>,
    ) -> Self {
        let sequence = Arc::new(AtomicU64::new(0));
        Self {
            stdout: inspect_output(
                process.stdout(),
                DriverOutputSource::Stdout,
                Arc::clone(&sequence),
                listener.clone(),
            ),
            stderr: inspect_output(
                process.stderr(),
                DriverOutputSource::Stderr,
                sequence,
                listener,
            ),
        }
    }
}

fn inspect_output<D, R>(
    stream: &BroadcastOutputStream<D, R>,
    source: DriverOutputSource,
    sequence: Arc<AtomicU64>,
    listener: Option<DriverOutputListener>,
) -> Consumer<()>
where
    D: Delivery,
    R: Replay,
{
    stream.inspect_lines(
        move |line| {
            let line_ref: &str = &line;
            tracing::debug!(source = ?source, driver_output = line_ref, "driver log");

            if let Some(listener) = &listener {
                listener.emit(DriverOutputLine {
                    source,
                    sequence: sequence.fetch_add(1, Ordering::SeqCst),
                    line: line.into_owned(),
                });
            }

            Next::Continue
        },
        LineParsingOptions::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;
    use std::sync::Mutex;

    #[test]
    fn driver_output_listener_invokes_callback() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let listener = {
            let lines = Arc::clone(&lines);
            DriverOutputListener::new(move |line| {
                lines
                    .lock()
                    .expect("lines mutex should not be poisoned")
                    .push(line);
            })
        };

        listener.emit(DriverOutputLine {
            source: DriverOutputSource::Stdout,
            sequence: 0,
            line: "ready".to_owned(),
        });

        let lines = lines.lock().expect("lines mutex should not be poisoned");
        assert_that!(lines.as_slice()).contains_exactly([DriverOutputLine {
            source: DriverOutputSource::Stdout,
            sequence: 0,
            line: "ready".to_owned(),
        }]);
    }
}
