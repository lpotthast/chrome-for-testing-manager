//! Shared network and lifecycle policies.
//!
//! These value types configure several independent subsystems. Aggregate configurations own
//! them, while resolvers, stores, drivers, and sessions consume only the settings they need.

use std::time::Duration;
use tokio_process_tools::GracefulShutdown;
use typed_builder::TypedBuilder;

/// HTTP connection and request deadlines used by networked services.
///
/// The `_timeout` suffix is kept on every field so that the generated builder setters say what
/// they configure.
#[expect(clippy::struct_field_names)]
#[derive(Debug, Clone, TypedBuilder)]
pub struct NetworkPolicy {
    /// TCP connection deadline shared by all HTTP clients (manifest, artifact, readiness, `DevTools`,
    /// and `WebDriver`).
    #[builder(default = Duration::from_secs(30))]
    connect_timeout: Duration,

    /// Overall deadline for a release-manifest request.
    #[builder(default = Duration::from_secs(30))]
    manifest_timeout: Duration,

    /// Overall deadline for one artifact request, including its response body.
    #[builder(default = Duration::from_secs(15 * 60))]
    artifact_download_timeout: Duration,

    /// Overall deadline for one `WebDriver` request of a managed session.
    #[cfg_attr(
        not(feature = "thirtyfour"),
        expect(dead_code, reason = "session-only setting")
    )]
    #[builder(default = Duration::from_secs(120))]
    webdriver_request_timeout: Duration,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl NetworkPolicy {
    /// Return the TCP connection deadline.
    pub(crate) const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Return the release-manifest request deadline.
    pub(crate) const fn manifest_timeout(&self) -> Duration {
        self.manifest_timeout
    }

    /// Return the per-artifact download deadline.
    pub(crate) const fn artifact_download_timeout(&self) -> Duration {
        self.artifact_download_timeout
    }

    /// Return the per-request deadline of managed `WebDriver` sessions.
    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn webdriver_request_timeout(&self) -> Duration {
        self.webdriver_request_timeout
    }
}

/// Process startup, readiness, graceful shutdown, and session cleanup policy.
#[derive(Debug, Clone, TypedBuilder)]
pub struct LifecyclePolicy {
    /// Per-platform graceful-shutdown policy for managed child processes.
    #[builder(default = Self::default_graceful_shutdown())]
    graceful_shutdown: GracefulShutdown,

    /// Maximum time for `ChromeDriver` to expose and confirm its status endpoint.
    #[builder(default = Duration::from_secs(10))]
    driver_startup_timeout: Duration,

    /// Maximum time for Chrome Headless Shell to expose `DevTools` and open its initial page.
    #[cfg_attr(
        not(feature = "thirtyfour"),
        expect(dead_code, reason = "session-only setting")
    )]
    #[builder(default = Duration::from_secs(10))]
    headless_shell_startup_timeout: Duration,

    /// Upper bound for quitting a `WebDriver` session during cleanup.
    ///
    /// A session that cannot be quit in time is abandoned. Terminating a Chrome Headless Shell
    /// afterwards is bounded by the `graceful_shutdown` policy instead.
    #[cfg_attr(
        not(feature = "thirtyfour"),
        expect(dead_code, reason = "session-only setting")
    )]
    #[builder(default = Duration::from_secs(30))]
    session_cleanup_timeout: Duration,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl LifecyclePolicy {
    /// Return the graceful-shutdown policy for managed child processes.
    pub(crate) const fn graceful_shutdown(&self) -> &GracefulShutdown {
        &self.graceful_shutdown
    }

    /// Return the `ChromeDriver` startup deadline.
    pub(crate) const fn driver_startup_timeout(&self) -> Duration {
        self.driver_startup_timeout
    }

    /// Return the Chrome Headless Shell startup deadline.
    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn headless_shell_startup_timeout(&self) -> Duration {
        self.headless_shell_startup_timeout
    }

    /// Return the deadline for quitting a `WebDriver` session during cleanup.
    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn session_cleanup_timeout(&self) -> Duration {
        self.session_cleanup_timeout
    }

    /// Construct the default per-platform graceful-shutdown policy.
    ///
    /// The default allows 3 seconds for `SIGTERM` on Unix or `CTRL_BREAK_EVENT` on Windows before
    /// forced termination.
    fn default_graceful_shutdown() -> GracefulShutdown {
        let timeout = Duration::from_secs(3);
        GracefulShutdown::builder()
            .unix_sigterm(timeout)
            .windows_ctrl_break(timeout)
            .build()
    }
}
