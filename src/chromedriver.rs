use crate::ChromeForTestingManagerError;
use crate::mgr::{ChromeForTestingManager, LoadedChromePackage};
use crate::output::{DriverOutputInspectors, DriverOutputListener};
use crate::port::{Port, PortRequest};
use crate::version::VersionRequest;
use chrome_for_testing::Channel;
use rootcause::prelude::ResultExt;
#[cfg(feature = "thirtyfour")]
use rootcause::{IntoReportCollection, markers::SendSync};
use rootcause::{Report, report};
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;
use tokio_process_tools::{
    BroadcastOutputStream, ReliableDelivery, ReplayEnabled, TerminateOnDrop,
};
use typed_builder::TypedBuilder;

/// Timeouts used when terminating the spawned chromedriver process.
///
/// `interrupt` is the grace period the process gets to exit after receiving SIGINT (or the
/// platform equivalent); `terminate` is the additional grace period after escalating to SIGTERM.
/// Both default to 3 seconds.
///
/// ```
/// # use chrome_for_testing_manager::TerminationTimeouts;
/// # use std::time::Duration;
/// let timeouts = TerminationTimeouts::builder()
///     .interrupt(Duration::from_secs(5))
///     .terminate(Duration::from_secs(5))
///     .build();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct TerminationTimeouts {
    /// Grace period after the initial interrupt signal. Defaults to 3 seconds.
    #[builder(default = Duration::from_secs(3))]
    pub interrupt: Duration,

    /// Grace period after escalating to terminate. Defaults to 3 seconds.
    #[builder(default = Duration::from_secs(3))]
    pub terminate: Duration,
}

impl Default for TerminationTimeouts {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// Configuration used when running a `ChromeDriver` process.
///
/// Construct via [`Self::builder`] or [`Self::default`]. Defaults: latest stable Chrome,
/// OS-assigned port, no output listener, default cache directory, 3 s / 3 s termination
/// timeouts.
///
/// ```no_run
/// # use chrome_for_testing_manager::{Channel, ChromedriverRunConfig, DriverOutputListener,
/// #     TerminationTimeouts};
/// # use std::time::Duration;
/// let config = ChromedriverRunConfig::builder()
///     .version(Channel::Stable)            // accepts Channel, Version, or VersionRequest
///     .port(8080u16)                        // accepts u16, Port, or PortRequest
///     .output_listener(DriverOutputListener::new(|line| println!("{line:?}")))
///     .termination_timeouts(
///         TerminationTimeouts::builder()
///             .interrupt(Duration::from_secs(5))
///             .terminate(Duration::from_secs(5))
///             .build(),
///     )
///     .build();
/// ```
#[derive(Debug, Clone, TypedBuilder)]
pub struct ChromedriverRunConfig {
    /// The requested `ChromeDriver` version.
    ///
    /// Accepts anything implementing `Into<VersionRequest>`, including [`Channel`] and
    /// [`crate::Version`].
    #[builder(default = VersionRequest::LatestIn(Channel::Stable), setter(into))]
    pub version: VersionRequest,

    /// The requested `ChromeDriver` port.
    ///
    /// Accepts anything implementing `Into<PortRequest>`, including a bare `u16` and [`Port`].
    #[builder(default = PortRequest::Any, setter(into))]
    pub port: PortRequest,

    /// Optional callback for browser-driver process output lines.
    #[builder(default, setter(strip_option(fallback = output_listener_opt)))]
    pub output_listener: Option<DriverOutputListener>,

    /// Optional override for the cache directory holding downloaded chrome / chromedriver
    /// artifacts. Defaults to the platform's per-user cache directory.
    #[builder(default, setter(strip_option(fallback = cache_dir_opt)))]
    pub cache_dir: Option<PathBuf>,

    /// Timeouts applied when the [`Chromedriver`] handle is dropped or
    /// [`Chromedriver::terminate`] is called.
    #[builder(default)]
    pub termination_timeouts: TerminationTimeouts,
}

impl Default for ChromedriverRunConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// A handle to a spawned chromedriver process plus its resolved Chrome / `ChromeDriver` binaries.
///
/// Terminates automatically when dropped, using the timeouts configured via
/// [`ChromedriverRunConfig::termination_timeouts`]. The on-drop automation keeps tests safe in the
/// face of panics. Call [`Self::terminate`] to drive the same shutdown explicitly and surface any
/// error.
///
/// Drive `WebDriver` sessions through [`Self::with_session`] / [`Self::with_custom_session`].
/// Sessions are independent, so multiple of them can run concurrently against the same chromedriver
/// via `tokio::join!` or `tokio::spawn` on a multi-thread runtime.
pub struct Chromedriver {
    /// The manager instance used to resolve a version, download it and starting the chromedriver.
    mgr: ChromeForTestingManager,

    /// Chrome and chromedriver binaries used for testing.
    loaded: LoadedChromePackage,

    /// The running chromedriver process. Terminated when dropped.
    ///
    /// Always stores a process handle. The value is only taken out on termination,
    /// notifying our `Drop` impl that the process was gracefully terminated when seeing `None`.
    process: Option<TerminateOnDrop<BroadcastOutputStream<ReliableDelivery, ReplayEnabled>>>,

    /// Long-lived browser-driver output inspectors.
    output_inspectors: Option<DriverOutputInspectors>,

    /// The port the chromedriver process listens on.
    port: Port,

    /// Timeouts to use when terminating, including on drop.
    termination_timeouts: TerminationTimeouts,
}

impl Debug for Chromedriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chromedriver")
            .field("mgr", &self.mgr)
            .field("loaded", &self.loaded)
            .field("process", &self.process)
            .field("output_inspectors", &self.output_inspectors)
            .field("port", &self.port)
            .field("termination_timeouts", &self.termination_timeouts)
            .finish()
    }
}

