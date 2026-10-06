//! Lower-level API for explicit resolve, install, cache, and launch operations.
//!
//! The services themselves live in their own modules. The manager wires them together and
//! provides the cancellation-aware public methods. See the
//! [crate-level cancellation section](crate#cancellation-and-drop-safety).

pub(crate) mod config;

use crate::artifact_store::ArtifactStore;
use crate::background::BackgroundTasks;
use crate::browser::{BrowserArtifactRequest, ChromeBinary, LoadedBrowserPackage};
use crate::cache::{CacheDir, CachePruneResult};
use crate::chromedriver::ChromeDriverConfig;
use crate::chromedriver::process::ChromeDriverProcess;
use crate::error::HttpClientPurpose;
use crate::manager::config::ChromeForTestingManagerConfig;
use crate::policy::LifecyclePolicy;
use crate::version::resolver::VersionResolver;
use crate::version::{SelectedVersion, VersionRequest};
use crate::{CancellationToken, ChromeForTestingError, Result};
use ::chrome_for_testing::Platform;
use rootcause::prelude::ResultExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Overall deadline for one local `ChromeDriver` or `DevTools` request.
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Lower-level API for version resolution, atomic artifact installation, and process launch.
///
/// [`crate::ChromeForTesting`] composes these steps. Use the manager directly to run them
/// separately, e.g. to pre-warm the cache or to launch several `ChromeDriver` processes off one
/// download. Clones share the cache, the HTTP clients, and the background cleanups.
///
/// ```no_run
/// use chrome_for_testing_manager::{
///     BrowserArtifactRequest, CancellationToken, ChromeDriverConfig, ChromeForTestingManager,
///     Result, VersionRequest,
/// };
///
/// # async fn run() -> Result<()> {
/// let manager = ChromeForTestingManager::new()?;
/// let selected = manager
///     .resolve_version(
///         VersionRequest::stable(),
///         BrowserArtifactRequest::Chrome,
///         CancellationToken::new(),
///     )
///     .await?;
/// let packages = manager.download(&selected, CancellationToken::new()).await?;
/// let driver = manager
///     .launch_driver(&packages[0], ChromeDriverConfig::default(), CancellationToken::new())
///     .await?;
/// println!("ChromeDriver listens on port {}", driver.port());
/// driver.terminate().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ChromeForTestingManager {
    resolver: VersionResolver,
    artifact_store: ArtifactStore,
    /// No-proxy client for loopback requests against `ChromeDriver` and `DevTools`.
    local_client: reqwest::Client,
    /// No-proxy client for `WebDriver` sessions, bounded by the `WebDriver` request deadline.
    #[cfg(feature = "thirtyfour")]
    webdriver_client: reqwest::Client,
    /// Cleanups of dropped session runs, process handles, and installations, shared by all
    /// clones.
    background_tasks: BackgroundTasks,
    lifecycle: LifecyclePolicy,
    platform: Platform,
}

impl ChromeForTestingManager {
    /// Create a manager with the default cache location and policies.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::UnsupportedPlatform`] on platforms without Chrome for
    /// Testing builds, [`ChromeForTestingError::DetermineCacheDir`] or
    /// [`ChromeForTestingError::CreateCacheDir`] if the cache directory cannot be prepared, and
    /// [`ChromeForTestingError::BuildHttpClient`] if an HTTP client cannot be built.
    pub fn new() -> Result<Self> {
        Self::new_with_config(ChromeForTestingManagerConfig::default())
    }

    /// Create a manager with the default policies and a custom cache directory.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::new`].
    pub fn new_with_cache_dir(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::new_with_config(
            ChromeForTestingManagerConfig::builder()
                .cache_dir(cache_dir)
                .build(),
        )
    }

