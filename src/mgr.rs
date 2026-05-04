use crate::cache::CacheDir;
use crate::download;
use crate::output::{DriverOutputInspectors, DriverOutputListener};
use crate::port::{Port, PortRequest};
use crate::version::{SelectedVersion, VersionRequest};
use crate::{ChromeForTestingArtifact, ChromeForTestingManagerError};
use chrome_for_testing::{KnownGoodVersions, LastKnownGoodVersions, Platform, Version};
use rootcause::{Report, bail, option_ext::OptionExt, prelude::ResultExt, report};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio_process_tools::{
    BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_MAX_LINE_LENGTH,
    DEFAULT_READ_CHUNK_SIZE, LineOverflowBehavior, LineParsingOptions, NumBytesExt, Process,
    ProcessHandle, ReliableDelivery, ReplayEnabled, WaitForLineResult,
};

/// A downloaded Chrome and `ChromeDriver` pair, with their on-disk executable paths resolved.
///
/// Returned by [`ChromeForTestingManager::download`]. Hand it to
/// [`ChromeForTestingManager::launch_chromedriver`] or [`ChromeForTestingManager::prepare_caps`]
/// to drive a browser session.
#[derive(Debug)]
pub struct LoadedChromePackage {
    chrome_executable: PathBuf,
    chromedriver_executable: PathBuf,
}

impl LoadedChromePackage {
    /// Path to the cached Chrome browser executable.
    #[must_use]
    pub fn chrome_executable(&self) -> &std::path::Path {
        &self.chrome_executable
    }

    /// Path to the cached `ChromeDriver` executable.
    #[must_use]
    pub fn chromedriver_executable(&self) -> &std::path::Path {
        &self.chromedriver_executable
    }
}

/// Lower-level orchestrator for chrome-for-testing artifacts.
///
/// Most users should use [`crate::Chromedriver`], which wraps this manager with sensible defaults
/// and handles process lifecycle automatically. Reach for `ChromeForTestingManager` directly when
/// you need finer control:
///
/// - **Pre-warm a cache** without spawning chromedriver: call [`Self::resolve_version`] and
///   [`Self::download`], then drop the result.
/// - **Run multiple chromedriver instances** off a single resolved version: call
///   [`Self::launch_chromedriver`] repeatedly with the same `LoadedChromePackage`.
/// - **Inspect or modify the resolved version** before downloading (channel, available platforms).
/// - **Pin a custom cache directory** via [`Self::new_with_cache_dir`] (useful in CI).
/// - **Drive sessions through a non-`thirtyfour`** `WebDriver` client by using the chromedriver
///   process and port directly.
#[derive(Debug)]
pub struct ChromeForTestingManager {
    client: reqwest::Client,
    cache_dir: CacheDir,
    platform: Platform,
}

impl ChromeForTestingManager {
    /// Create a manager that uses the platform-default cache directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the current platform is unsupported or the cache directory
    /// cannot be determined or created.
    pub fn new() -> Result<Self, Report<ChromeForTestingManagerError>> {
        Ok(Self {
            client: reqwest::Client::new(),
            cache_dir: CacheDir::get_or_create()?,
            platform: Platform::detect()
                .context(ChromeForTestingManagerError::UnsupportedPlatform)?,
        })
    }

    /// Create a manager that caches downloaded artifacts under `cache_dir`.
    ///
    /// The directory is created if it does not exist. Useful in CI to share the cache across
    /// runs, or to keep artifacts out of the user-default cache location.
    ///
    /// # Errors
    ///
    /// Returns an error if the current platform is unsupported or the directory cannot be created.
    pub fn new_with_cache_dir(
        cache_dir: PathBuf,
    ) -> Result<Self, Report<ChromeForTestingManagerError>> {
        Ok(Self {
            client: reqwest::Client::new(),
            cache_dir: CacheDir::create_at(cache_dir)?,
            platform: Platform::detect()
                .context(ChromeForTestingManagerError::UnsupportedPlatform)?,
        })
    }

    fn version_dir(&self, version: Version) -> PathBuf {
        self.cache_dir.path().join(version.to_string())
    }

    /// # Errors
    ///
    /// Returns an error if the cache directory cannot be deleted or re-created.
    pub async fn clear_cache(&self) -> Result<(), Report<ChromeForTestingManagerError>> {
        self.cache_dir.clear().await
    }

