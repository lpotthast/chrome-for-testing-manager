#![cfg_attr(feature = "thirtyfour", doc = include_str!("../README.md"))]
#![cfg_attr(
    not(feature = "thirtyfour"),
    doc = r"Programmatic management of [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/)
installations.

Resolves a `chrome` / `chromedriver` pair against the Chrome for Testing release index, downloads
it into a per-user cache, and spawns `chromedriver` on a configurable or OS-assigned port. Enable
the default `thirtyfour` feature for scoped `WebDriver` sessions.

Start with [`ChromeForTesting::launch`]. Reach for [`ChromeForTestingManager`] when you need finer
control over the resolve / download / launch steps.
"
)]
//!
//! ## Cancellation and drop safety
//!
//! Cancellation is opt-in. Pass a [`CancellationToken`] through
//! [`ChromeForTestingConfig::builder`], the session builder's `with_cancellation` step (feature
//! `thirtyfour`), or the lower-level [`ChromeForTestingManager`] methods to cancel work
//! cooperatively. Cancelled work is rolled back before [`ChromeForTestingError::Cancelled`] is
//! returned: an in-flight installation stops and removes its staging directory, a starting process
//! is terminated, and a `WebDriver` handshake in flight is completed and the new session closed.
//!
//! Dropping a future instead of cancelling it is handled as well, but less observably:
//!
//! - An installation is cancelled. Its extraction stops at the next chunk, and its staging
//!   directory is removed by the next installation of that artifact.
//! - A process that is starting up, and every managed process when its handle is dropped, is
//!   terminated synchronously, briefly blocking a runtime worker.
//! - A `WebDriver` session run hands its cleanup (quitting the session, terminating a Chrome
//!   Headless Shell) to the Tokio runtime. A session whose handshake was cut off cannot be closed;
//!   `ChromeDriver` ends it when it terminates.
//!
//! None of this survives the Tokio runtime shutting down or the process being killed. Prefer
//! explicit cancellation followed by awaiting the operation, and call [`ChromeForTesting::shutdown`]
//! for observable graceful shutdown.

mod artifact_store;
mod browser;
mod cache;
pub(crate) mod chromedriver;
mod error;
pub(crate) mod facade;
pub(crate) mod manager;
mod policy;
pub(crate) mod port;
pub(crate) mod process_support;
#[cfg(feature = "thirtyfour")]
pub(crate) mod session;
#[cfg(test)]
mod test_support;
pub(crate) mod version;

pub use ::chrome_for_testing::Channel;
pub use ::chrome_for_testing::Platform;
pub use ::chrome_for_testing::Version;
pub use browser::{BrowserArtifactRequest, ChromeBinary, LoadedBrowserPackage};
pub use cache::CachePruneResult;
pub use chromedriver::ChromeDriverConfig;
pub use chromedriver::output::{
    DriverOutputLine, DriverOutputSource, DriverOutputSubscription, DriverOutputSubscriptionError,
};
pub use chromedriver::process::ChromeDriverProcess;
pub use error::{ChromeForTestingArtifact, ChromeForTestingError, HttpClientPurpose, Result};
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

/// Await an operation that is safe to abandon, giving cancellation deterministic precedence.
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
