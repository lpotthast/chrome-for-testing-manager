//! User-facing configuration for the managed Chrome for Testing facade.
//!
//! Common selection and lifecycle settings remain directly discoverable; technical driver
//! settings are grouped under [`ChromeDriverConfig`].

use crate::CancellationToken;
use crate::browser::ChromeBinary;
use crate::chromedriver::ChromeDriverConfig;
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
///     .cache_dir("target/chrome-cache")
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
    pub(crate) version: VersionRequest,

    /// The browser package to use for sessions.
    #[builder(default)]
    pub(crate) chrome_binary: ChromeBinary,

    /// Optional cache directory. The platform-specific per-user cache is used when absent.
    #[builder(default, setter(into, strip_option(fallback = cache_dir_opt)))]
    pub(crate) cache_dir: Option<PathBuf>,

    /// Optional token for cooperative cancellation of [`crate::ChromeForTesting::launch`].
    ///
    /// It covers resolution, installation, and driver startup only. Sessions are cancelled
    /// through [`crate::SessionBuilder::with_cancellation`], and the launched environment is
    /// stopped with [`crate::ChromeForTesting::shutdown`] or by dropping it.
    ///
    /// See the [crate-level cancellation section](crate#cancellation-and-drop-safety).
    #[builder(default, setter(strip_option(fallback = cancellation_opt)))]
    pub(crate) cancellation: Option<CancellationToken>,

    /// HTTP policy shared by networked services.
    #[builder(default)]
    pub(crate) network: NetworkPolicy,

    /// Process and session lifecycle policy.
    #[builder(default)]
    pub(crate) lifecycle: LifecyclePolicy,

    /// Technical configuration for the managed `ChromeDriver` process.
    #[builder(default)]
    pub(crate) driver: ChromeDriverConfig,
}

impl Default for ChromeForTestingConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}
