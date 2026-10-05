//! User-facing configuration for the managed Chrome for Testing facade.
//!
//! Common selection and lifecycle settings remain directly discoverable; technical driver
//! settings are grouped under [`ChromeDriverConfig`].

use crate::CancellationToken;
use crate::browser::ChromeBinary;
use crate::chromedriver::ChromeDriverConfig;
use crate::manager::config::ChromeForTestingManagerConfig;
use crate::policy::{LifecyclePolicy, NetworkPolicy};
use crate::version::VersionRequest;
use ::chrome_for_testing::Channel;
use std::path::PathBuf;
use typed_builder::TypedBuilder;

/// Configuration for launching a managed Chrome for Testing environment.
///
/// Manager-independent settings are directly discoverable here. Technical `ChromeDriver`
/// settings are grouped under [`ChromeDriverConfig`].
///
/// ```
/// use chrome_for_testing_manager::{Channel, ChromeDriverConfig, ChromeForTestingConfig};
///
/// let config = ChromeForTestingConfig::builder()
///     .version(Channel::Beta)
///     .cache_dir("target/chrome-cache".into())
///     .driver(ChromeDriverConfig::builder().port(3000u16).build())
///     .build();
/// ```
#[derive(Debug, Clone, TypedBuilder)]
pub struct ChromeForTestingConfig {
    /// The requested Chrome for Testing version.
    ///
    /// Accepts anything implementing `Into<VersionRequest>`, including [`Channel`] and
    /// [`crate::Version`].
    #[builder(default = VersionRequest::LatestIn(Channel::Stable), setter(into))]
    version: VersionRequest,

    /// The browser package to use for sessions.
    #[builder(default)]
    chrome_binary: ChromeBinary,

    /// Optional cache directory. The platform-specific per-user cache is used when absent.
    #[builder(default, setter(strip_option(fallback = cache_dir_opt)))]
    cache_dir: Option<PathBuf>,

    /// Optional token for cooperative cancellation of the launch and its cleanup.
    ///
    /// See the [crate-level cancellation section](crate#cancellation-and-drop-safety).
    #[builder(default, setter(strip_option(fallback = cancellation_opt)))]
    cancellation: Option<CancellationToken>,

    /// HTTP policy shared by networked services.
    #[builder(default)]
    network: NetworkPolicy,

    /// Process and session lifecycle policy.
    #[builder(default)]
    lifecycle: LifecyclePolicy,

    /// Technical configuration for the managed `ChromeDriver` process.
    #[builder(default)]
    driver: ChromeDriverConfig,
}

impl Default for ChromeForTestingConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ChromeForTestingConfig {
    pub(crate) fn into_parts(
        self,
    ) -> (
        VersionRequest,
        ChromeBinary,
        Option<CancellationToken>,
        ChromeForTestingManagerConfig,
        ChromeDriverConfig,
    ) {
        let Self {
            version,
            chrome_binary,
            cache_dir,
            cancellation,
            network,
            lifecycle,
            driver,
        } = self;
        let manager_config = ChromeForTestingManagerConfig::builder()
            .cache_dir_opt(cache_dir)
            .network(network)
            .lifecycle(lifecycle)
            .build();
        (version, chrome_binary, cancellation, manager_config, driver)
    }
}
