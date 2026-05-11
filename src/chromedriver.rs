use crate::ChromeForTestingManagerError;
use crate::mgr::{ChromeForTestingManager, LoadedChromePackage};
use crate::output::{DriverOutputInspectors, DriverOutputListener};
use crate::port::{Port, PortRequest};
#[cfg(feature = "thirtyfour")]
use crate::session_builder::{DefaultCaps, DefaultConfig, SessionBuilder};
use crate::version::VersionRequest;
use chrome_for_testing::Channel;
use rootcause::prelude::ResultExt;
use rootcause::{Report, report};
use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;
use tokio_process_tools::{
    BroadcastOutputStream, GracefulShutdown, ReliableWithBackpressure, ReplayEnabled,
    TerminateOnDrop,
};
use typed_builder::TypedBuilder;

/// Default per-platform graceful-shutdown budget used when terminating the spawned `chromedriver`
/// process: 3 s `SIGTERM` on Unix (then `SIGKILL`) and 3 s `CTRL_BREAK_EVENT` on Windows (then
/// `TerminateProcess`).
#[must_use]
pub(crate) fn default_graceful_shutdown() -> GracefulShutdown {
    let timeout = Duration::from_secs(3);
    GracefulShutdown::builder()
        .unix_sigterm(timeout)
        .windows_ctrl_break(timeout)
        .build()
}

/// Configuration used when running a `ChromeDriver` process.
///
/// Construct via [`Self::builder`] or [`Self::default`]. Defaults: latest stable Chrome,
/// OS-assigned port, no output listener, default cache directory, 3s graceful termination budget
/// on all systems.
///
/// ```no_run
/// # use chrome_for_testing_manager::{Channel, ChromedriverRunConfig, DriverOutputListener, GracefulShutdown};
/// # use std::time::Duration;
/// let config = ChromedriverRunConfig::builder()
///     .version(Channel::Stable)            // Accepts Channel, Version, or VersionRequest.
///     .port(8080u16)                       // Accepts u16, Port, or PortRequest.
///     .output_listener(DriverOutputListener::new(|line| println!("{line:?}")))
///     .graceful_shutdown(
///         GracefulShutdown::builder()
///             .unix_sigterm(Duration::from_secs(3))
///             .windows_ctrl_break(Duration::from_secs(3))
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

    /// Per-platform graceful-shutdown budget applied when the [`Chromedriver`] handle is dropped
    /// or [`Chromedriver::terminate`] is called.
    #[builder(default = default_graceful_shutdown())]
    pub graceful_shutdown: GracefulShutdown,
}

impl Default for ChromedriverRunConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// A handle to a spawned chromedriver process plus its resolved Chrome / `ChromeDriver` binaries.
///
/// Terminates automatically when dropped, using the budget configured via
/// [`ChromedriverRunConfig::graceful_shutdown`]. The on-drop automation keeps tests safe in the
/// face of panics. Call [`Self::terminate`] to drive the same shutdown explicitly and surface any
/// error.
///
/// Drive browser sessions through [`Self::session`]. Sessions are independent, so multiple of them
/// can run concurrently against the same chromedriver via `tokio::join!` or `tokio::spawn` on a
/// multi-thread runtime.
pub struct Chromedriver {
    /// The manager instance used to resolve a version, download it and starting the chromedriver.
    pub(crate) mgr: ChromeForTestingManager,

    /// Chrome and chromedriver binaries used for testing.
    pub(crate) loaded: LoadedChromePackage,

    /// The running chromedriver process. Terminated when dropped.
    ///
    /// Always stores a process handle. The value is only taken out on termination,
    /// notifying our `Drop` impl that the process was gracefully terminated when seeing `None`.
    process:
        Option<TerminateOnDrop<BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>>>,

    /// Long-lived browser-driver output inspectors.
    output_inspectors: Option<DriverOutputInspectors>,

    /// The port the chromedriver process listens on.
    port: Port,

    /// Graceful-shutdown budget to use when terminating, including on drop.
    graceful_shutdown: GracefulShutdown,
}

