use crate::ChromeForTestingManagerError;
use crate::mgr::{ChromeForTestingManager, LoadedChromePackage, VersionRequest};
use crate::port::{Port, PortRequest};
use chrome_for_testing::Channel;
use rootcause::prelude::ResultExt;
#[cfg(feature = "thirtyfour")]
use rootcause::{IntoReportCollection, markers::SendSync};
use rootcause::{Report, report};
use std::fmt::{Debug, Formatter};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;
use tokio_process_tools::TerminateOnDrop;
use tokio_process_tools::broadcast::BroadcastOutputStream;

/// A wrapper struct for a spawned chromedriver process.
/// Keep this alive until your test is complete.
///
/// Automatically terminates the spawned chromedriver process when dropped.
///
/// You can always manually call `terminate`, but the on-drop automation makes it much safer in
/// quickly panicking contexts, such as tests.
pub struct Chromedriver {
    /// The manager instance used to resolve a version, download it and starting the chromedriver.
    mgr: ChromeForTestingManager,

    /// Chrome and chromedriver binaries used for testing.
    loaded: LoadedChromePackage,

    /// The running chromedriver process. Terminated when dropped.
    ///
    /// Always stores a process handle. The value is only taken out on termination,
    /// notifying our `Drop` impl that the process was gracefully terminated when seeing `None`.
    process: Option<TerminateOnDrop<BroadcastOutputStream>>,

    /// The port the chromedriver process listens on.
    port: Port,
}

impl Debug for Chromedriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chromedriver")
            .field("mgr", &self.mgr)
            .field("loaded", &self.loaded)
            .field("process", &self.process)
            .field("port", &self.port)
            .finish()
    }
}

impl Chromedriver {
    /// Resolve, download, and launch a chromedriver process.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime is not multithreaded, version resolution fails,
    /// the download fails, or the chromedriver process cannot be spawned.
    pub async fn run(
        version: VersionRequest,
        port: PortRequest,
    ) -> Result<Chromedriver, Report<ChromeForTestingManagerError>> {
        // Assert that async-drop will work.
        // This is the only way of constructing a `Chromedriver` instance,
        // so it's safe to do this here.
        match tokio::runtime::Handle::current().runtime_flavor() {
            RuntimeFlavor::MultiThread => { /* we want this */ }
            unsupported_flavor => {
                return Err(report!(ChromeForTestingManagerError::UnsupportedRuntime {
                    runtime_flavor: unsupported_flavor,
                }));
            }
        }

        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(version).await?;
        let loaded = mgr.download(selected).await?;
        let (process_handle, actual_port) = mgr.launch_chromedriver(&loaded, port).await?;
        Ok(Chromedriver {
            process: Some(
                process_handle.terminate_on_drop(Duration::from_secs(3), Duration::from_secs(3)),
            ),
            port: actual_port,
            loaded,
            mgr,
        })
    }

    /// Shortcut for [`Self::run`] with the latest stable channel version on any port.
    ///
    /// # Errors
    ///
    /// See [`Self::run`].
    pub async fn run_latest_stable() -> Result<Chromedriver, Report<ChromeForTestingManagerError>> {
        Self::run(VersionRequest::LatestIn(Channel::Stable), PortRequest::Any).await
    }

    /// Shortcut for [`Self::run`] with the latest beta channel version on any port.
    ///
    /// # Errors
    ///
    /// See [`Self::run`].
    pub async fn run_latest_beta() -> Result<Chromedriver, Report<ChromeForTestingManagerError>> {
        Self::run(VersionRequest::LatestIn(Channel::Beta), PortRequest::Any).await
    }

    /// Shortcut for [`Self::run`] with the latest dev channel version on any port.
    ///
    /// # Errors
    ///
    /// See [`Self::run`].
    pub async fn run_latest_dev() -> Result<Chromedriver, Report<ChromeForTestingManagerError>> {
        Self::run(VersionRequest::LatestIn(Channel::Dev), PortRequest::Any).await
    }