    /// Resolve a [`VersionRequest`] against the chrome-for-testing release index.
    ///
    /// Returns a [`SelectedVersion`] suitable for [`Self::download`]. No artifacts are downloaded
    /// at this point; this only performs the HTTP requests needed to determine which version to
    /// fetch.
    ///
    /// # Errors
    ///
    /// Returns an error if the version manifest cannot be fetched or no matching version exists.
    pub async fn resolve_version(
        &self,
        version_selection: VersionRequest,
    ) -> Result<SelectedVersion, Report<ChromeForTestingManagerError>> {
        let selected = match &version_selection {
            VersionRequest::Latest => {
                let all = KnownGoodVersions::fetch(&self.client).await.context(
                    ChromeForTestingManagerError::RequestVersions {
                        version_request: version_selection.clone(),
                    },
                )?;
                all.versions
                    .iter()
                    .filter(|v| v.downloads.chromedriver.is_some())
                    .max_by_key(|v| v.version)
                    .cloned()
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
            VersionRequest::LatestIn(channel) => {
                let all = LastKnownGoodVersions::fetch(&self.client).await.context(
                    ChromeForTestingManagerError::RequestVersions {
                        version_request: version_selection.clone(),
                    },
                )?;
                all.channel(channel)
                    .cloned()
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
            VersionRequest::Fixed(version) => {
                let all = KnownGoodVersions::fetch(&self.client).await.context(
                    ChromeForTestingManagerError::RequestVersions {
                        version_request: version_selection.clone(),
                    },
                )?;
                all.versions
                    .into_iter()
                    .find(|v| v.version == *version)
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
        };

        let selected = selected.context(ChromeForTestingManagerError::NoMatchingVersion {
            version_request: version_selection,
        })?;

        Ok(selected)
    }

    /// Download Chrome and `ChromeDriver` for `selected` into the cache directory.
    ///
    /// If both binaries already exist on disk this is a no-op and returns the cached paths.
    ///
    /// # Errors
    ///
    /// Returns an error if no platform-matching download exists, the cache directory cannot be
    /// prepared, or the download / extraction fails.
    pub async fn download(
        &self,
        selected: SelectedVersion,
    ) -> Result<LoadedChromePackage, Report<ChromeForTestingManagerError>> {
        let Some(selected_chrome_download) = selected.chrome.clone() else {
            bail!(ChromeForTestingManagerError::NoChromeDownload {
                version: selected.version,
                platform: self.platform,
            });
        };

        let Some(selected_chromedriver_download) = selected.chromedriver.clone() else {
            bail!(ChromeForTestingManagerError::NoChromedriverDownload {
                version: selected.version,
                platform: self.platform,
            });
        };

        // Check if download is necessary.
        let version_dir = self.version_dir(selected.version);
        let platform_dir = version_dir.join(self.platform.to_string());
        fs::create_dir_all(&platform_dir).await.context(
            ChromeForTestingManagerError::CreatePlatformDir {
                platform_dir: platform_dir.clone(),
            },
        )?;

        let chrome_executable = platform_dir.join(self.platform.chrome_executable_path());
        let chromedriver_executable =
            platform_dir.join(self.platform.chromedriver_executable_path());

        let channel_label = selected
            .channel
            .map_or_else(String::new, |ch| ch.to_string());

        // Download chrome if necessary.
        let is_chrome_downloaded = chrome_executable.exists() && chrome_executable.is_file();
        if is_chrome_downloaded {
            tracing::info!(
                "Chrome {} already installed at {chrome_executable:?}...",
                selected.version
            );
        } else {
            tracing::info!("Installing {channel_label} Chrome {}", selected.version);
            download::download_zip(
                &self.client,
                &selected_chrome_download.url,
                &platform_dir,
                &platform_dir,
                ChromeForTestingArtifact::Chrome,
            )
            .await?;
        }

        // Download chromedriver if necessary.
        let is_chromedriver_downloaded =
            chromedriver_executable.exists() && chromedriver_executable.is_file();
        if is_chromedriver_downloaded {
            tracing::info!(
                "Chromedriver {} already installed at {chromedriver_executable:?}...",
                selected.version
            );
        } else {
            tracing::info!(
                "Installing {channel_label} Chromedriver {}",
                selected.version
            );
            download::download_zip(
                &self.client,
                &selected_chromedriver_download.url,
                &platform_dir,
                &platform_dir,
                ChromeForTestingArtifact::ChromeDriver,
            )
            .await?;
        }

        Ok(LoadedChromePackage {
            chrome_executable,
            chromedriver_executable,
        })
    }

    /// Launch a chromedriver process from `loaded` on the requested port.
    ///
    /// Returns the spawned process handle, the actual bound port (relevant when
    /// [`PortRequest::Any`] was used), and the long-lived output inspectors that drive the
    /// optional [`DriverOutputListener`]. Keep the inspectors alive while you want to receive
    /// output lines.
    ///
    /// The returned [`ProcessHandle`] is not auto-terminated; either wrap it with
    /// [`ProcessHandle::terminate_on_drop`] or call its `terminate` method explicitly.
    ///
    /// # Errors
    ///
    /// Returns an error if the chromedriver binary cannot be spawned or does not report
    /// successful startup within 10 seconds.
    ///
    /// # Panics
    ///
    /// Panics if the chromedriver executable path contains non-Unicode bytes.
    pub async fn launch_chromedriver(
        &self,
        loaded: &LoadedChromePackage,
        port: PortRequest,
        output_listener: Option<DriverOutputListener>,
    ) -> Result<
        (
            ProcessHandle<BroadcastOutputStream<ReliableDelivery, ReplayEnabled>>,
            Port,
            DriverOutputInspectors,
        ),
        Report<ChromeForTestingManagerError>,
    > {
        let chromedriver_exe_path_str = loaded
            .chromedriver_executable
            .to_str()
            .expect("valid unicode");

        tracing::info!(
            "Launching chromedriver... {:?}",
            loaded.chromedriver_executable
        );
        let mut command = Command::new(chromedriver_exe_path_str);
        match port {
            PortRequest::Any => {}
            PortRequest::Specific(Port(port)) => {
                command.arg(format!("--port={port}"));
            }
        }
        let loglevel = chrome_for_testing::chromedriver::LogLevel::Info;
        command.arg(format!("--log-level={loglevel}"));

        self.apply_chromedriver_creation_flags(&mut command);

        let mut chromedriver_process = Process::new(command)
            .name("chromedriver")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .context(ChromeForTestingManagerError::SpawnChromedriver {
                path: loaded.chromedriver_executable.clone(),
            })?;

        let output_inspectors =
            DriverOutputInspectors::start(&chromedriver_process, output_listener);

        tracing::info!("Waiting for chromedriver to start...");
        let started_on_port = Arc::new(AtomicU16::new(0));
        let started_on_port_clone = started_on_port.clone();
        let startup_result = chromedriver_process
            .stdout()
            .wait_for_line(
                Duration::from_secs(10),
                move |line| {
                    if line.contains("started successfully on port") {
                        let Some(port) = line
                            .trim()
                            .trim_matches('"')
                            .trim_end_matches('.')
                            .split(' ')
                            .next_back()
                            .and_then(|s| s.parse::<u16>().ok())
                        else {
                            tracing::error!(
                                "Failed to parse port from chromedriver output: {line:?}"
                            );
                            return false;
                        };
                        started_on_port_clone.store(port, std::sync::atomic::Ordering::Release);
                        true
                    } else {
                        false
                    }
                },
                LineParsingOptions::builder()
                    .max_line_length(DEFAULT_MAX_LINE_LENGTH)
                    .overflow_behavior(LineOverflowBehavior::DropAdditionalData)
                    .buffer_compaction_threshold(None)
                    .build(),
            )
            .await
            .context(ChromeForTestingManagerError::WaitForChromedriverStartup {
                path: loaded.chromedriver_executable.clone(),
            })?;
        match startup_result {
            WaitForLineResult::Matched => {}
            WaitForLineResult::StreamClosed | WaitForLineResult::Timeout => {
                if let Err(err) = chromedriver_process
                    .terminate(Duration::from_secs(3), Duration::from_secs(3))
                    .await
                {
                    tracing::warn!(
                        error = %err,
                        "failed to terminate chromedriver after startup failure"
                    );
                }

                return Err(report!(
                    ChromeForTestingManagerError::WaitForChromedriverStartup {
                        path: loaded.chromedriver_executable.clone(),
                    }
                ));
            }
        }

        // It SHOULD definitely be terminated.
        // But the default implementation when "must_be_terminated" raises a panic if not terminated.
        // Our custom `Drop` impl on `Chromedriver` relaxes this and only logs an ERROR instead.
        chromedriver_process.must_not_be_terminated();

        Ok((
            chromedriver_process,
            Port(Arc::into_inner(started_on_port).unwrap().into_inner()),
            output_inspectors,
        ))
    }

    #[cfg(target_os = "windows")]
    fn apply_chromedriver_creation_flags<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        use std::os::windows::process::CommandExt;

        // CREATE_NO_WINDOW (0x08000000) is a Windows-specific process creation flag that prevents
        // a process from creating a new window. This is relevant for ChromeDriver because:
        //   - ChromeDriver is typically a console application on Windows.
        //   - Without this flag, launching ChromeDriver would create a visible console window.
        //   - In our automation scenario, we don't want users to see this console window popping up.
        //   - The window isn't necessary since we're already capturing the stdout/stderr streams programmatically.
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        command.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(target_os = "windows"))]
    #[allow(clippy::unused_self)] // Symmetry with the Windows variant that uses `self`.
    fn apply_chromedriver_creation_flags<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
    }

