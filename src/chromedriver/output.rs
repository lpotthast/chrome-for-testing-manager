//! Structured output observation for managed child processes.
//!
//! [`OutputCapture`] continuously drains a child's stdout and stderr from spawn on. It keeps a
//! bounded history of recent lines, which startup errors attach and
//! [`crate::ChromeDriverProcess::recent_output`] exposes, and distributes lines through bounded
//! non-blocking subscriptions without backpressuring the child process. Only the stream consumers
//! hold strong senders, so subscriptions observe [`DriverOutputSubscriptionError::Closed`] as soon
//! as both output streams end, whether the process was terminated or exited on its own.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::process_support::ManagedProcessHandle;
use tokio::sync::broadcast;
use tokio_process_tools::{
    BroadcastOutputStream, Consumable, Consumer, Delivery, LineParsingOptions, Next, ParseLines,
    Replay,
};
use unwrap_infallible::UnwrapInfallible;

/// Number of output lines retained by each non-blocking subscriber.
const OUTPUT_CHANNEL_CAPACITY: usize = 1_024;

/// Number of most recent output lines retained as history.
const OUTPUT_HISTORY_LINES: usize = 256;

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
#[non_exhaustive]
pub struct DriverOutputLine {
    /// The output stream this line came from.
    pub source: DriverOutputSource,

    /// The parsed output line without its line terminator (`\n` or `\r\n`).
    pub line: String,
}

impl DriverOutputLine {
    /// Create an output line, e.g. to feed recorded or fixture output into code under test.
    #[must_use]
    pub fn new(source: DriverOutputSource, line: impl Into<String>) -> Self {
        Self {
            source,
            line: line.into(),
        }
    }
}

impl fmt::Display for DriverOutputLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match self.source {
            DriverOutputSource::Stdout => "stdout",
            DriverOutputSource::Stderr => "stderr",
        };
        write!(f, "[{source}] {}", self.line)
    }
}

/// Error returned while receiving non-blocking `ChromeDriver` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
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
    /// the bounded channel. This is recoverable: the subscription continues with the oldest line
    /// still retained, so keep receiving. Returns [`DriverOutputSubscriptionError::Closed`] once
    /// both process output streams have ended; no further lines will arrive.
    pub async fn recv(&mut self) -> Result<DriverOutputLine, DriverOutputSubscriptionError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(skipped) => {
                DriverOutputSubscriptionError::Lagged { skipped }
            }
            broadcast::error::RecvError::Closed => DriverOutputSubscriptionError::Closed,
        })
    }
}

type OutputHistory = Arc<Mutex<VecDeque<DriverOutputLine>>>;

/// Line-inspecting consumers of a managed process's stdout and stderr, started right after spawn
/// and kept for the process lifetime.
pub(crate) struct OutputCapture {
    stdout: Consumer<()>,
    stderr: Consumer<()>,
    history: OutputHistory,
    /// Weak, so that the channel closes once both consumers (the only strong senders) finish.
    sender: broadcast::WeakSender<DriverOutputLine>,
}

impl fmt::Debug for OutputCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputCapture")
            .field("stdout_finished", &self.stdout.is_finished())
            .field("stderr_finished", &self.stderr.is_finished())
            .field(
                "subscribers",
                &self
                    .sender
                    .upgrade()
                    .map_or(0, |sender| sender.receiver_count()),
            )
            .finish_non_exhaustive()
    }
}

impl OutputCapture {
    /// Start capturing both output streams. `name` labels the lines in tracing output.
    pub(crate) fn start(process: &ManagedProcessHandle, name: &'static str) -> Self {
        let (sender, _) = broadcast::channel(OUTPUT_CHANNEL_CAPACITY);
        let history = OutputHistory::default();
        Self {
            stdout: Self::inspect_output(
                process.stdout(),
                name,
                DriverOutputSource::Stdout,
                &sender,
                &history,
            ),
            stderr: Self::inspect_output(
                process.stderr(),
                name,
                DriverOutputSource::Stderr,
                &sender,
                &history,
            ),
            history,
            sender: sender.downgrade(),
        }
    }

    pub(crate) fn subscribe(&self) -> DriverOutputSubscription {
        let receiver = match self.sender.upgrade() {
            Some(sender) => sender.subscribe(),
            // Output already ended: hand out a subscription that reports `Closed` immediately.
            None => broadcast::channel(1).1,
        };
        DriverOutputSubscription { receiver }
    }

    /// Return the recent output together with a subscription to every later line.
    ///
    /// Lines are recorded and published under the history lock, which is held here while taking
    /// the snapshot and subscribing, so every line is either in the snapshot or delivered to the
    /// subscription, never both.
    pub(crate) fn subscribe_with_history(
        &self,
    ) -> (Vec<DriverOutputLine>, DriverOutputSubscription) {
        let history = self
            .history
            .lock()
            .expect("output history mutex is not poisoned");
        let subscription = self.subscribe();
        (history.iter().cloned().collect(), subscription)
    }

    /// Return up to the last [`OUTPUT_HISTORY_LINES`] lines, oldest first.
    pub(crate) fn recent_output(&self) -> Vec<DriverOutputLine> {
        self.history
            .lock()
            .expect("output history mutex is not poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// Wait up to `timeout` for both consumers to process the remaining output of an exited
    /// process, then stop them and return the recent output. Without this, the final lines could
    /// be lost. Subscriptions observe `Closed` afterwards.
    pub(crate) async fn finish(self, timeout: Duration) -> Vec<DriverOutputLine> {
        let Self {
            stdout,
            stderr,
            history,
            sender: _,
        } = self;
        let drained = tokio::time::timeout(timeout, async {
            let (stdout, stderr) = tokio::join!(stdout.wait(), stderr.wait());
            for result in [stdout, stderr] {
                if let Err(error) = result {
                    tracing::debug!(%error, "output consumer ended with an error");
                }
            }
        })
        .await;
        if drained.is_err() {
            tracing::debug!("output consumers did not finish in time and were aborted");
        }
        let history = history
            .lock()
            .expect("output history mutex is not poisoned");
        history.iter().cloned().collect()
    }

    fn inspect_output<D, R>(
        stream: &BroadcastOutputStream<D, R>,
        name: &'static str,
        source: DriverOutputSource,
        sender: &broadcast::Sender<DriverOutputLine>,
        history: &OutputHistory,
    ) -> Consumer<()>
    where
        D: Delivery,
        R: Replay,
    {
        let sender = sender.clone();
        let history = Arc::clone(history);
        stream
            .consume(ParseLines::inspect(
                LineParsingOptions::default(),
                move |line| {
                    // Line parsing splits at `\n` only, which leaves the `\r` of `\r\n`.
                    let line_ref: &str = line.strip_suffix('\r').unwrap_or(&line);
                    tracing::debug!(process = name, source = ?source, output = line_ref, "process output");

                    let line = DriverOutputLine::new(source, line_ref);
                    // Publish under the history lock; see `subscribe_with_history`.
                    let mut history = history.lock().expect("output history mutex is not poisoned");
                    if history.len() == OUTPUT_HISTORY_LINES {
                        history.pop_front();
                    }
                    history.push_back(line.clone());
                    let _ = sender.send(line);

                    Next::Continue
                },
            ))
            .unwrap_infallible()
    }
}