    /// Create a manager from an explicit configuration.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::new`].
    pub fn new_with_config(config: ChromeForTestingManagerConfig) -> Result<Self> {
        let ChromeForTestingManagerConfig {
            cache_dir,
            network,
            lifecycle,
        } = config;
        // Detect the platform first, so that an unsupported one leaves no cache directory behind.
        let platform = Platform::detect().context(ChromeForTestingError::UnsupportedPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        })?;
        let cache_dir = match cache_dir {
            Some(cache_dir) => CacheDir::create_at(cache_dir)?,
            None => CacheDir::get_or_create()?,
        };
        let build_client =
            |builder: reqwest::ClientBuilder, timeout: Duration, purpose: HttpClientPurpose| {
                builder
                    .connect_timeout(network.connect_timeout())
                    .timeout(timeout)
                    .build()
                    .context(ChromeForTestingError::BuildHttpClient { purpose })
            };
        // One client for all requests leaving the machine. Its default deadline covers manifest
        // requests; artifact downloads override it per request.
        let external_client = build_client(
            reqwest::Client::builder(),
            network.manifest_timeout(),
            HttpClientPurpose::ChromeForTesting,
        )?;
        let local_client = build_client(
            reqwest::Client::builder().no_proxy(),
            LOCAL_REQUEST_TIMEOUT,
            HttpClientPurpose::LocalProcess,
        )?;
        #[cfg(feature = "thirtyfour")]
        let webdriver_client = build_client(
            reqwest::Client::builder().no_proxy(),
            network.webdriver_request_timeout(),
            HttpClientPurpose::WebDriver,
        )?;

