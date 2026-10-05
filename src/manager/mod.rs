//! Lower-level composition facade for explicit resolve, install, cache, and launch operations.
//!
//! Domain services live in their owning modules; the manager wires them together and provides
//! cancellation-aware public orchestration methods. See the
//! [crate-level cancellation section](crate#cancellation-and-drop-safety) for the drop-safety
//! guarantee behind the cleanup-sensitive methods.

pub(crate) mod config;

use crate::artifact_store::ArtifactStore;
use crate::browser::{BrowserArtifactRequest, ChromeBinary, LoadedBrowserPackage};
use crate::cache::{CacheDir, CachePruneResult};
use crate::chromedriver::ChromeDriverConfig;
use crate::chromedriver::process::{ChromeDriverLaunchRequest, ChromeDriverProcess};
use crate::manager::config::ChromeForTestingManagerConfig;
use crate::operation::AbortSafeOperation;
use crate::policy::LifecyclePolicy;
use crate::version::resolver::VersionResolver;
use crate::version::{SelectedVersion, VersionRequest};
use crate::{CancellationToken, ChromeForTestingError, Result};
use ::chrome_for_testing::Platform;
use rootcause::{Report, prelude::ResultExt, report};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Overall deadline for one local `ChromeDriver` or `DevTools` request.
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Lower-level façade for version resolution, atomic artifact installation, and process launch.
#[derive(Debug, Clone)]
pub struct ChromeForTestingManager {
    resolver: VersionResolver,
    artifact_store: ArtifactStore,
    /// No-proxy client for loopback requests against `ChromeDriver` and `DevTools`.
    local_client: reqwest::Client,
    lifecycle: LifecyclePolicy,
    platform: Platform,
}

impl ChromeForTestingManager {
    /// Create a manager with default cache location and timeouts.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform, cache, or HTTP clients cannot be prepared.
    pub fn new() -> Result<Self> {
        Self::new_with_config(ChromeForTestingManagerConfig::default())
    }

    /// Create a manager with default timeouts and a custom cache directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform, cache, or HTTP clients cannot be prepared.
    pub fn new_with_cache_dir(cache_dir: PathBuf) -> Result<Self> {
        Self::new_with_config(
            ChromeForTestingManagerConfig::builder()
                .cache_dir(cache_dir)
                .build(),
        )
    }

    /// Create a manager from explicit cache and timeout configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform, cache, or one of the two HTTP clients cannot be
    /// prepared.
    pub fn new_with_config(config: ChromeForTestingManagerConfig) -> Result<Self> {
        let (cache_dir, network, lifecycle) = config.into_parts();
        let cache_dir = match cache_dir {
            Some(cache_dir) => CacheDir::create_at(cache_dir)?,
            None => CacheDir::get_or_create()?,
        };
        let platform = Platform::detect().map_err(Self::unsupported_platform_error)?;
        // One client for all requests leaving the machine. Its default deadline covers manifest
        // requests; artifact downloads override it per request.
        let external_client = reqwest::Client::builder()
            .connect_timeout(network.connect_timeout())
            .timeout(network.manifest_timeout())
            .build()
            .context(ChromeForTestingError::BuildHttpClient {
                purpose: "Chrome for Testing",
            })?;
        let local_client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(network.connect_timeout())
            .timeout(LOCAL_REQUEST_TIMEOUT)
            .build()
            .context(ChromeForTestingError::BuildHttpClient {
                purpose: "local process",
            })?;
        let resolver = VersionResolver::new(external_client.clone(), platform);
        let artifact_store = ArtifactStore::new(
            cache_dir,
            external_client,
            network.artifact_download_timeout(),
            platform,
        );

        Ok(Self {
            resolver,
            artifact_store,
            local_client,
            lifecycle,
            platform,
        })
    }

