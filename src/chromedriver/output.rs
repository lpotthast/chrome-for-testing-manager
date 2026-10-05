//! Structured `ChromeDriver` output observation.
//!
//! Output is continuously drained and distributed through bounded non-blocking subscriptions
//! without backpressuring the child process.

use std::fmt;

use tokio::sync::broadcast;
use tokio_process_tools::{
    BroadcastOutputStream, Consumable, Consumer, Delivery, LineParsingOptions, Next, ParseLines,
    ProcessHandle, ReliableWithBackpressure, Replay, ReplayEnabled,
};
use unwrap_infallible::UnwrapInfallible;

/// Number of driver output lines retained by each non-blocking subscriber.
const OUTPUT_CHANNEL_CAPACITY: usize = 1_024;

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

    /// The parsed output line without its trailing newline character.
    pub line: String,
}

/// Error returned while receiving non-blocking `ChromeDriver` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DriverOutputSubscriptionError {
    /// The subscriber fell behind the bounded output channel.
    #[error("driver output subscriber lagged by {skipped} lines")]
    Lagged {
        /// Number of lines skipped by the bounded channel.
        skipped: u64,
    },

    /// The `ChromeDriver` output channel has closed.
    #[error("driver output channel closed")]
    Closed,
}

/// A bounded, non-blocking subscription to future `ChromeDriver` output lines.
pub struct DriverOutputSubscription {
    receiver: broadcast::Receiver<DriverOutputLine>,
}

impl fmt::Debug for DriverOutputSubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverOutputSubscription")
            .finish_non_exhaustive()
    }
}

impl DriverOutputSubscription {
    /// Receive the next output line.
    ///
    /// # Errors
    ///
    /// Returns [`DriverOutputSubscriptionError::Lagged`] when this subscriber did not keep up with
    /// the bounded channel, or [`DriverOutputSubscriptionError::Closed`] after process output ends.
    pub async fn recv(&mut self) -> Result<DriverOutputLine, DriverOutputSubscriptionError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(skipped) => {
                DriverOutputSubscriptionError::Lagged { skipped }
            }
            broadcast::error::RecvError::Closed => DriverOutputSubscriptionError::Closed,
        })
    }
}

/// Long-lived line-inspecting [`Consumer`] handles for the chromedriver process's stdout and
/// stderr streams.
///
/// Owned internally by [`crate::ChromeDriverProcess`] for the process lifetime.
pub(crate) struct DriverOutputInspectors {
    stdout: Consumer<()>,
    stderr: Consumer<()>,
    sender: broadcast::Sender<DriverOutputLine>,
}

impl fmt::Debug for DriverOutputInspectors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverOutputInspectors")
            .field("stdout_finished", &self.stdout.is_finished())
            .field("stderr_finished", &self.stderr.is_finished())
            .field("subscribers", &self.sender.receiver_count())
            .finish()
    }
}

impl DriverOutputInspectors {
    pub(crate) fn start(
        process: &ProcessHandle<BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>>,
    ) -> Self {
        let (sender, _) = broadcast::channel(OUTPUT_CHANNEL_CAPACITY);
        Self {
            stdout: Self::inspect_output(process.stdout(), DriverOutputSource::Stdout, &sender),
            stderr: Self::inspect_output(process.stderr(), DriverOutputSource::Stderr, &sender),
            sender,
        }
    }

    pub(crate) fn subscribe(&self) -> DriverOutputSubscription {
        DriverOutputSubscription {
            receiver: self.sender.subscribe(),
        }
    }

    fn inspect_output<D, R>(
        stream: &BroadcastOutputStream<D, R>,
        source: DriverOutputSource,
        sender: &broadcast::Sender<DriverOutputLine>,
    ) -> Consumer<()>
    where
        D: Delivery,
        R: Replay,
    {
        let sender = sender.clone();
        stream
            .consume(ParseLines::inspect(
                LineParsingOptions::default(),
                move |line| {
                    let line_ref: &str = &line;
                    tracing::debug!(source = ?source, driver_output = line_ref, "driver log");

                    let _ = sender.send(DriverOutputLine {
                        source,
                        line: line.into_owned(),
                    });

                    Next::Continue
                },
            ))
            .unwrap_infallible()
    }
}