        let background_tasks = BackgroundTasks::default();
        Ok(Self {
            resolver: VersionResolver::new(external_client.clone(), platform),
            artifact_store: ArtifactStore::new(
                cache_dir,
                external_client,
                network.artifact_download_timeout(),
                platform,
                background_tasks.clone(),
            ),
            local_client,
            #[cfg(feature = "thirtyfour")]
            webdriver_client,
            background_tasks,
            lifecycle,
            platform,
        })
    }

    /// Return the cache root used by this manager.
    ///
    /// The cache contents live in a layout-versioned directory beneath it, so releases of this
    /// crate with incompatible on-disk layouts can share one cache root without interfering.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        self.artifact_store.cache_dir().root()
    }

    /// Return the platform whose artifacts this manager resolves and installs.
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// Remove every cached version, unless the cache is in use.
    ///
    /// Loaded packages, running processes, and installations hold a shared cache lease. This
    /// operation does not wait for them: after a brief retry (about 100 ms) covering lock handover,
    /// it returns [`ChromeForTestingError::CacheInUse`]. Unrecognized entries are left untouched,
    /// and so are version directories that releases before 0.13 stored directly in the cache root.
    /// Delete those manually once no older release uses them.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::CacheInUse`] if the cache is in use, and other errors if
    /// its entries cannot be read or removed.
    pub async fn clear_cache(&self) -> Result<()> {
        crate::ensure_runtime()?;
        self.artifact_store.cache_dir().clear().await
    }

    /// Remove cached version directories except for the explicitly retained versions.
    ///
    /// Like [`Self::clear_cache`], pruning is rejected while the cache is in use, and it leaves
    /// unrecognized entries and pre-0.13 version directories untouched.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::CacheInUse`] if the cache is in use, and other errors if
    /// an unretained version cannot be removed.
    pub async fn prune_cache(
        &self,
        retained_versions: &[chrome_for_testing::Version],
    ) -> Result<CachePruneResult> {
        crate::ensure_runtime()?;
        self.artifact_store
            .cache_dir()
            .prune(retained_versions)
            .await
    }

    /// Resolve a version request against the Chrome for Testing release manifest.
    ///
    /// The selected release provides `ChromeDriver` and every browser package in
    /// `requested_artifacts` for this manager's platform. [`VersionRequest::Latest`] picks the
    /// newest such release. A channel or pinned request uses exactly its release and fails if that
    /// release lacks a requested download.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::Cancelled`] on cancellation,
    /// [`ChromeForTestingError::RequestVersions`] if the manifest cannot be fetched, and
    /// [`ChromeForTestingError::NoMatchingVersion`] if no release matches.
    pub async fn resolve_version(
        &self,
        version_selection: VersionRequest,
        requested_artifacts: BrowserArtifactRequest,
        cancellation: CancellationToken,
    ) -> Result<SelectedVersion> {
        crate::ensure_runtime()?;
        self.resolver
            .resolve(version_selection, requested_artifacts, cancellation)
            .await
    }

    /// Atomically install the browser packages resolved in `selected` and their matching
    /// `ChromeDriver`.
    ///
    /// Returns one package per resolved browser, Chrome before Chrome Headless Shell. Artifacts
    /// already in the cache are reused. The artifacts install concurrently, and all of them are
    /// drained on every outcome. Cancellation waits for extraction and staging cleanup before
    /// returning. Dropping the returned future cancels the installation as well, which then rolls
    /// back in the background. See the
    /// [crate-level cancellation section](crate#cancellation-and-drop-safety).
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::Cancelled`] when caller cancellation initiated the
    /// rollback. Other errors cover missing downloads, locking, transfer, extraction, validation,
    /// and cleanup.
    pub async fn download(
        &self,
        selected: &SelectedVersion,
        cancellation: CancellationToken,
    ) -> Result<Vec<LoadedBrowserPackage>> {
        crate::ensure_runtime()?;
        self.artifact_store
            .install(selected, selected.requested_artifacts(), cancellation)
            .await
    }

    /// Atomically install one browser package and its matching `ChromeDriver`.
    ///
    /// The binary must be part of the resolved artifact set recorded in `selected`. Only
    /// `chrome_binary` and `ChromeDriver` are installed, even if `selected` resolved more. Behaves
    /// like [`Self::download`] otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::BrowserArtifactNotResolved`] when `chrome_binary` was not
    /// part of the resolution, and the same errors as [`Self::download`] otherwise.
    pub async fn download_for(
        &self,
        selected: &SelectedVersion,
        chrome_binary: ChromeBinary,
        cancellation: CancellationToken,
    ) -> Result<LoadedBrowserPackage> {
        use rootcause::option_ext::OptionExt;

        let not_resolved = || ChromeForTestingError::BrowserArtifactNotResolved {
            chrome_binary,
            version: selected.version(),
            platform: selected.platform(),
        };
        crate::ensure_runtime()?;
        if !selected.requested_artifacts().contains(chrome_binary) {
            return Err(rootcause::report!(not_resolved()));
        }
        self.artifact_store
            .install(selected, chrome_binary.into(), cancellation)
            .await?
            .pop()
            .context_with(not_resolved)
    }

    /// Launch the validated package's matching `ChromeDriver`.
    ///
    /// Cancellation terminates a process spawned before the error is returned. Dropping the
    /// returned future, or the returned process, terminates it in the background. See
    /// [`Self::wait_for_background_tasks`].
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::MissingRuntime`] outside a Tokio runtime. Other errors
    /// cover cancellation, spawn, startup, or cleanup failure.
    pub async fn launch_driver(
        &self,
        loaded: &LoadedBrowserPackage,
        config: ChromeDriverConfig,
        cancellation: CancellationToken,
    ) -> Result<ChromeDriverProcess> {
        ChromeDriverProcess::launch(
            loaded.chromedriver_executable(),
            loaded.cache_lease(),
            config,
            &cancellation,
            &self.local_client,
            &self.lifecycle,
            &self.background_tasks,
        )
        .await
    }

    /// Wait for the cleanups that dropped operations handed to the Tokio runtime, and report
    /// their failures.
    ///
    /// Dropping a future or handle cannot await its cleanup, so it runs in the background:
    /// terminating the process of a dropped [`ChromeDriverProcess`], quitting the session of a
    /// dropped session run, or rolling back a dropped installation. This waits until none is left,
    /// including cleanups started while waiting. [`crate::ChromeForTesting::shutdown`] calls it
    /// before terminating `ChromeDriver`.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::BackgroundCleanup`], with each failure attached, if
    /// cleanups failed since the last call.
    pub async fn wait_for_background_tasks(&self) -> Result<()> {
        self.background_tasks.wait().await
    }

    /// Prepare `thirtyfour` capabilities for a loaded browser package: headless, with the cached
    /// browser executable as the binary.
    ///
    /// Use them to open sessions with `thirtyfour` directly against a driver started through
    /// [`Self::launch_driver`]. [`crate::ChromeForTesting::session`] uses them as its starting
    /// point as well.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::PrepareChromeCapabilities`] if the browser path is
    /// not Unicode or `thirtyfour` rejects the capability values.
    #[cfg(feature = "thirtyfour")]
    pub fn prepare_caps(
        &self,
        loaded: &LoadedBrowserPackage,
    ) -> Result<thirtyfour::ChromeCapabilities> {
        use rootcause::option_ext::OptionExt;
        use thirtyfour::ChromiumLikeCapabilities;

        let browser_executable = loaded.browser_executable();
        let prepare_error = || ChromeForTestingError::PrepareChromeCapabilities {
            browser_executable: browser_executable.to_owned(),
        };
        let browser_executable_string = browser_executable.to_str().context_with(prepare_error)?;
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_headless().context_with(prepare_error)?;
        caps.set_binary(browser_executable_string)
            .context_with(prepare_error)?;
        Ok(caps)
    }

    /// The no-proxy HTTP client for `ChromeDriver` status and `DevTools` requests.
    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn local_client(&self) -> &reqwest::Client {
        &self.local_client
    }

    /// The no-proxy HTTP client for `WebDriver` sessions.
    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn webdriver_client(&self) -> &reqwest::Client {
        &self.webdriver_client
    }

    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn lifecycle(&self) -> &LifecyclePolicy {
        &self.lifecycle
    }

    /// Tracks the background cleanups of dropped operations.
    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn background_tasks(&self) -> &BackgroundTasks {
        &self.background_tasks
    }

    #[cfg(test)]
    pub(crate) fn set_manifest_base_url(&mut self, base_url: reqwest::Url) {
        self.resolver.set_manifest_base_url(base_url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkPolicy;
    use crate::artifact_store::COMPLETION_MARKER;
    use crate::test_support::{
        FixtureServer, ResponseSpec, TestDirectory, contains_transaction_residue,
        large_artifact_zip, platform_artifact_zips,
    };
    use assertr::prelude::*;
    use chrome_for_testing::Download;
    use chrome_for_testing::Version;
    use rootcause::Report;
    use std::collections::HashMap;
    use std::result::Result;

    const KNOWN_GOOD_MANIFEST_PATH: &str =
        "/chrome-for-testing/known-good-versions-with-downloads.json";

    #[test]
    fn async_operations_report_a_missing_runtime() -> Result<(), Report> {
        let directory = TestDirectory::new("missing-runtime")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;

        let is_missing_runtime = |error: Report<ChromeForTestingError>| {
            matches!(
                error.current_context(),
                ChromeForTestingError::MissingRuntime
            )
        };
        let resolved = futures::executor::block_on(manager.resolve_version(
            VersionRequest::Latest,
            BrowserArtifactRequest::Chrome,
            CancellationToken::new(),
        ));
        assert_that!(resolved.is_err_and(is_missing_runtime)).is_true();
        let cleared = futures::executor::block_on(manager.clear_cache());
        assert_that!(cleared.is_err_and(is_missing_runtime)).is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_cancelled_download_fails_before_touching_the_cache() -> Result<(), Report> {
        let directory = TestDirectory::new("download-pre-cancelled")?;
        let server = FixtureServer::start(HashMap::new()).await?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let selected = selected_version(&manager, &server);
        let files_before = files_beneath(directory.path());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = manager
            .download(&selected, cancellation)
            .await
            .expect_err("a pre-cancelled download must fail");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        assert_that!(files_beneath(directory.path())).is_equal_to(files_before);
        Ok(())
    }

    /// All files beneath `dir`, recursively, sorted.
    fn files_beneath(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut pending = vec![dir.to_owned()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("readable test directory") {
                let path = entry.expect("readable test directory entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_interrupts_stalled_manifest_fetch() -> Result<(), Report> {
        let directory = TestDirectory::new("manifest-cancellation")?;
        let server = FixtureServer::start(HashMap::from([(
            KNOWN_GOOD_MANIFEST_PATH.to_owned(),
            ResponseSpec::Stall,
        )]))
        .await?;
        let mut manager = test_manager(&directory, Duration::from_secs(5))?;
        manager.set_manifest_base_url(server.base_url());
        let cancellation = CancellationToken::new();
        let cancel_after_request = async {
            server.wait_for_hits(KNOWN_GOOD_MANIFEST_PATH, 1).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            manager.resolve_version(
                VersionRequest::Latest,
                ChromeBinary::Chrome.into(),
                cancellation.clone(),
            ),
            cancel_after_request,
        );
        assert_that!(matches!(
            result
                .expect_err("manifest fetch must be cancelled")
                .current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latest_resolution_skips_newer_versions_missing_requested_artifacts()
    -> Result<(), Report> {
        let directory = TestDirectory::new("artifact-aware-resolution")?;
        let mut manager = test_manager(&directory, Duration::from_secs(5))?;
        let platform = manager.platform().to_string();
        let manifest = serde_json::json!({
            "timestamp": "2026-08-18T00:00:00Z",
            "versions": [
                {
                    "version": "135.0.7019.0",
                    "revision": "1",
                    "downloads": {
                        "chrome": [{"platform": platform, "url": "https://example/chrome-135.zip"}],
                        "chromedriver": [{"platform": platform, "url": "https://example/driver-135.zip"}],
                        "chrome-headless-shell": [{"platform": platform, "url": "https://example/headless-135.zip"}]
                    }
                },
                {
                    "version": "136.0.7103.0",
                    "revision": "2",
                    "downloads": {
                        "chrome": [{"platform": platform, "url": "https://example/chrome-136.zip"}],
                        "chromedriver": [{"platform": platform, "url": "https://example/driver-136.zip"}]
                    }
                }
            ]
        });
        let server = FixtureServer::start(HashMap::from([(
            KNOWN_GOOD_MANIFEST_PATH.to_owned(),
            ResponseSpec::body(serde_json::to_vec(&manifest)?),
        )]))
        .await?;
        manager.set_manifest_base_url(server.base_url());

        let headless = manager
            .resolve_version(
                VersionRequest::Latest,
                BrowserArtifactRequest::ChromeHeadlessShell,
                CancellationToken::new(),
            )
            .await?;
        assert_that!(headless.version().to_string()).is_equal_to("135.0.7019.0");
        assert_that!(headless.has_chrome_headless_shell_download()).is_true();

        let chrome = manager
            .resolve_version(
                VersionRequest::Latest,
                BrowserArtifactRequest::Chrome,
                CancellationToken::new(),
            )
            .await?;
        assert_that!(chrome.version().to_string()).is_equal_to("136.0.7103.0");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_interrupts_stalled_body_and_cleans_staging() -> Result<(), Report> {
        let directory = TestDirectory::new("transfer-cancellation")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let (_, driver_zip) = platform_artifact_zips(manager.platform)?;
        let server = FixtureServer::start(HashMap::from([
            ("/browser.zip".to_owned(), ResponseSpec::Stall),
            ("/driver.zip".to_owned(), ResponseSpec::body(driver_zip)),
        ]))
        .await?;
        let selected = selected_version(&manager, &server);
        let cancellation = CancellationToken::new();
        let cancel_after_request = async {
            server.wait_for_hits("/browser.zip", 1).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            manager.download(&selected, cancellation.clone()),
            cancel_after_request,
        );
        assert_that!(matches!(
            result
                .expect_err("transfer must be cancelled")
                .current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        assert_that!(contains_transaction_residue(directory.path()).await?).is_false();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_extraction_awaits_worker_and_cleans_partial_tree()
    -> Result<(), Report> {
        let directory = TestDirectory::new("extraction-cancellation")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let browser_zip = large_artifact_zip(manager.platform.chrome_executable_path())?;
        let (_, driver_zip) = platform_artifact_zips(manager.platform)?;
        let server = FixtureServer::start(HashMap::from([
            ("/browser.zip".to_owned(), ResponseSpec::body(browser_zip)),
            ("/driver.zip".to_owned(), ResponseSpec::body(driver_zip)),
        ]))
        .await?;
        let selected = selected_version(&manager, &server);
        let cancellation = CancellationToken::new();
        let cancel_after_partial_extraction = async {
            wait_for_partial_browser_extraction(&manager, selected.version).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            manager.download(&selected, cancellation.clone()),
            cancel_after_partial_extraction,
        );
        assert_that!(matches!(
            result
                .expect_err("in-flight extraction must be cancelled")
                .current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        assert_that!(contains_transaction_residue(directory.path()).await?).is_false();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn truncated_zip_is_rejected_without_transaction_residue() -> Result<(), Report> {
        let directory = TestDirectory::new("truncated-zip")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let (_, driver_zip) = platform_artifact_zips(manager.platform)?;
        let server = FixtureServer::start(HashMap::from([
            (
                "/browser.zip".to_owned(),
                ResponseSpec::body(b"PK\x03\x04truncated".to_vec()),
            ),
            ("/driver.zip".to_owned(), ResponseSpec::body(driver_zip)),
        ]))
        .await?;
        let selected = selected_version(&manager, &server);

        let error = manager
            .download(&selected, CancellationToken::new())
            .await
            .expect_err("truncated ZIP must fail");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::InvalidZip { .. } | ChromeForTestingError::ExtractZip { .. }
        ))
        .is_true();
        assert_that!(contains_transaction_residue(directory.path()).await?).is_false();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_managers_install_each_artifact_once_and_hold_cache_lease()
    -> Result<(), Report> {
        let directory = TestDirectory::new("concurrent-install")?;
        let manager_a = test_manager(&directory, Duration::from_secs(5))?;
        let manager_b = test_manager(&directory, Duration::from_secs(5))?;
        let (browser_zip, driver_zip) = platform_artifact_zips(manager_a.platform)?;
        let server = FixtureServer::start(HashMap::from([
            ("/browser.zip".to_owned(), ResponseSpec::body(browser_zip)),
            ("/driver.zip".to_owned(), ResponseSpec::body(driver_zip)),
        ]))
        .await?;
        let selected = selected_version(&manager_a, &server);

        let (packages_a, packages_b) = tokio::join!(
            manager_a.download(&selected, CancellationToken::new()),
            manager_b.download(&selected, CancellationToken::new()),
        );
        let packages_a = packages_a?;
        let packages_b = packages_b?;
        assert_that!(server.hits("/browser.zip")).is_equal_to(1);
        assert_that!(server.hits("/driver.zip")).is_equal_to(1);
        assert_that!(contains_transaction_residue(directory.path()).await?).is_false();

        let clear_error = manager_a
            .clear_cache()
            .await
            .expect_err("loaded packages keep a shared cache lease");
        assert_that!(matches!(
            clear_error.current_context(),
            ChromeForTestingError::CacheInUse { .. }
        ))
        .is_true();
        drop(packages_a);
        drop(packages_b);
        manager_a.clear_cache().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_for_installs_only_the_requested_browser() -> Result<(), Report> {
        let directory = TestDirectory::new("download-for-one-browser")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let (browser_zip, driver_zip) = platform_artifact_zips(manager.platform)?;
        let server = FixtureServer::start(HashMap::from([
            ("/browser.zip".to_owned(), ResponseSpec::body(browser_zip)),
            ("/driver.zip".to_owned(), ResponseSpec::body(driver_zip)),
            (
                "/headless-shell.zip".to_owned(),
                ResponseSpec::body(b"not a zip".to_vec()),
            ),
        ]))
        .await?;
        let mut selected = selected_version(&manager, &server);
        selected.requested_artifacts = BrowserArtifactRequest::Both;
        selected.chrome_headless_shell = Some(Download {
            platform: manager.platform,
            url: server.url("/headless-shell.zip"),
        });

        let loaded = manager
            .download_for(&selected, ChromeBinary::Chrome, CancellationToken::new())
            .await?;

        assert_that!(loaded.chrome_binary()).is_equal_to(ChromeBinary::Chrome);
        assert_that!(server.hits("/headless-shell.zip")).is_equal_to(0);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_and_legacy_markers_are_reinstalled_exactly_once() -> Result<(), Report> {
        let directory = TestDirectory::new("marker-migration")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let (browser_zip, driver_zip) = platform_artifact_zips(manager.platform)?;
        let server = FixtureServer::start(HashMap::from([
            ("/browser.zip".to_owned(), ResponseSpec::body(browser_zip)),
            ("/driver.zip".to_owned(), ResponseSpec::body(driver_zip)),
        ]))
        .await?;
        let selected = selected_version(&manager, &server);

        let packages = manager
            .download(&selected, CancellationToken::new())
            .await?;
        let marker = package_marker(
            packages[0].browser_executable(),
            manager.artifact_store.cache_dir().path(),
        );
        drop(packages);
        tokio::fs::write(&marker, "invalid marker").await?;

        drop(
            manager
                .download(&selected, CancellationToken::new())
                .await?,
        );
        assert_that!(server.hits("/browser.zip")).is_equal_to(2);
        drop(
            manager
                .download(&selected, CancellationToken::new())
                .await?,
        );
        assert_that!(server.hits("/browser.zip")).is_equal_to(2);

        let packages = manager
            .download(&selected, CancellationToken::new())
            .await?;
        let browser_executable = packages[0].browser_executable().to_owned();
        drop(packages);
        tokio::fs::write(&browser_executable, "tampered executable").await?;
        drop(
            manager
                .download(&selected, CancellationToken::new())
                .await?,
        );
        assert_that!(server.hits("/browser.zip"))
            .with_detail_message("executable size mismatch must force a reinstall")
            .is_equal_to(3);

        tokio::fs::remove_file(&marker).await?;
        drop(
            manager
                .download(&selected, CancellationToken::new())
                .await?,
        );
        assert_that!(server.hits("/browser.zip")).is_equal_to(4);
        drop(
            manager
                .download(&selected, CancellationToken::new())
                .await?,
        );
        assert_that!(server.hits("/browser.zip")).is_equal_to(4);
        Ok(())
    }

    #[cfg(all(unix, feature = "thirtyfour"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn prepare_caps_reports_non_unicode_browser_path() -> Result<(), Report> {
        use std::os::unix::ffi::OsStringExt;

        let directory = TestDirectory::new("non-unicode-path")?;
        let manager = test_manager(&directory, Duration::from_secs(5))?;
        let lease = manager
            .artifact_store
            .cache_dir()
            .acquire_shared(&CancellationToken::new())
            .await?;
        let loaded = LoadedBrowserPackage::new(
            ChromeBinary::Chrome,
            manager
                .cache_dir()
                .join(std::ffi::OsString::from_vec(vec![b'b', 0xff])),
            manager.cache_dir().join("chromedriver"),
            lease,
        );
        let error = manager
            .prepare_caps(&loaded)
            .expect_err("non-Unicode path must return a typed error");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::PrepareChromeCapabilities { .. }
        ))
        .is_true();
        Ok(())
    }

    fn test_manager(
        directory: &TestDirectory,
        artifact_timeout: Duration,
    ) -> Result<ChromeForTestingManager, Report<ChromeForTestingError>> {
        ChromeForTestingManager::new_with_config(
            ChromeForTestingManagerConfig::builder()
                .cache_dir(directory.path().to_owned())
                .network(
                    NetworkPolicy::builder()
                        .artifact_download_timeout(artifact_timeout)
                        .build(),
                )
                .build(),
        )
    }

    fn selected_version(
        manager: &ChromeForTestingManager,
        server: &FixtureServer,
    ) -> SelectedVersion {
        let version: Version = "135.0.7019.0".parse().expect("valid version literal");
        SelectedVersion {
            channel: None,
            version,
            platform: manager.platform,
            requested_artifacts: ChromeBinary::Chrome.into(),
            chrome: Some(Download {
                platform: manager.platform,
                url: server.url("/browser.zip"),
            }),
            chrome_headless_shell: None,
            chromedriver: Some(Download {
                platform: manager.platform,
                url: server.url("/driver.zip"),
            }),
        }
    }

    fn package_marker(executable: &Path, cache_dir: &Path) -> PathBuf {
        let relative = executable
            .strip_prefix(cache_dir)
            .expect("loaded executable is beneath test cache");
        let mut components = relative.components();
        let version = components.next().expect("version component");
        let platform = components.next().expect("platform component");
        let package = components.next().expect("package component");
        cache_dir
            .join(version)
            .join(platform)
            .join(package)
            .join(COMPLETION_MARKER)
    }

    async fn wait_for_partial_browser_extraction(
        manager: &ChromeForTestingManager,
        version: Version,
    ) {
        let package_root = manager
            .platform
            .chrome_executable_path()
            .components()
            .next()
            .expect("platform executable path has a package root");
        let platform_dir = manager
            .artifact_store
            .cache_dir()
            .path()
            .join(version.to_string())
            .join(manager.platform.to_string());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(mut entries) = tokio::fs::read_dir(&platform_dir).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".staging.chrome.")
                            && tokio::fs::metadata(
                                entry
                                    .path()
                                    .join("unpacked")
                                    .join(package_root)
                                    .join("padding-0000.bin"),
                            )
                            .await
                            .is_ok()
                        {
                            return;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("browser extraction produced a partial tree");
    }
}