    /// Shortcut for [`Self::run`] with the latest canary channel version on any port.
    ///
    /// # Errors
    ///
    /// See [`Self::run`].
    pub async fn run_latest_canary() -> Result<Chromedriver, Report<ChromeForTestingManagerError>> {
        Self::run(VersionRequest::LatestIn(Channel::Canary), PortRequest::Any).await
    }

    /// Gracefully terminate the chromedriver process with default timeouts (3s each).
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated within the timeout.
    pub async fn terminate(self) -> Result<ExitStatus, Report<ChromeForTestingManagerError>> {
        self.terminate_with_timeouts(Duration::from_secs(3), Duration::from_secs(3))
            .await
    }

    /// Gracefully terminate the chromedriver process with custom timeouts.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated within the given timeouts.
    #[expect(clippy::missing_panics_doc)] // Process handle is always present; only taken here.
    pub async fn terminate_with_timeouts(
        mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, Report<ChromeForTestingManagerError>> {
        self.process
            .take()
            .expect("present")
            .terminate(interrupt_timeout, terminate_timeout)
            .await
            .context(ChromeForTestingManagerError::TerminateChromedriver { port: self.port })
    }

    /// Execute an async closure with a `WebDriver` session.
    /// The session will be automatically cleaned up after the closure completes.
    ///
    /// # Errors
    ///
    /// Returns an error if session creation fails or the user closure returns an error.
    #[cfg(feature = "thirtyfour")]
    pub async fn with_session<T, E, F>(
        &self,
        f: F,
    ) -> Result<T, Report<ChromeForTestingManagerError>>
    where
        F: for<'a> AsyncFnOnce(&'a crate::session::Session) -> Result<T, E>,
        E: IntoReportCollection<SendSync>,
    {
        self.with_custom_session(|_caps| Ok(()), f).await
    }

    /// Execute an async closure with a custom-configured `WebDriver` session.
    /// The session will be automatically cleaned up after the closure completes.
    ///
    /// # Errors
    ///
    /// Returns an error if capability setup, session creation, or the user closure fails.
    #[cfg(feature = "thirtyfour")]
    pub async fn with_custom_session<T, E, F>(
        &self,
        setup: impl Fn(
            &mut thirtyfour::ChromeCapabilities,
        ) -> Result<(), thirtyfour::prelude::WebDriverError>,
        f: F,
    ) -> Result<T, Report<ChromeForTestingManagerError>>
    where
        F: for<'a> AsyncFnOnce(&'a crate::session::Session) -> Result<T, E>,
        E: IntoReportCollection<SendSync>,
    {
        use crate::session::Session;
        use futures::FutureExt;

        let mut caps = self.mgr.prepare_caps(&self.loaded)?;
        setup(&mut caps).context(ChromeForTestingManagerError::ConfigureSessionCapabilities)?;
        let driver = thirtyfour::WebDriver::new(format!("http://localhost:{}", self.port), caps)
            .await
            .context(ChromeForTestingManagerError::StartWebDriverSession { port: self.port })?;

        let session = Session { driver };

        // Execute the user function.
        let maybe_panicked = core::panic::AssertUnwindSafe(f(&session))
            .catch_unwind()
            .await;

        let user_result = match maybe_panicked {
            Ok(result) => result.context(ChromeForTestingManagerError::RunSessionCallback),
            Err(payload) => {
                if let Err(quit_err) = session.quit().await {
                    tracing::error!(
                        "Failed to quit WebDriver session after user callback panic: {quit_err:?}"
                    );
                }
                std::panic::resume_unwind(payload);
            }
        };

        // No matter what happened, clean up the session.
        let quit_result = session.quit().await;

        match (user_result, quit_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(quit_err)) => Err(quit_err),
            (Err(user_err), Ok(())) => Err(user_err),
            (Err(mut user_err), Err(quit_err)) => {
                tracing::error!(
                    "Failed to quit WebDriver session after user failure: {quit_err:?}"
                );
                user_err
                    .children_mut()
                    .push(quit_err.into_dynamic().into_cloneable());
                Err(user_err)
            }
        }
    }
}
