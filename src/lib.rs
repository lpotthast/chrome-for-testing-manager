//! Drive a real Chrome browser from your Rust tests without ever installing Chrome yourself.
//!
//! `chrome-for-testing-manager` is a thin orchestration layer over Google's
//! [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/) release index. It picks the right
//! `chrome` + `chromedriver` pair for your platform, downloads them into a local cache the first time you ask, spawns
//! `chromedriver` on a port of your choosing (or one the OS picks), and hands you a managed
//! [`thirtyfour`](https://docs.rs/thirtyfour) `WebDriver` session. When your test finishes, panics, or is canceled, the
//! spawned process is terminated and the session closed for you.
//!
//! It exists so that browser tests in CI and on developer machines don't depend on whatever Chrome happens to be installed,
//! and so that bumping the Chrome version under test is one simple change.
//!
//! # Why use it
//!
//! - **No global Chrome dependency.** Tests don't care what Chrome happens to be installed on the host. Every developer
//!   and CI runner runs against the browser this library brings along. No more "works on my machine."
//! - **Deterministic upgrades.** Pin to a specific `Version`, follow a `Channel` (Stable / Beta / Dev / Canary), or always
//!   grab the latest. Switching is a one-line change.
//! - **Port and lifecycle managed for you.** Bind to a fixed port for debugging or let the OS pick one for parallel test
//!   isolation. A process guard attempts termination on drop while its multithreaded Tokio runtime is active; use explicit
//!   `shutdown().await` when termination and its result must be observed.
//! - **Ergonomic `thirtyfour` integration.** Run a browser test inside `session().run(|s| ...)` where the `WebDriver`
//!   session is created, scoped, and torn down automatically. Optional `.with_caps(...)`, `.with_config(...)`, and
//!   `.with_cancellation(...)` builder steps let you tweak Chrome capabilities, the `WebDriver` client, or cancellation
//!   without leaving the chain. `thirtyfour` is the default session provider; disabling it keeps lower-level version,
//!   cache, download, and process management while removing session APIs.
//! - **Observable.** Call `subscribe_output()` for a bounded, non-blocking subscription streaming `chromedriver`
//!   stdout/stderr lines into your own logging or fixtures, `recent_output()` for the last lines printed since spawn, or
//!   `subscribe_output_with_history()` for both without missing or duplicating a line. Startup failures carry that recent
//!   output in their error report.
//!
//! # Installation
//!
//! ```toml
//! [dependencies]
//! chrome-for-testing-manager = "0.13"
//! rootcause = "0.13"
//! thirtyfour = "0.37"
//!
//! # Additional dependencies for the example below.
//! assertr = "0.7"
//! tokio = { version = "1", features = ["full"] }
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! # #[cfg(feature = "thirtyfour")]
//! # mod example {
//! use assertr::prelude::*;
//! use chrome_for_testing_manager::ChromeForTesting;
//! use rootcause::Report;
//! use std::time::Duration;
//! use thirtyfour::prelude::*;
//!
//! // This library requires being used in a multithreaded runtime.
//! // If you want to run a test, use: `#[tokio::test(flavor = "multi_thread")]`.
//! #[tokio::main]
//! async fn main() -> Result<(), Report> {
//!     let chrome = ChromeForTesting::launch(Default::default()).await?;
//!     let session_result = chrome
//!         .session()
//!         .run(async |session| {
//!             session.goto(concat!(
//!                 "data:text/html,",
//!                 "<title>Local fixture</title>",
//!                 "<form id='search-form'><input id='searchInput'>",
//!                 "<button type='button' onclick=\"document.title='Selenium'\">Search</button></form>",
//!                 "<h1 id='firstHeading'>Selenium</h1>"
//!             )).await?;
//!
//!             let search_form = session.find(By::Id("search-form")).await?;
//!             let search_input = search_form.find(By::Id("searchInput")).await?;
//!             search_input.send_keys("selenium").await?;
//!
//!             let submit_btn = search_form.find(By::Css("button[type='button']")).await?;
//!             submit_btn.click().await?;
//!
//!             // Look for header to implicitly wait for the page to load.
//!             let _heading = session
//!                 .query(By::Id("firstHeading"))
//!                 .wait(Duration::from_secs(2), Duration::from_millis(100))
//!                 .exists()
//!                 .await?;
//!             assert_that!(session.title().await?).is_equal_to("Selenium");
//!
//!             Ok::<(), WebDriverError>(())
//!         }).await;
//!     let shutdown_result = chrome.shutdown().await;
//!
//!     session_result?;
//!     shutdown_result?;
//!     Ok(())
//! }
//! # }
//! # fn main() {}
//! ```
//!
//! Cancellation is opt-in: pass a `CancellationToken` (re-exported from `tokio-util`) through the config's
//! `.cancellation(...)` setter or `SessionBuilder::with_cancellation(...)`. Cancelled work rolls back cooperatively
//! (process startup is terminated, an in-flight cache transaction removes its staging, a session callback is dropped and
//! the session closed) and is reported as `ChromeForTestingError::Cancelled` after cleanup has been drained. What
//! happens when a future is dropped instead is documented in "Cancellation and drop safety" below. Use
//! `ChromeForTesting::shutdown().await` when graceful shutdown and its result matter.
//!
//! # Configuration
//!
//! Anything beyond defaults goes through `ChromeForTestingConfig::builder()`. The `version` setter accepts a `Channel`,
//! a specific `Version`, or a `VersionRequest`. Shared process and session behavior belongs to `LifecyclePolicy`;
//! technical driver settings are grouped in `ChromeDriverConfig`, whose `port` setter accepts a `u16`, a `Port`, or a
//! `PortRequest`. Passing `0u16` requests an OS-assigned port.
//!
//! ```rust,no_run
//! use chrome_for_testing_manager::{
//!     Channel, ChromeDriverConfig, ChromeForTesting, ChromeForTestingConfig,
//!     DriverOutputSubscriptionError, GracefulShutdown, LifecyclePolicy, NetworkPolicy,
//! };
//! use std::time::Duration;
//!
//! async fn run() -> Result<(), rootcause::Report<chrome_for_testing_manager::ChromeForTestingError>> {
//!     let config = ChromeForTestingConfig::builder()
//!         .version(Channel::Beta)
//!         .cache_dir("target/chrome-cache")
//!         .network(
//!             NetworkPolicy::builder()
//!                 .artifact_download_timeout(Duration::from_secs(10 * 60))
//!                 .build(),
//!         )
//!         .lifecycle(
//!             LifecyclePolicy::builder()
//!                 .graceful_shutdown(
//!                     GracefulShutdown::builder()
//!                         .unix_sigterm(Duration::from_secs(5))
//!                         .windows_ctrl_break(Duration::from_secs(5))
//!                         .build(),
//!                 )
//!                 .build(),
//!         )
//!         .driver(ChromeDriverConfig::builder().port(3000u16).build())
//!         .build();
//!     let chrome = ChromeForTesting::launch(config).await?;
//!
//!     let mut driver_output = chrome.subscribe_output();
//!     tokio::spawn(async move {
//!         loop {
//!             match driver_output.recv().await {
//!                 Ok(line) => println!("{line}"),
//!                 // This subscriber fell behind and missed some lines; it can keep receiving.
//!                 Err(DriverOutputSubscriptionError::Lagged { skipped }) => {
//!                     eprintln!("missed {skipped} chromedriver output lines");
//!                 }
//!                 // The driver's output has ended.
//!                 Err(_) => break,
//!             }
//!         }
//!     });
//!
//!     chrome.shutdown().await?;
//!     Ok(())
//! }
//! ```
//!
//! `GracefulShutdown` (re-exported from `tokio-process-tools`) mirrors the platform's actual graceful-shutdown model:
//!
//! - on Unix, it carries one or more phases (`SIGTERM` and/or `SIGINT`).
//! - on Windows, it carries a single `CTRL_BREAK_EVENT` budget.
//!
//! # Managed sessions opt-out
//!
//! The `session()` builder providing a `thirtyfour` session, called in the example, is only available because
//! `chrome-for-testing-manager` enables its `thirtyfour` feature by default.
//!
//! If you only want its chrome/chromedriver version resolution, download, and launch orchestration, declare the dependency
//! as
//!
//! ```toml
//! chrome-for-testing-manager = { version = "0.13", default-features = false }
//! ```
//!
//! instead.
//!
//! # Going lower-level
//!
//! For most users `ChromeForTesting` is the right entry point. Non-`thirtyfour` `WebDriver` clients can connect through
//! `driver_port()`. With regular Chrome, they register `browser_executable()` as the browser binary in their session
//! capabilities. With Chrome Headless Shell, they launch `browser_executable()` with `--remote-debugging-port` themselves
//! and attach through `goog:chromeOptions.debuggerAddress`, as the managed `thirtyfour` sessions do. If you need finer
//! control, such as pre-warming the cache without spawning chromedriver or running multiple chromedriver instances off a
//! single download, reach for `ChromeForTestingManager` directly.
//! It exposes `resolve_version`, `download` / `download_for`, `launch_driver`, and `prepare_caps` as separate steps.
//! Resolution takes a non-empty `BrowserArtifactRequest`, so `Latest` is selected only from releases that actually contain
//! `ChromeDriver` and every requested browser artifact. `download` installs exactly that resolved set, while `download_for`
//! installs and returns only the `LoadedBrowserPackage` for one specific binary. Each returned `LoadedBrowserPackage` records the selected
//! binary and both paths while privately retaining a cache lease.
//! `launch_driver` consumes a `ChromeDriverConfig` and returns `ChromeDriverProcess`, which exposes `port()`, a bounded
//! non-blocking `subscribe_output()`, `recent_output()`, `subscribe_output_with_history()`, and consuming `terminate()`
//! without exposing the generic process implementation.
//!
//! Installations are cross-process coordinated through typed shared-cache and exclusive-artifact lock guards. Each
//! artifact is downloaded and validated in a unique same-filesystem staging directory, marked with the executable size,
//! and atomically renamed into place. Loaded packages and launched driver processes retain a shared
//! cache lease, so `clear_cache()` and `prune_cache(...)` return the typed `CacheInUse` error instead of deleting active
//! artifacts. `prune_cache(...)` removes unretained version directories while preserving the lock namespace and unknown
//! owner files.
//!
//! The cache contents live in a layout-versioned directory beneath the cache root, so releases with incompatible on-disk
//! layouts can share one cache root without replacing each other's packages. Releases before 0.13 stored versions
//! directly in the cache root. Neither `clear_cache()` nor `prune_cache(...)` touches those, so delete them manually once
//! no older release uses them.
//!
//! # Cancellation and drop safety
//!
//! Cancellation is opt-in. Pass a [`CancellationToken`] through [`ChromeForTestingConfig::builder`],
//! the session builder's `with_cancellation` step (feature `thirtyfour`), or the lower-level
//! [`ChromeForTestingManager`] methods to cancel work cooperatively. Cancelled work is rolled back
//! before [`ChromeForTestingError::Cancelled`] is returned: an in-flight installation stops and
//! removes its staging directory, a starting process is terminated, and a `WebDriver` handshake in
//! flight is completed and the new session closed.
//!
//! Dropping a future instead of cancelling it is handled as well, but less observably:
//!
//! - An installation is cancelled and rolls back in the background: its extraction stops at the next
//!   chunk, and its staging directory is removed before its cache locks are released. A failed
//!   rollback is logged.
//! - A process that is starting up, and every managed process when its handle is dropped, is
//!   terminated synchronously, briefly blocking a runtime worker.
//! - A `WebDriver` session run hands its cleanup (quitting the session, terminating a Chrome Headless
//!   Shell) to the Tokio runtime, and [`ChromeForTesting::shutdown`] waits for it and reports its
//!   failure. A session whose
//!   handshake was cut off cannot be closed; `ChromeDriver` ends it when it terminates.
//!
//! None of this survives the Tokio runtime shutting down or the process being killed. Prefer explicit
//! cancellation followed by awaiting the operation, and call [`ChromeForTesting::shutdown`] for
//! observable graceful shutdown.

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
pub use chromedriver::output::{
    DriverOutputLine, DriverOutputSource, DriverOutputSubscription, DriverOutputSubscriptionError,
};
pub use chromedriver::process::ChromeDriverProcess;
pub use chromedriver::{ChromeDriverConfig, ChromeDriverLogLevel};
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

/// Return [`ChromeForTestingError::MissingRuntime`] unless a Tokio runtime is active, instead of
/// letting Tokio or `reqwest` panic.
pub(crate) fn ensure_runtime() -> Result<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current()
        .map_err(|_| report!(ChromeForTestingError::MissingRuntime))
}

pub(crate) fn ensure_multithreaded_runtime() -> Result<()> {
    match ensure_runtime()?.runtime_flavor() {
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