    /// Prepare a [`thirtyfour::ChromeCapabilities`] pre-wired with the loaded Chrome binary path
    /// and the headless flag set.
    ///
    /// Useful when constructing a [`thirtyfour::WebDriver`] manually instead of using
    /// [`crate::Chromedriver::with_session`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying capability setup fails.
    ///
    /// # Panics
    ///
    /// Panics if the cached Chrome executable path contains non-Unicode bytes.
    #[cfg(feature = "thirtyfour")]
    #[allow(clippy::unused_self)] // Takes &self for API consistency with other methods.
    pub fn prepare_caps(
        &self,
        loaded: &LoadedChromePackage,
    ) -> Result<thirtyfour::ChromeCapabilities, Report<ChromeForTestingManagerError>> {
        use thirtyfour::ChromiumLikeCapabilities;

        tracing::info!(
            "Registering {:?} in capabilities.",
            loaded.chrome_executable
        );
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_headless()
            .context(ChromeForTestingManagerError::PrepareChromeCapabilities {
                chrome_executable: loaded.chrome_executable.clone(),
            })?;
        caps.set_binary(loaded.chrome_executable.to_str().expect("valid unicode"))
            .context(ChromeForTestingManagerError::PrepareChromeCapabilities {
                chrome_executable: loaded.chrome_executable.clone(),
            })?;
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use crate::mgr::ChromeForTestingManager;
    use crate::port::Port;
    use crate::port::PortRequest;
    use crate::{Channel, Version, VersionRequest};
    use assertr::prelude::*;
    use rootcause::Report;
    use serial_test::serial;
    use std::time::Duration;

    #[ctor::ctor(unsafe)]
    fn init_test_tracing() {
        tracing_subscriber::fmt().with_test_writer().try_init().ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clear_cache_and_download_new() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        mgr.clear_cache().await?;
        let selected = mgr
            .resolve_version(VersionRequest::LatestIn(Channel::Stable))
            .await?;
        let loaded = mgr.download(selected).await?;

        assert_that!(loaded.chrome_executable).exists().is_a_file();
        assert_that!(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_latest() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = mgr.download(selected).await?;

        assert_that!(loaded.chrome_executable).exists().is_a_file();
        assert_that!(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_latest_in_stable_channel() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr
            .resolve_version(VersionRequest::LatestIn(Channel::Stable))
            .await?;
        let loaded = mgr.download(selected).await?;

        assert_that!(loaded.chrome_executable).exists().is_a_file();
        assert_that!(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_specific() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr
            .resolve_version(VersionRequest::Fixed(Version {
                major: 135,
                minor: 0,
                patch: 7019,
                build: 0,
            }))
            .await?;
        let loaded = mgr.download(selected).await?;

        assert_that!(loaded.chrome_executable).exists().is_a_file();
        assert_that!(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn launch_chromedriver_on_specific_port() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = mgr.download(selected).await?;
        let (chromedriver, port, _output_inspectors) = mgr
            .launch_chromedriver(&loaded, PortRequest::Specific(Port(3333)), None)
            .await?;
        let _chromedriver =
            chromedriver.terminate_on_drop(Duration::from_secs(3), Duration::from_secs(3));
        assert_that!(port).is_equal_to(Port(3333));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn download_and_launch_chromedriver_on_random_port_and_prepare_thirtyfour_webdriver()
    -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = mgr.download(selected).await?;
        let (chromedriver, port, _output_inspectors) = mgr
            .launch_chromedriver(&loaded, PortRequest::Any, None)
            .await?;
        let _chromedriver =
            chromedriver.terminate_on_drop(Duration::from_secs(3), Duration::from_secs(3));

        let caps = mgr.prepare_caps(&loaded)?;
        let driver = thirtyfour::WebDriver::new(format!("http://localhost:{port}"), caps).await?;
        driver.goto("https://www.google.com").await?;

        let url = driver.current_url().await?;
        assert_that!(url).has_display_value("https://www.google.com/");

        driver.quit().await?;

        Ok(())
    }
}