    /// Return the cache root used by this manager.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        self.artifact_store.cache_dir().path()
    }

    /// Return the platform whose artifacts this manager resolves and installs.
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// Clear installed artifacts when no download or loaded package holds a cache lease.
    ///
    /// This operation deliberately does not wait for users of the cache. It returns
    /// [`ChromeForTestingError::CacheInUse`] immediately instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache is in use or its entries cannot be removed.
    pub async fn clear_cache(&self) -> Result<()> {
        self.artifact_store.clear().await
    }

    /// Remove cached version directories except for the explicitly retained versions.
    ///
    /// Like [`Self::clear_cache`], pruning is rejected while any loaded package or installation
    /// owns a cache lease. Unrecognized root entries and the lock namespace are left untouched.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache is in use or an unretained version cannot be removed.
    pub async fn prune_cache(
        &self,
        retained_versions: &[chrome_for_testing::Version],
    ) -> Result<CachePruneResult> {
        self.artifact_store.prune(retained_versions).await
    }

    /// Resolve a version request against the Chrome for Testing release manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::Cancelled`] on cancellation, or an error if the
    /// manifest cannot be fetched or no matching version exists.
    pub async fn resolve_version(
        &self,
        version_selection: VersionRequest,
        requested_artifacts: BrowserArtifactRequest,
        cancellation: CancellationToken,
    ) -> Result<SelectedVersion> {
        self.resolver
            .resolve(version_selection, requested_artifacts, cancellation)
            .await
    }

    /// Atomically install requested browser packages and their matching `ChromeDriver`.
    ///
    /// Concurrent artifact transactions are drained on every outcome, and cancellation waits for
    /// extraction and staging cleanup before returning; see the
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
        let selected = selected.clone();
        self.run_installation(
            cancellation,
            move |store, operation_cancellation| async move {
                store.download(&selected, operation_cancellation).await
            },
        )
        .await
    }

    /// Atomically install one browser package and its matching `ChromeDriver`.
    ///
    /// The binary must be part of the resolved artifact set recorded in `selected`. Behaves like
    /// [`Self::download`] otherwise.
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
        let selected = selected.clone();
        self.run_installation(
            cancellation,
            move |store, operation_cancellation| async move {
                store
                    .download_for(&selected, chrome_binary, operation_cancellation)
                    .await
            },
        )
        .await
    }

    /// Run an installation closure over a cloned artifact store inside an abort-safe operation.
    async fn run_installation<T, F, Fut>(
        &self,
        cancellation: CancellationToken,
        start: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(ArtifactStore, CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let artifact_store = self.artifact_store.clone();
        AbortSafeOperation::run(
            "artifact installation",
            cancellation,
            move |operation_cancellation| start(artifact_store, operation_cancellation),
        )
        .await
    }

    /// Launch the validated package's matching `ChromeDriver`.
    ///
    /// Cancellation terminates any process acquired before the error is returned; see the
    /// [crate-level cancellation section](crate#cancellation-and-drop-safety).
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, unsupported runtime, spawn, startup, or cleanup failure.
    pub async fn launch_driver(
        &self,
        loaded: &LoadedBrowserPackage,
        config: ChromeDriverConfig,
        cancellation: CancellationToken,
    ) -> Result<ChromeDriverProcess> {
        let status_client = self.local_client.clone();
        let lifecycle = self.lifecycle.clone();
        let executable = loaded.chromedriver_executable().to_owned();
        let cache_lease = loaded.cache_lease();
        AbortSafeOperation::run(
            "ChromeDriver launch",
            cancellation,
            move |operation_cancellation| async move {
                ChromeDriverProcess::launch(
                    ChromeDriverLaunchRequest {
                        executable,
                        cache_lease,
                        config,
                        cancellation: operation_cancellation,
                    },
                    &status_client,
                    &lifecycle,
                )
                .await
            },
        )
        .await
    }

    /// Prepare default headless Chrome capabilities for a loaded browser package.
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
        use thirtyfour::ChromiumLikeCapabilities;

        let browser_executable = loaded.browser_executable();
        let browser_executable_string = browser_executable.to_str().ok_or_else(|| {
            report!(ChromeForTestingError::PrepareChromeCapabilities {
                browser_executable: browser_executable.to_owned(),
            })
        })?;
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_headless()
            .context(ChromeForTestingError::PrepareChromeCapabilities {
                browser_executable: browser_executable.to_owned(),
            })?;
        if loaded.chrome_binary() == ChromeBinary::Chrome {
            caps.set_binary(browser_executable_string).context(
                ChromeForTestingError::PrepareChromeCapabilities {
                    browser_executable: browser_executable.to_owned(),
                },
            )?;
        }
        Ok(caps)
    }

    #[cfg(feature = "thirtyfour")]
    pub(crate) async fn launch_headless_shell_session(
        &self,
        loaded: &LoadedBrowserPackage,
        caps: &mut thirtyfour::ChromeCapabilities,
        cancellation: &CancellationToken,
    ) -> Result<crate::session::headless_shell::HeadlessShellSession> {
        crate::session::headless_shell::HeadlessShellSession::launch(
            loaded,
            caps,
            self.lifecycle.graceful_shutdown().clone(),
            &self.local_client,
            &self.lifecycle,
            cancellation,
        )
        .await
    }

    #[cfg(feature = "thirtyfour")]
    pub(crate) const fn session_cleanup_timeout(&self) -> Duration {
        self.lifecycle.session_cleanup_timeout()
    }

    #[cfg(test)]
    pub(crate) fn set_manifest_base_url(&mut self, base_url: reqwest::Url) {
        self.resolver.set_manifest_base_url(base_url);
    }

    fn unsupported_platform_error(error: impl std::fmt::Display) -> Report<ChromeForTestingError> {
        report!(ChromeForTestingError::UnsupportedPlatform)
            .attach(format!("chrome-for-testing error:\n{error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkPolicy;
    use crate::artifact_store::completion_marker_name;
    use crate::test_support::{
        FixtureServer, ResponseSpec, TestDirectory, contains_transaction_residue,
        large_artifact_zip, platform_artifact_zips,
    };
    use assertr::prelude::*;
    use chrome_for_testing::Download;
    use chrome_for_testing::Version;
    use std::collections::HashMap;
    use std::result::Result;

    const KNOWN_GOOD_MANIFEST_PATH: &str =
        "/chrome-for-testing/known-good-versions-with-downloads.json";

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
        let marker = package_marker(packages[0].browser_executable(), manager.cache_dir());
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
            .acquire_shared(CancellationToken::new())
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
            .join(completion_marker_name())
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
            .cache_dir()
            .join(version.to_string())
            .join(manager.platform.to_string());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(mut entries) = tokio::fs::read_dir(&platform_dir).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".staging-chrome-")
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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("browser extraction produced a partial tree");
    }
}
