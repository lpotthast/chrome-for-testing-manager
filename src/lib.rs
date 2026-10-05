//! Programmatic management of [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/)
//! installations.
//!
//! Resolves a `chrome` / `chromedriver` pair against the Chrome for Testing release index,
//! downloads it into a per-user cache, spawns `chromedriver` on a configurable or OS-assigned
//! port, and (with the default `thirtyfour` feature) provides scoped `WebDriver` sessions with
//! explicit async cleanup.
//!
//! Start with [`ChromeForTesting::launch`]. Reach for [`ChromeForTestingManager`] when you need
//! finer control over the resolve / download / launch steps.
//!
//! # Cancellation and drop safety
//!
//! Cancellation is opt-in. Pass a [`CancellationToken`] through
//! [`ChromeForTestingConfig::builder`], the session builder's `with_cancellation` step (feature
//! `thirtyfour`), or the lower-level [`ChromeForTestingManager`] methods to cancel work
//! cooperatively. Cancellation is reported as [`ChromeForTestingError::Cancelled`] after cleanup
//! has been drained.
//!
//! Cleanup-sensitive operations (artifact installation, process launch, `WebDriver` connection
//! and cleanup) transfer their work to a runtime-owned task. Dropping such a public future
//! signals cooperative cancellation instead of abandoning staged files, spawned processes, or an
//! in-flight session handshake; the task then drains extraction, terminates processes, or closes
//! the session while the Tokio runtime remains alive. This is an in-process guarantee, not
//! durable execution: runtime shutdown or process termination can prevent cleanup from
//! finishing, and once the public future is dropped its final value and cleanup errors are no
//! longer observable. Prefer explicit cancellation followed by awaiting the operation, and call
//! [`ChromeForTesting::shutdown`] for observable graceful shutdown.

mod artifact_store;
mod browser;
mod cache;
pub(crate) mod chromedriver;
mod error;
pub(crate) mod facade;
pub(crate) mod manager;
mod operation;
mod policy;
pub(crate) mod port;
pub(crate) mod process_support;
#[cfg(feature = "thirtyfour")]
pub(crate) mod session;
#[cfg(test)]
mod test_support;
pub(crate) mod version;

pub use ::chrome_for_testing::Channel;
pub use ::chrome_for_testing::Version;
pub use browser::{BrowserArtifactRequest, ChromeBinary, LoadedBrowserPackage};
pub use cache::CachePruneResult;
pub use chromedriver::ChromeDriverConfig;
pub use chromedriver::output::{
    DriverOutputLine, DriverOutputSource, DriverOutputSubscription, DriverOutputSubscriptionError,
};
pub use chromedriver::process::ChromeDriverProcess;
pub use error::{ChromeForTestingArtifact, ChromeForTestingError, Result};
pub use facade::{ChromeForTesting, ChromeForTestingConfig};
pub use manager::ChromeForTestingManager;
pub use manager::config::ChromeForTestingManagerConfig;
pub use policy::{LifecyclePolicy, NetworkPolicy};
pub use port::{Port, PortRequest};
use rootcause::report;
#[cfg(feature = "thirtyfour")]
pub use session::{Session, SessionBuilder};
use tokio::runtime::RuntimeFlavor;
pub use tokio_process_tools::{
    GracefulShutdown, GracefulShutdownBuilder, UnixGracefulPhase, UnixGracefulShutdown,
    UnixGracefulSignal, WindowsGracefulShutdown,
};
pub use tokio_util::sync::CancellationToken;
pub use version::{SelectedVersion, VersionRequest};

/// Await a drop-safe operation while giving cancellation deterministic precedence.
///
/// The operation future is dropped when cancellation wins. Resource-acquiring handshakes whose
/// cancellation requires explicit cleanup must use their own `tokio::select!` instead.
pub(crate) async fn await_or_cancelled<T>(
    cancellation: &CancellationToken,
    operation: impl Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            Err(report!(ChromeForTestingError::Cancelled))
        }
        output = operation => Ok(output),
    }
}

/// Return [`ChromeForTestingError::Cancelled`] when the token has been cancelled.
pub(crate) fn check_cancelled(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(report!(ChromeForTestingError::Cancelled));
    }
    Ok(())
}

pub(crate) fn ensure_multithreaded_runtime() -> Result<()> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| report!(ChromeForTestingError::MissingRuntime))?;
    match handle.runtime_flavor() {
        RuntimeFlavor::MultiThread => Ok(()),
        unsupported_flavor => Err(report!(ChromeForTestingError::UnsupportedRuntime {
            runtime_flavor: unsupported_flavor,
        })),
    }
}

#[cfg(test)]
mod tests {
    use assertr::prelude::*;

    #[test]
    fn missing_runtime_is_reported_as_a_typed_error() {
        let error = super::ensure_multithreaded_runtime()
            .expect_err("a thread without a Tokio runtime must be rejected");
        assert_that!(matches!(
            error.current_context(),
            super::ChromeForTestingError::MissingRuntime
        ))
        .is_true();
    }
}
