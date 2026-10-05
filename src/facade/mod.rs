//! Recommended managed Chrome for Testing facade.
//!
//! This module composes resolution, installation, driver launch, and optional scoped sessions
//! while keeping lower-level manager and process details out of the default workflow.

mod config;

pub use config::ChromeForTestingConfig;

use crate::Result;
use crate::browser::LoadedBrowserPackage;
use crate::chromedriver::output::DriverOutputSubscription;
use crate::chromedriver::process::ChromeDriverProcess;
use crate::manager::ChromeForTestingManager;
use crate::port::Port;
#[cfg(feature = "thirtyfour")]
use crate::session::SessionBuilder;
use crate::version::SelectedVersion;
use std::path::Path;
use std::process::ExitStatus;

/// A managed Chrome for Testing environment.
///
/// This handle owns the resolved browser package and its matching `ChromeDriver` process. Its
/// process guard attempts termination when dropped inside an active multithreaded Tokio runtime;
/// dropping it on a thread without one (for example after that runtime has shut down) panics
/// instead of silently leaking the child process. Drop is only a fallback; call [`Self::shutdown`]
/// to drive shutdown explicitly and surface any error. No drop guard can guarantee cleanup after
/// abrupt process termination.
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
    doc = r#"
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
"#
)]
#[derive(Debug)]
pub struct ChromeForTesting {
    /// Read only by the feature-gated [`Self::session`]; derived `Debug` does not count as a use.
    #[cfg_attr(not(feature = "thirtyfour"), expect(dead_code))]
    manager: ChromeForTestingManager,
    selected: SelectedVersion,
    loaded: LoadedBrowserPackage,
    driver: ChromeDriverProcess,
}

impl ChromeForTesting {
    #[cfg(all(test, feature = "thirtyfour"))]
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
    /// [crate-level cancellation section](crate#cancellation-and-drop-safety) for the exact
    /// drop-safety guarantee.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime is not multithreaded, resolution or download fails, the
    /// driver cannot be launched, or the operation is cancelled.
    pub async fn launch(config: ChromeForTestingConfig) -> Result<Self> {
        crate::ensure_multithreaded_runtime()?;

        let (version, chrome_binary, cancellation, manager_config, driver_config) =
            config.into_parts();
        let cancellation = cancellation.unwrap_or_default();
        crate::check_cancelled(&cancellation)?;

        let manager = ChromeForTestingManager::new_with_config(manager_config)?;
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
    /// Non-`thirtyfour` `WebDriver` clients connecting through [`Self::driver_port`] must register
    /// this path as the browser binary in their session capabilities.
    #[must_use]
    pub fn browser_executable(&self) -> &Path {
        self.loaded.browser_executable()
    }

    /// Subscribe to future `ChromeDriver` output without backpressuring the child process.
    #[must_use]
    pub fn subscribe_output(&self) -> DriverOutputSubscription {
        self.driver.subscribe_output()
    }

    /// Gracefully shut down the managed Chrome for Testing environment.
    ///
    /// # Errors
    ///
    /// Returns the driver exit status, or an error if the environment cannot be shut down within
    /// the configured policy.
    pub async fn shutdown(self) -> Result<ExitStatus> {
        self.driver.terminate().await
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