impl Debug for Chromedriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chromedriver")
            .field("mgr", &self.mgr)
            .field("loaded", &self.loaded)
            .field("process", &self.process)
            .field("output_inspectors", &self.output_inspectors)
            .field("port", &self.port)
            .field("graceful_shutdown", &self.graceful_shutdown)
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
        let graceful_shutdown = config.graceful_shutdown;
        let (process_handle, actual_port, output_inspectors) = mgr
            .launch_chromedriver(
                &loaded,
                config.port,
                config.output_listener,
                graceful_shutdown.clone(),
            )
            .await?;
        Ok(Chromedriver {
            process: Some(process_handle.terminate_on_drop(graceful_shutdown.clone())),
            output_inspectors: Some(output_inspectors),
            port: actual_port,
            loaded,
            mgr,
            graceful_shutdown,
        })
    }

    /// The port the chromedriver process is listening on.
    ///
    /// When constructed with [`PortRequest::Any`] this reflects the OS-assigned port.
    #[must_use]
    pub fn port(&self) -> Port {
        self.port
    }

    /// Gracefully terminate the chromedriver process with the configured [`GracefulShutdown`],
    /// configurable via the `graceful_shutdown` field of [`ChromedriverRunConfig`].
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated within the configured graceful-shutdown
    /// budget.
    #[expect(clippy::missing_panics_doc)] // Process handle is always present; only taken here.
    pub async fn terminate(mut self) -> Result<ExitStatus, Report<ChromeForTestingManagerError>> {
        let _output_inspectors = self.output_inspectors.take();
        self.process
            .take()
            .expect("present")
            .terminate(self.graceful_shutdown)
            .await
            .context(ChromeForTestingManagerError::TerminateChromedriver { port: self.port })
    }

    /// Start building a scoped `thirtyfour` [`crate::Session`] against this chromedriver.
    ///
    /// This is the primary entry point for running a browser test. The returned
    /// [`SessionBuilder`] is a fluent, chainable builder with three steps:
    ///
    /// 1. (optional) [`SessionBuilder::with_caps`] mutates the
    ///    [`thirtyfour::ChromeCapabilities`] used to create the session (e.g. unset headless, add
    ///    Chrome args, register extensions).
    /// 2. (optional) [`SessionBuilder::with_config`] receives the
    ///    [`thirtyfour::WebDriverBuilder`] and may set client-side options such as the element
    ///    poller, request timeout, user-agent, or keep-alive flag before the session is opened.
    /// 3. (required, terminal) [`SessionBuilder::run`] opens the session, awaits the user
    ///    closure, and tears the session down once the closure resolves or panics.
    ///
    /// Sessions are independent. Multiple sessions can run concurrently against the same
    /// `Chromedriver` via `tokio::join!` or `tokio::spawn` on a multi-thread runtime.
    ///
    /// # Examples
    ///
    /// Smallest case (default headless capabilities, default client config):
    ///
    /// ```no_run
    /// use chrome_for_testing_manager::Chromedriver;
    /// use rootcause::Report;
    ///
    /// # async fn run() -> Result<(), Report> {
    /// Chromedriver::run_default()
    ///     .await?
    ///     .session()
    ///     .run(async |session| {
    ///         session.goto("https://wikipedia.org").await?;
    ///         Ok::<(), thirtyfour::error::WebDriverError>(())
    ///     })
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// With both setup steps:
    ///
    /// ```no_run
    /// use chrome_for_testing_manager::Chromedriver;
    /// use rootcause::Report;
    /// use std::time::Duration;
    /// use thirtyfour::ChromiumLikeCapabilities;
    ///
    /// # async fn run() -> Result<(), Report> {
    /// Chromedriver::run_default()
    ///     .await?
    ///     .session()
    ///     .with_caps(ChromiumLikeCapabilities::unset_headless)
    ///     .with_config(|b| b.request_timeout(Duration::from_secs(30)))
    ///     .run(async |session| {
    ///         session.goto("https://wikipedia.org").await?;
    ///         Ok::<(), thirtyfour::error::WebDriverError>(())
    ///     })
    ///     .await?;
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "thirtyfour")]
    #[must_use]
    pub fn session(&self) -> SessionBuilder<'_, DefaultCaps, DefaultConfig> {
        SessionBuilder::new(self)
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
    fn builder_accepts_cache_dir_and_graceful_shutdown() {
        let shutdown = GracefulShutdown::builder()
            .unix_sigterm(Duration::from_secs(1))
            .windows_ctrl_break(Duration::from_secs(2))
            .build();
        let config = ChromedriverRunConfig::builder()
            .cache_dir(PathBuf::from("/tmp/cft-cache"))
            .graceful_shutdown(shutdown.clone())
            .build();

        assert_that!(config.cache_dir).is_equal_to(Some(PathBuf::from("/tmp/cft-cache")));
        assert_that!(config.graceful_shutdown).is_equal_to(shutdown);
    }

    #[test]
    fn default_graceful_shutdown_uses_three_second_sigterm_and_ctrl_break() {
        let expected = GracefulShutdown::builder()
            .unix_sigterm(Duration::from_secs(3))
            .windows_ctrl_break(Duration::from_secs(3))
            .build();
        assert_that!(default_graceful_shutdown()).is_equal_to(expected);
    }
}