impl Chromedriver {
    /// Convenience: resolve, download, and launch chromedriver using
    /// [`ChromedriverRunConfig::default`]. Equivalent to
    /// `Chromedriver::run(ChromedriverRunConfig::default()).await`.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime is not multithreaded, version resolution fails,
    /// the download fails, or the chromedriver process cannot be spawned.
    pub async fn run_default() -> Result<Chromedriver, Report<ChromeForTestingManagerError>> {
        Self::run(ChromedriverRunConfig::default()).await
    }

    /// Resolve, download, and launch a chromedriver process.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime is not multithreaded, version resolution fails,
    /// the download fails, or the chromedriver process cannot be spawned.
    pub async fn run(
        config: ChromedriverRunConfig,
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

        let mgr = match config.cache_dir {
            Some(cache_dir) => ChromeForTestingManager::new_with_cache_dir(cache_dir)?,
            None => ChromeForTestingManager::new()?,
        };
        let selected = mgr.resolve_version(config.version).await?;
        let loaded = mgr.download(selected).await?;
        let (process_handle, actual_port, output_inspectors) = mgr
            .launch_chromedriver(&loaded, config.port, config.output_listener)
            .await?;
        let termination_timeouts = config.termination_timeouts;
        Ok(Chromedriver {
            process: Some(process_handle.terminate_on_drop(
                termination_timeouts.interrupt,
                termination_timeouts.terminate,
            )),
            output_inspectors: Some(output_inspectors),
            port: actual_port,
            loaded,
            mgr,
            termination_timeouts,
        })
    }

    /// The port the chromedriver process is listening on.
    ///
    /// When constructed with [`PortRequest::Any`] this reflects the OS-assigned port.
    #[must_use]
    pub fn port(&self) -> Port {
        self.port
    }

    /// Gracefully terminate the chromedriver process with the configured termination timeouts
    /// (defaults to 3s / 3s; configurable via the `termination_timeouts` field of
    /// [`ChromedriverRunConfig`]).
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated within the timeout.
    #[expect(clippy::missing_panics_doc)] // Process handle is always present; only taken here.
    pub async fn terminate(mut self) -> Result<ExitStatus, Report<ChromeForTestingManagerError>> {
        let TerminationTimeouts {
            interrupt,
            terminate,
        } = self.termination_timeouts;
        let _output_inspectors = self.output_inspectors.take();
        self.process
            .take()
            .expect("present")
            .terminate(interrupt, terminate)
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
        setup: impl FnOnce(
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

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;

    #[test]
    fn run_config_defaults_to_latest_stable_on_any_port() {
        let config = ChromedriverRunConfig::builder().build();

        assert_that!(config.version).is_equal_to(VersionRequest::LatestIn(Channel::Stable));
        assert_that!(config.port).is_equal_to(PortRequest::Any);
        assert_that!(config.output_listener).is_none();
    }

    #[test]
    fn run_config_accepts_bare_output_listener() {
        let listener = DriverOutputListener::new(|_line| {});

        let config = ChromedriverRunConfig::builder()
            .output_listener(listener)
            .build();

        assert_that!(config.output_listener).is_some();
    }

    #[test]
    fn run_config_accepts_optional_output_listener() {
        let listener = DriverOutputListener::new(|_line| {});

        let config = ChromedriverRunConfig::builder()
            .output_listener_opt(Some(listener))
            .build();

        assert_that!(config.output_listener).is_some();

        let config = ChromedriverRunConfig::builder()
            .output_listener_opt(None)
            .build();

        assert_that!(config.output_listener).is_none();
    }

    #[test]
    fn builder_port_accepts_u16_via_setter_into() {
        let config = ChromedriverRunConfig::builder().port(8080u16).build();
        assert_that!(config.port).is_equal_to(PortRequest::Specific(Port(8080)));
    }

    #[test]
    fn builder_version_accepts_channel_via_setter_into() {
        let config = ChromedriverRunConfig::builder()
            .version(Channel::Beta)
            .build();
        assert_that!(config.version).is_equal_to(VersionRequest::LatestIn(Channel::Beta));
    }

    #[test]
    fn builder_accepts_cache_dir_and_termination_timeouts() {
        let timeouts = TerminationTimeouts::builder()
            .interrupt(Duration::from_secs(1))
            .terminate(Duration::from_secs(2))
            .build();
        let config = ChromedriverRunConfig::builder()
            .cache_dir(PathBuf::from("/tmp/cft-cache"))
            .termination_timeouts(timeouts)
            .build();

        assert_that!(config.cache_dir).is_equal_to(Some(PathBuf::from("/tmp/cft-cache")));
        assert_that!(config.termination_timeouts).is_equal_to(timeouts);
    }

    #[test]
    fn termination_timeouts_builder_uses_three_second_defaults_for_unset_fields() {
        let timeouts = TerminationTimeouts::builder()
            .interrupt(Duration::from_secs(7))
            .build();
        assert_that!(timeouts.interrupt).is_equal_to(Duration::from_secs(7));
        assert_that!(timeouts.terminate).is_equal_to(Duration::from_secs(3));
    }

    #[test]
    fn termination_timeouts_default_is_three_seconds_each() {
        let timeouts = TerminationTimeouts::default();
        assert_that!(timeouts.interrupt).is_equal_to(Duration::from_secs(3));
        assert_that!(timeouts.terminate).is_equal_to(Duration::from_secs(3));
    }
}
