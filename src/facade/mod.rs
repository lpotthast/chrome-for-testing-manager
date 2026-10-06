//! Recommended managed Chrome for Testing facade.
//!
//! This module composes resolution, installation, driver launch, and optional scoped sessions
//! while keeping lower-level manager and process details out of the default workflow.

mod config;

pub use config::ChromeForTestingConfig;

use crate::Result;
use crate::browser::LoadedBrowserPackage;
use crate::chromedriver::output::{DriverOutputLine, DriverOutputSubscription};
use crate::chromedriver::process::ChromeDriverProcess;
use crate::manager::ChromeForTestingManager;
use crate::manager::config::ChromeForTestingManagerConfig;
use crate::port::Port;
#[cfg(feature = "thirtyfour")]
use crate::session::SessionBuilder;
use crate::version::SelectedVersion;
use std::path::Path;
use std::process::ExitStatus;

/// A managed Chrome for Testing environment.
///
/// This handle owns the resolved browser package and its matching `ChromeDriver` process. Dropping
/// it terminates `ChromeDriver` gracefully in the background of the current Tokio runtime; without
/// a runtime (for example after it has shut down), the process is killed as a last resort. Drop is
/// only a fallback; call [`Self::shutdown`] to drive shutdown explicitly and surface any error. No
/// drop guard can guarantee cleanup after abrupt process termination.
///
#[cfg_attr(
    feature = "thirtyfour",
    doc = r#"
```no_run
use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;

# async fn run() -> Result<(), Report> {
let chrome = ChromeForTesting::launch(Default::default()).await?;
let session_result = chrome
    .session()
    .run(async |session| {
        session.goto("data:text/html,<title>Local fixture</title>").await?;
        Ok::<(), thirtyfour::error::WebDriverError>(())
    })
    .await;
let shutdown_result = chrome.shutdown().await;
session_result?;
shutdown_result?;
# Ok(())
# }
```
"#
)]
#[cfg_attr(
    not(feature = "thirtyfour"),
    doc = r"
```no_run
use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;

# async fn run() -> Result<(), Report> {
let chrome = ChromeForTesting::launch(Default::default()).await?;
// Point any WebDriver client at the managed driver and cached browser binary:
let port = chrome.driver_port();
let browser_executable = chrome.browser_executable().to_owned();
chrome.shutdown().await?;
# Ok(())
# }
```
"
)]
#[derive(Debug)]
pub struct ChromeForTesting {
    manager: ChromeForTestingManager,
    selected: SelectedVersion,
    loaded: LoadedBrowserPackage,
    driver: ChromeDriverProcess,
}

impl ChromeForTesting {
    #[cfg(all(test, unix, feature = "thirtyfour"))]
    pub(crate) fn from_test_parts(
        manager: ChromeForTestingManager,
        selected: SelectedVersion,
        loaded: LoadedBrowserPackage,
        driver: ChromeDriverProcess,
    ) -> Self {
        Self {
            manager,
            selected,
            loaded,
            driver,
        }
    }

    /// Resolve, download, and launch a managed Chrome for Testing environment.
    ///
    /// Cancellation is opt-in through the config's `cancellation` token; see the
    /// [crate-level cancellation section](crate#cancellation-and-drop-safety) for what happens
    /// when this future is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ChromeForTestingError::Cancelled`] on cancellation,
    /// [`crate::ChromeForTestingError::MissingRuntime`] outside a Tokio runtime, and
    /// [`crate::ChromeForTestingError::UnsupportedPlatform`] on platforms without Chrome for
    /// Testing builds. Other errors cover preparing the cache
    /// directory and HTTP clients, version resolution, download, and launching `ChromeDriver`.
    pub async fn launch(config: ChromeForTestingConfig) -> Result<Self> {
        crate::ensure_runtime()?;

        let ChromeForTestingConfig {
            version,
            chrome_binary,
            cache_dir,
            cancellation,
            network,
            lifecycle,
            driver: driver_config,
        } = config;
        let cancellation = cancellation.unwrap_or_default();
        crate::check_cancelled(&cancellation)?;

        let manager = ChromeForTestingManager::new_with_config(ChromeForTestingManagerConfig {
            cache_dir,
            network,
            lifecycle,
        })?;
        let selected = manager
            .resolve_version(version, chrome_binary.into(), cancellation.clone())
            .await?;
        let loaded = manager
            .download_for(&selected, chrome_binary, cancellation.clone())
            .await?;
        let driver = manager
            .launch_driver(&loaded, driver_config, cancellation)
            .await?;

        Ok(Self {
            manager,
            selected,
            loaded,
            driver,
        })
    }

