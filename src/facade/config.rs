//! User-facing configuration for the managed Chrome for Testing facade.
//!
//! Version, browser, cache, and policy settings live directly on [`ChromeForTestingConfig`].
//! Settings of the `ChromeDriver` process are grouped under [`ChromeDriverConfig`].

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
/// Every setting has a default, so `ChromeForTestingConfig::default()` launches the latest Stable
/// Chrome with an OS-assigned `ChromeDriver` port. The builder offers these setters:
///
/// - `version`: the Chrome for Testing version to resolve. Accepts anything implementing
///   `Into<VersionRequest>`, such as a [`Channel`] or a [`crate::Version`]. Defaults to the latest
///   Stable release.
/// - `chrome_binary`: the browser package to download and run sessions against. Defaults to
///   [`ChromeBinary::Chrome`].
/// - `cache_dir` (or `cache_dir_opt`): the cache root. Defaults to the platform's per-user cache
///   directory.
/// - `cancellation` (or `cancellation_opt`): a token cancelling [`crate::ChromeForTesting::launch`]
///   cooperatively. It covers version resolution, installation, and `ChromeDriver` startup only.
///   Sessions are cancelled through `SessionBuilder::with_cancellation` (feature `thirtyfour`),
///   and the launched environment is stopped with [`crate::ChromeForTesting::shutdown`]. See the
///   [crate-level cancellation section](crate#cancellation-and-drop-safety).
/// - `network`: the [`NetworkPolicy`] with HTTP deadlines.
/// - `lifecycle`: the [`LifecyclePolicy`] with startup, shutdown, and session-cleanup timing.
/// - `driver`: the [`ChromeDriverConfig`] with the `ChromeDriver` port and log level.
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
    #[builder(default = VersionRequest::LatestIn(Channel::Stable), setter(into))]
    pub(crate) version: VersionRequest,

    /// The browser package to download and run sessions against.
    #[builder(default)]
    pub(crate) chrome_binary: ChromeBinary,

    /// The cache root, or `None` for the platform's per-user cache directory.
    #[builder(default, setter(into, strip_option(fallback = cache_dir_opt)))]
    pub(crate) cache_dir: Option<PathBuf>,

    /// Cooperative cancellation of [`crate::ChromeForTesting::launch`].
    #[builder(default, setter(strip_option(fallback = cancellation_opt)))]
    pub(crate) cancellation: Option<CancellationToken>,

    /// HTTP deadlines.
    #[builder(default)]
    pub(crate) network: NetworkPolicy,

    /// Process and session lifecycle policy.
    #[builder(default)]
    pub(crate) lifecycle: LifecyclePolicy,

    /// Settings of the managed `ChromeDriver` process.
    #[builder(default)]
    pub(crate) driver: ChromeDriverConfig,
}

impl Default for ChromeForTestingConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}