    /// Return the release that version resolution selected for this environment.
    ///
    /// Exposes the concrete version, channel, and platform behind requests such as
    /// [`crate::VersionRequest::Latest`].
    #[must_use]
    pub const fn selected_version(&self) -> &SelectedVersion {
        &self.selected
    }

    /// Return the port on which the managed `ChromeDriver` is listening.
    ///
    /// When configured with [`crate::PortRequest::Any`], this is the OS-assigned port.
    #[must_use]
    pub fn driver_port(&self) -> Port {
        self.driver.port()
    }

    /// Return the cached browser executable backing this environment.
    ///
    /// Non-`thirtyfour` `WebDriver` clients connecting through [`Self::driver_port`] use it as
    /// follows:
    ///
    /// - [`ChromeBinary::Chrome`](crate::ChromeBinary::Chrome): register this path as the browser
    ///   binary (`goog:chromeOptions.binary`) in the session capabilities.
    /// - [`ChromeBinary::ChromeHeadlessShell`](crate::ChromeBinary::ChromeHeadlessShell): launch
    ///   this executable with `--remote-debugging-port`, then attach `ChromeDriver` to it through
    ///   `goog:chromeOptions.debuggerAddress`. This is what the managed `thirtyfour` sessions do.
    #[must_use]
    pub fn browser_executable(&self) -> &Path {
        self.loaded.browser_executable()
    }

    /// Subscribe to future `ChromeDriver` output without backpressuring the child process.
    ///
    /// Use [`Self::recent_output`] for lines printed before subscribing.
    #[must_use]
    pub fn subscribe_output(&self) -> DriverOutputSubscription {
        self.driver.subscribe_output()
    }

    /// Return the most recent `ChromeDriver` output lines (up to 256, from spawn on), oldest
    /// first.
    ///
    /// Use [`Self::subscribe_output_with_history`] to combine history and subscription without
    /// losing or duplicating lines printed in between.
    #[must_use]
    pub fn recent_output(&self) -> Vec<DriverOutputLine> {
        self.driver.recent_output()
    }

    /// Return the most recent `ChromeDriver` output lines (up to 256, oldest first) together with
    /// a subscription to every line printed afterwards.
    ///
    /// Every line is either part of the returned history or delivered to the subscription, never
    /// both.
    #[must_use]
    pub fn subscribe_output_with_history(
        &self,
    ) -> (Vec<DriverOutputLine>, DriverOutputSubscription) {
        self.driver.subscribe_output_with_history()
    }

    /// Gracefully shut down the managed Chrome for Testing environment and return the driver's
    /// exit status.
    ///
    /// Cleanups that dropped operations handed to the runtime (e.g. quitting the sessions of
    /// dropped session runs and terminating their Chrome Headless Shells) are awaited first, while
    /// `ChromeDriver` still runs; see
    /// [`ChromeForTestingManager::wait_for_background_tasks`]. Each of them is bounded by the
    /// configured [`crate::LifecyclePolicy`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::ChromeForTestingError::TerminateProcess`] if `ChromeDriver` cannot be
    /// terminated, and [`crate::ChromeForTestingError::BackgroundCleanup`] if a background cleanup
    /// failed. A termination failure remains primary when both occur.
    pub async fn shutdown(self) -> Result<ExitStatus> {
        let background_result = self.manager.wait_for_background_tasks().await;
        let terminate_result = self.driver.terminate().await;
        crate::error::operation_result_with_cleanup(terminate_result, background_result)
    }

    /// Start building a scoped `thirtyfour` session against this environment.
    ///
    /// Call [`SessionBuilder::run`] to open the session, execute the user closure, and clean up the
    /// session regardless of success, error, cancellation, or panic.
    #[cfg(feature = "thirtyfour")]
    #[must_use]
    pub fn session(&self) -> SessionBuilder<'_> {
        SessionBuilder::new(&self.manager, &self.loaded, self.driver.port())
    }
}
