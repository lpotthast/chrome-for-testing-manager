use crate::cache::CacheDir;
use crate::download;
use crate::output::{DriverOutputInspectors, DriverOutputListener};
use crate::port::{Port, PortRequest};
use crate::version::{SelectedVersion, VersionRequest};
use crate::{ChromeForTestingArtifact, ChromeForTestingManagerError};
use chrome_for_testing::{KnownGoodVersions, LastKnownGoodVersions, Platform, Version};
use rootcause::{Report, bail, option_ext::OptionExt, prelude::ResultExt, report};
#[cfg(feature = "thirtyfour")]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "thirtyfour")]
use std::sync::Mutex;
use std::sync::atomic::AtomicU16;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio_process_tools::{
    BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_MAX_LINE_LENGTH,
    DEFAULT_READ_CHUNK_SIZE, GracefulShutdown, LineOverflowBehavior, LineParsingOptions,
    NumBytesExt, Process, ProcessHandle, ReliableWithBackpressure, ReplayEnabled,
    WaitForLineResult,
};

type ManagedProcessOutput = BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>;
type ManagedProcessHandle = ProcessHandle<ManagedProcessOutput>;
#[cfg(feature = "thirtyfour")]
type RecentBrowserOutput = Arc<Mutex<VecDeque<String>>>;

#[cfg(feature = "thirtyfour")]
const BROWSER_STARTUP_OUTPUT_LINES: usize = 80;
#[cfg(feature = "thirtyfour")]
const DEFAULT_HEADLESS_SHELL_REMOTE_DEBUGGING_ARG: &str = "--remote-debugging-port=0";

/// Chrome-compatible browser binary to register with `ChromeDriver`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChromeBinary {
    /// The regular Chrome for Testing browser package.
    #[default]
    Chrome,

    /// The Chrome Headless Shell package.
    ChromeHeadlessShell,
}

impl ChromeBinary {
    const fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::ChromeHeadlessShell => "Chrome Headless Shell",
        }
    }

    const fn artifact(self) -> ChromeForTestingArtifact {
        match self {
            Self::Chrome => ChromeForTestingArtifact::Chrome,
            Self::ChromeHeadlessShell => ChromeForTestingArtifact::ChromeHeadlessShell,
        }
    }

    fn executable_path(self, platform: Platform) -> &'static Path {
        match self {
            Self::Chrome => platform.chrome_executable_path(),
            Self::ChromeHeadlessShell => platform.chrome_headless_shell_executable_path(),
        }
    }
}

#[cfg(feature = "thirtyfour")]
#[derive(Debug)]
pub(crate) struct HeadlessShellSession {
    process: tokio_process_tools::TerminateOnDrop<ManagedProcessOutput>,
    debugger_address: String,
    shutdown: GracefulShutdown,
}

#[cfg(feature = "thirtyfour")]
impl HeadlessShellSession {
    pub(crate) fn debugger_address(&self) -> &str {
        &self.debugger_address
    }

    pub(crate) async fn terminate(
        mut self,
    ) -> Result<std::process::ExitStatus, Report<ChromeForTestingManagerError>> {
        self.process.terminate(self.shutdown.clone()).await.context(
            ChromeForTestingManagerError::TerminateBrowser {
                debugger_address: self.debugger_address.clone(),
            },
        )
    }
}

/// A downloaded regular Chrome for Testing package paired with a matching `ChromeDriver`.
///
/// Use this when code specifically needs the full Chrome browser package. APIs that can operate
/// on either regular Chrome or Chrome Headless Shell use [`LoadedBrowserPackage`] instead.
#[derive(Debug, Clone)]
pub struct LoadedChromePackage {
    chrome_executable: PathBuf,
    chromedriver_executable: PathBuf,
}

impl LoadedChromePackage {
    fn new(chrome_executable: PathBuf, chromedriver_executable: PathBuf) -> Self {
        Self {
            chrome_executable,
            chromedriver_executable,
        }
    }

    /// Path to the cached regular Chrome executable.
    #[must_use]
    pub fn chrome_executable(&self) -> &Path {
        &self.chrome_executable
    }

    /// Path to the cached `ChromeDriver` executable.
    #[must_use]
    pub fn chromedriver_executable(&self) -> &Path {
        &self.chromedriver_executable
    }
}

/// A downloaded Chrome Headless Shell package paired with a matching `ChromeDriver`.
///
/// Use this when code specifically needs the headless-shell package. APIs that can operate on
/// either regular Chrome or Chrome Headless Shell use [`LoadedBrowserPackage`] instead.
#[derive(Debug, Clone)]
pub struct LoadedChromeHeadlessShellPackage {
    chrome_headless_shell_executable: PathBuf,
    chromedriver_executable: PathBuf,
}

impl LoadedChromeHeadlessShellPackage {
    fn new(chrome_headless_shell_executable: PathBuf, chromedriver_executable: PathBuf) -> Self {
        Self {
            chrome_headless_shell_executable,
            chromedriver_executable,
        }
    }

    /// Path to the cached Chrome Headless Shell executable.
    #[must_use]
    pub fn chrome_headless_shell_executable(&self) -> &Path {
        &self.chrome_headless_shell_executable
    }

    /// Path to the cached `ChromeDriver` executable.
    #[must_use]
    pub fn chromedriver_executable(&self) -> &Path {
        &self.chromedriver_executable
    }
}

/// A downloaded Chrome-compatible browser package paired with a matching `ChromeDriver`.
///
/// Returned by [`ChromeForTestingManager::download`]. Match on the enum when behavior differs
/// between regular Chrome and Chrome Headless Shell, or use [`Self::browser_executable`] and
/// [`Self::chromedriver_executable`] for behavior shared by both browser packages.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LoadedBrowserPackage {
    /// Regular Chrome for Testing.
    Chrome(LoadedChromePackage),

    /// Chrome Headless Shell.
    ChromeHeadlessShell(LoadedChromeHeadlessShellPackage),
}

impl LoadedBrowserPackage {
    fn new(
        chrome_binary: ChromeBinary,
        browser_executable: PathBuf,
        chromedriver_executable: PathBuf,
    ) -> Self {
        match chrome_binary {
            ChromeBinary::Chrome => Self::Chrome(LoadedChromePackage::new(
                browser_executable,
                chromedriver_executable,
            )),
            ChromeBinary::ChromeHeadlessShell => Self::ChromeHeadlessShell(
                LoadedChromeHeadlessShellPackage::new(browser_executable, chromedriver_executable),
            ),
        }
    }

    /// The Chrome-compatible browser binary selected for this package.
    #[must_use]
    pub const fn chrome_binary(&self) -> ChromeBinary {
        match self {
            Self::Chrome(_) => ChromeBinary::Chrome,
            Self::ChromeHeadlessShell(_) => ChromeBinary::ChromeHeadlessShell,
        }
    }

    /// Path to the cached browser executable.
    #[must_use]
    pub fn browser_executable(&self) -> &Path {
        match self {
            Self::Chrome(package) => package.chrome_executable(),
            Self::ChromeHeadlessShell(package) => package.chrome_headless_shell_executable(),
        }
    }

    /// Path to the cached `ChromeDriver` executable.
    #[must_use]
    pub fn chromedriver_executable(&self) -> &Path {
        match self {
            Self::Chrome(package) => package.chromedriver_executable(),
            Self::ChromeHeadlessShell(package) => package.chromedriver_executable(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestedChromeBinaries {
    chrome: bool,
    chrome_headless_shell: bool,
}

impl RequestedChromeBinaries {
    const fn single(chrome_binary: ChromeBinary) -> Self {
        match chrome_binary {
            ChromeBinary::Chrome => Self {
                chrome: true,
                chrome_headless_shell: false,
            },
            ChromeBinary::ChromeHeadlessShell => Self {
                chrome: false,
                chrome_headless_shell: true,
            },
        }
    }

    fn from_slice(
        chrome_binaries: &[ChromeBinary],
    ) -> Result<Self, Report<ChromeForTestingManagerError>> {
        if chrome_binaries.is_empty() {
            bail!(ChromeForTestingManagerError::EmptyChromeBinaryDownloadRequest);
        }

        Ok(Self {
            chrome: chrome_binaries.contains(&ChromeBinary::Chrome),
            chrome_headless_shell: chrome_binaries.contains(&ChromeBinary::ChromeHeadlessShell),
        })
    }
}

#[derive(Debug)]
struct DownloadedBrowserArtifacts {
    chromedriver: PathBuf,
    chrome: Option<PathBuf>,
    chrome_headless_shell: Option<PathBuf>,
}

impl DownloadedBrowserArtifacts {
    fn package_for(
        &self,
        chrome_binary: ChromeBinary,
        version: Version,
        platform: Platform,
    ) -> Result<LoadedBrowserPackage, Report<ChromeForTestingManagerError>> {
        let browser_executable = self
            .browser_executable(chrome_binary, version, platform)?
            .clone();

        Ok(LoadedBrowserPackage::new(
            chrome_binary,
            browser_executable,
            self.chromedriver.clone(),
        ))
    }

    fn browser_executable(
        &self,
        chrome_binary: ChromeBinary,
        version: Version,
        platform: Platform,
    ) -> Result<&PathBuf, Report<ChromeForTestingManagerError>> {
        match chrome_binary {
            ChromeBinary::Chrome => self.chrome.as_ref().ok_or_else(|| {
                report!(ChromeForTestingManagerError::NoChromeDownload { version, platform })
            }),
            ChromeBinary::ChromeHeadlessShell => {
                self.chrome_headless_shell.as_ref().ok_or_else(|| {
                    report!(
                        ChromeForTestingManagerError::NoChromeHeadlessShellDownload {
                            version,
                            platform,
                        }
                    )
                })
            }
        }
    }
}

/// Lower-level orchestrator for chrome-for-testing artifacts.
///
/// Most users should use [`crate::Chromedriver`], which wraps this manager with sensible defaults
/// and handles process lifecycle automatically. Reach for `ChromeForTestingManager` directly when
/// you need finer control:
///
/// - **Pre-warm a cache** without spawning chromedriver: call [`Self::resolve_version`] and
///   [`Self::download`] with the [`ChromeBinary`] values you need, then drop the result.
/// - **Run multiple chromedriver instances** off a single resolved version: call
///   [`Self::launch_chromedriver`] repeatedly with the same [`LoadedBrowserPackage`].
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
            platform: Platform::detect().map_err(unsupported_platform_error)?,
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
            platform: Platform::detect().map_err(unsupported_platform_error)?,
        })
    }

    fn version_dir(&self, version: Version) -> PathBuf {
        self.cache_dir.path().join(version.to_string())
    }

    fn platform_dir(&self, version: Version) -> PathBuf {
        self.version_dir(version).join(self.platform.to_string())
    }

    async fn ensure_platform_dir(
        &self,
        version: Version,
    ) -> Result<PathBuf, Report<ChromeForTestingManagerError>> {
        let platform_dir = self.platform_dir(version);
        fs::create_dir_all(&platform_dir).await.context(
            ChromeForTestingManagerError::CreatePlatformDir {
                platform_dir: platform_dir.clone(),
            },
        )?;
        Ok(platform_dir)
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
                let all = KnownGoodVersions::fetch(&self.client)
                    .await
                    .map_err(|err| request_versions_error(err, &version_selection))?;
                all.versions
                    .iter()
                    .filter(|v| v.downloads.chromedriver.is_some())
                    .max_by_key(|v| v.version)
                    .cloned()
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
            VersionRequest::LatestIn(channel) => {
                let all = LastKnownGoodVersions::fetch(&self.client)
                    .await
                    .map_err(|err| request_versions_error(err, &version_selection))?;
                all.channel(channel)
                    .cloned()
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
            VersionRequest::Fixed(version) => {
                let all = KnownGoodVersions::fetch(&self.client)
                    .await
                    .map_err(|err| request_versions_error(err, &version_selection))?;
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

    /// Download the requested browser artifact(s) and matching `ChromeDriver`.
    ///
    /// Returns one [`LoadedBrowserPackage`] per requested [`ChromeBinary`], in request order. The
    /// download work is de-duplicated, so repeated binary values do not download the same artifact
    /// more than once.
    ///
    /// # Errors
    ///
    /// Returns an error if `chrome_binaries` is empty, no platform-matching browser or
    /// `ChromeDriver` download exists, the cache directory cannot be prepared, or the download /
    /// extraction fails.
    pub async fn download(
        &self,
        selected: &SelectedVersion,
        chrome_binaries: &[ChromeBinary],
    ) -> Result<Vec<LoadedBrowserPackage>, Report<ChromeForTestingManagerError>> {
        let requested = RequestedChromeBinaries::from_slice(chrome_binaries)?;
        let artifacts = self
            .download_requested_artifacts(selected, requested)
            .await?;
        let mut loaded = Vec::with_capacity(chrome_binaries.len());
        for chrome_binary in chrome_binaries {
            loaded.push(artifacts.package_for(*chrome_binary, selected.version, self.platform)?);
        }

        Ok(loaded)
    }

    pub(crate) async fn download_one(
        &self,
        selected: &SelectedVersion,
        chrome_binary: ChromeBinary,
    ) -> Result<LoadedBrowserPackage, Report<ChromeForTestingManagerError>> {
        let artifacts = self
            .download_requested_artifacts(selected, RequestedChromeBinaries::single(chrome_binary))
            .await?;
        artifacts.package_for(chrome_binary, selected.version, self.platform)
    }

    async fn download_requested_artifacts(
        &self,
        selected: &SelectedVersion,
        requested: RequestedChromeBinaries,
    ) -> Result<DownloadedBrowserArtifacts, Report<ChromeForTestingManagerError>> {
        let platform_dir = self.ensure_platform_dir(selected.version).await?;

        let (chromedriver, chrome, chrome_headless_shell) = tokio::try_join!(
            self.download_chromedriver(selected, &platform_dir),
            self.download_requested_browser(
                selected,
                &platform_dir,
                ChromeBinary::Chrome,
                requested.chrome,
            ),
            self.download_requested_browser(
                selected,
                &platform_dir,
                ChromeBinary::ChromeHeadlessShell,
                requested.chrome_headless_shell,
            ),
        )?;

        Ok(DownloadedBrowserArtifacts {
            chromedriver,
            chrome,
            chrome_headless_shell,
        })
    }

    async fn download_requested_browser(
        &self,
        selected: &SelectedVersion,
        platform_dir: &Path,
        chrome_binary: ChromeBinary,
        is_requested: bool,
    ) -> Result<Option<PathBuf>, Report<ChromeForTestingManagerError>> {
        if is_requested {
            self.download_browser(selected, platform_dir, chrome_binary)
                .await
                .map(Some)
        } else {
            Ok(None)
        }
    }

    async fn download_browser(
        &self,
        selected: &SelectedVersion,
        platform_dir: &Path,
        chrome_binary: ChromeBinary,
    ) -> Result<PathBuf, Report<ChromeForTestingManagerError>> {
        let selected_chrome_download = match chrome_binary {
            ChromeBinary::Chrome => selected.chrome.clone().ok_or_else(|| {
                report!(ChromeForTestingManagerError::NoChromeDownload {
                    version: selected.version,
                    platform: self.platform,
                })
            })?,
            ChromeBinary::ChromeHeadlessShell => {
                selected.chrome_headless_shell.clone().ok_or_else(|| {
                    report!(
                        ChromeForTestingManagerError::NoChromeHeadlessShellDownload {
                            version: selected.version,
                            platform: self.platform,
                        }
                    )
                })?
            }
        };

        let chrome_executable = platform_dir.join(chrome_binary.executable_path(self.platform));
        self.ensure_artifact_downloaded(
            selected,
            platform_dir,
            &chrome_executable,
            chrome_binary.artifact(),
            chrome_binary.label(),
            &selected_chrome_download.url,
        )
        .await?;

        Ok(chrome_executable)
    }

    async fn download_chromedriver(
        &self,
        selected: &SelectedVersion,
        platform_dir: &Path,
    ) -> Result<PathBuf, Report<ChromeForTestingManagerError>> {
        let Some(selected_chromedriver_download) = selected.chromedriver.clone() else {
            bail!(ChromeForTestingManagerError::NoChromedriverDownload {
                version: selected.version,
                platform: self.platform,
            });
        };

        let chromedriver_executable =
            platform_dir.join(self.platform.chromedriver_executable_path());
        self.ensure_artifact_downloaded(
            selected,
            platform_dir,
            &chromedriver_executable,
            ChromeForTestingArtifact::ChromeDriver,
            "Chromedriver",
            &selected_chromedriver_download.url,
        )
        .await?;

        Ok(chromedriver_executable)
    }

    async fn ensure_artifact_downloaded(
        &self,
        selected: &SelectedVersion,
        platform_dir: &Path,
        executable: &Path,
        artifact: ChromeForTestingArtifact,
        label: &str,
        url: &str,
    ) -> Result<(), Report<ChromeForTestingManagerError>> {
        let channel_label = selected
            .channel
            .as_ref()
            .map_or_else(String::new, ToString::to_string);

        if executable.exists() && executable.is_file() {
            tracing::info!(
                "{label} {} already installed at {executable:?}...",
                selected.version
            );
        } else {
            tracing::info!("Installing {channel_label} {label} {}", selected.version);
            download::download_zip(&self.client, url, platform_dir, platform_dir, artifact).await?;
        }

        Ok(())
    }

    /// Launch a chromedriver process from `loaded` on the requested port.
    ///
    /// Returns the spawned process handle, the actual bound port (relevant when
    /// [`PortRequest::Any`] was used), and the long-lived output inspectors that drive the
    /// optional [`DriverOutputListener`]. Keep the inspectors alive while you want to receive
    /// output lines.
    ///
    /// The returned [`ProcessHandle`] is not auto-terminated. Either wrap it with
    /// [`ProcessHandle::terminate_on_drop`] or call its `terminate` method explicitly. The
    /// `shutdown` argument is only used for the internal cleanup path that fires when
    /// chromedriver fails to report successful startup. Pass the same value you intend to use
    /// for graceful shutdown so a startup failure honors your tuned budget.
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
        loaded: &LoadedBrowserPackage,
        port: PortRequest,
        output_listener: Option<DriverOutputListener>,
        shutdown: GracefulShutdown,
    ) -> Result<
        (ManagedProcessHandle, Port, DriverOutputInspectors),
        Report<ChromeForTestingManagerError>,
    > {
        let chromedriver_executable = loaded.chromedriver_executable();
        let chromedriver_exe_path_str = chromedriver_executable.to_str().expect("valid unicode");

        tracing::info!("Launching chromedriver... {chromedriver_executable:?}");
        let mut command = Command::new(chromedriver_exe_path_str);
        match port {
            PortRequest::Any => {}
            PortRequest::Specific(port) => {
                command.arg(format!("--port={}", port.as_u16()));
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
                    .reliable_with_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .context(ChromeForTestingManagerError::SpawnChromedriver {
                path: chromedriver_executable.to_path_buf(),
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
                path: chromedriver_executable.to_path_buf(),
            })?;
        match startup_result {
            WaitForLineResult::Matched => {}
            WaitForLineResult::StreamClosed | WaitForLineResult::Timeout => {
                if let Err(err) = chromedriver_process.terminate(shutdown).await {
                    tracing::warn!(
                        error = %err,
                        "failed to terminate chromedriver after startup failure"
                    );
                }

                return Err(report!(
                    ChromeForTestingManagerError::WaitForChromedriverStartup {
                        path: chromedriver_executable.to_path_buf(),
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
            Port::new(Arc::into_inner(started_on_port).unwrap().into_inner()),
            output_inspectors,
        ))
    }

    /// Launch Chrome Headless Shell for a single attached `WebDriver` session.
    ///
    /// Headless Shell starts with no pages when `ChromeDriver` launches it directly. For that
    /// binary, start the browser first, create an initial blank page through `DevTools`, and let
    /// `ChromeDriver` attach via `debuggerAddress`.
    #[cfg(feature = "thirtyfour")]
    pub(crate) async fn launch_headless_shell_session(
        &self,
        loaded: &LoadedChromeHeadlessShellPackage,
        caps: &thirtyfour::ChromeCapabilities,
        shutdown: GracefulShutdown,
    ) -> Result<HeadlessShellSession, Report<ChromeForTestingManagerError>> {
        let chrome_headless_shell_executable =
            loaded.chrome_headless_shell_executable().to_path_buf();
        let chrome_headless_shell_executable_str = chrome_headless_shell_executable
            .to_str()
            .expect("valid unicode");
        tracing::info!("Launching Chrome Headless Shell... {chrome_headless_shell_executable:?}");

        let mut command = Command::new(chrome_headless_shell_executable_str);
        command.args(headless_shell_launch_args(caps)?);

        let mut browser_process = Process::new(command)
            .name("chrome-headless-shell")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .reliable_with_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .context(ChromeForTestingManagerError::SpawnBrowser {
                path: chrome_headless_shell_executable.clone(),
            })?;

        let debugger_address = match wait_for_devtools_address(
            &mut browser_process,
            &chrome_headless_shell_executable,
        )
        .await
        {
            Ok(debugger_address) => debugger_address,
            Err(err) => {
                terminate_browser_after_startup_failure(&mut browser_process, shutdown).await;
                return Err(err);
            }
        };

        if let Err(err) = self.create_initial_browser_page(&debugger_address).await {
            terminate_browser_after_startup_failure(&mut browser_process, shutdown).await;
            return Err(err);
        }

        Ok(HeadlessShellSession {
            process: browser_process.terminate_on_drop(shutdown.clone()),
            debugger_address,
            shutdown,
        })
    }

    #[cfg(target_os = "windows")]
    #[expect(clippy::unused_self)]
    fn apply_chromedriver_creation_flags<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        // CREATE_NO_WINDOW (0x08000000) is a Windows-specific process creation flag that prevents
        // a process from creating a new window. This is relevant for ChromeDriver because:
        //   - ChromeDriver is typically a console application on Windows.
        //   - Without this flag, launching ChromeDriver would create a visible console window.
        //   - In our automation scenario, we don't want users to see this console window popping up.
        //   - The window isn't necessary since we're already capturing the stdout/stderr streams programmatically.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        command.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(target_os = "windows"))]
    #[expect(clippy::unused_self)]
    fn apply_chromedriver_creation_flags<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
    }

    /// Prepare a [`thirtyfour::ChromeCapabilities`] pre-wired with the loaded Chrome binary path
    /// and the headless flag set.
    ///
    /// Useful when constructing a [`thirtyfour::WebDriver`] manually instead of using
    /// [`crate::Chromedriver::session`].
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
        loaded: &LoadedBrowserPackage,
    ) -> Result<thirtyfour::ChromeCapabilities, Report<ChromeForTestingManagerError>> {
        use thirtyfour::ChromiumLikeCapabilities;

        let browser_executable = loaded.browser_executable();
        tracing::debug!("Registering {browser_executable:?} in capabilities.");
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_headless()
            .context(ChromeForTestingManagerError::PrepareChromeCapabilities {
                browser_executable: browser_executable.to_path_buf(),
            })?;
        caps.set_binary(browser_executable.to_str().expect("valid unicode"))
            .context(ChromeForTestingManagerError::PrepareChromeCapabilities {
                browser_executable: browser_executable.to_path_buf(),
            })?;
        Ok(caps)
    }
}

#[cfg(feature = "thirtyfour")]
fn headless_shell_launch_args(
    caps: &thirtyfour::ChromeCapabilities,
) -> Result<Vec<String>, Report<ChromeForTestingManagerError>> {
    use thirtyfour::BrowserCapabilitiesHelper;

    let mut launch_args = Vec::new();
    let mut remote_debugging_port_arg = None::<String>;

    for arg in caps.args() {
        match classify_remote_debugging_arg(&arg) {
            Some(RemoteDebuggingArg::Pipe) => {
                return Err(report!(
                    ChromeForTestingManagerError::UnsupportedHeadlessShellRemoteDebuggingArg {
                        arg,
                    }
                ));
            }
            Some(RemoteDebuggingArg::InvalidPort) => {
                return Err(report!(
                    ChromeForTestingManagerError::InvalidHeadlessShellRemoteDebuggingPortArg {
                        arg,
                    }
                ));
            }
            Some(RemoteDebuggingArg::Port) => {
                if let Some(first_arg) = &remote_debugging_port_arg {
                    return Err(report!(
                        ChromeForTestingManagerError::ConflictingHeadlessShellRemoteDebuggingArgs {
                            first_arg: first_arg.clone(),
                            second_arg: arg,
                        }
                    ));
                }
                remote_debugging_port_arg = Some(arg);
            }
            None => launch_args.push(arg),
        }
    }

    launch_args.push(
        remote_debugging_port_arg
            .unwrap_or_else(|| DEFAULT_HEADLESS_SHELL_REMOTE_DEBUGGING_ARG.to_owned()),
    );
    Ok(launch_args)
}

#[cfg(feature = "thirtyfour")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDebuggingArg {
    Pipe,
    Port,
    InvalidPort,
}

#[cfg(feature = "thirtyfour")]
fn classify_remote_debugging_arg(arg: &str) -> Option<RemoteDebuggingArg> {
    if arg == "--remote-debugging-pipe" || arg.starts_with("--remote-debugging-pipe=") {
        return Some(RemoteDebuggingArg::Pipe);
    }

    if arg == "--remote-debugging-port" {
        return Some(RemoteDebuggingArg::InvalidPort);
    }

    let port = arg.strip_prefix("--remote-debugging-port=")?;

    if port.parse::<u16>().is_ok() {
        Some(RemoteDebuggingArg::Port)
    } else {
        Some(RemoteDebuggingArg::InvalidPort)
    }
}

#[cfg(feature = "thirtyfour")]
async fn wait_for_devtools_address(
    browser_process: &mut ManagedProcessHandle,
    chrome_executable: &Path,
) -> Result<String, Report<ChromeForTestingManagerError>> {
    let debugger_address = Arc::new(Mutex::new(None::<String>));
    let debugger_address_for_wait = debugger_address.clone();
    let recent_output = Arc::new(Mutex::new(VecDeque::new()));
    let recent_output_for_wait = recent_output.clone();
    let startup_result = match browser_process
        .stderr()
        .wait_for_line(
            Duration::from_secs(10),
            move |line| {
                push_recent_browser_output(&recent_output_for_wait, line.as_ref());
                let Some(address) = parse_devtools_address(&line) else {
                    return false;
                };
                let mut stored = debugger_address_for_wait.lock().expect("not poisoned");
                *stored = Some(address);
                true
            },
            LineParsingOptions::builder()
                .max_line_length(DEFAULT_MAX_LINE_LENGTH)
                .overflow_behavior(LineOverflowBehavior::DropAdditionalData)
                .buffer_compaction_threshold(None)
                .build(),
        )
        .await
    {
        Ok(result) => result,
        Err(err) => {
            log_recent_browser_startup_output(chrome_executable, &recent_output);
            return Err(Report::new_sendsync(err).context(
                ChromeForTestingManagerError::WaitForBrowserStartup {
                    path: chrome_executable.to_path_buf(),
                },
            ));
        }
    };

    match startup_result {
        WaitForLineResult::Matched => debugger_address
            .lock()
            .expect("not poisoned")
            .clone()
            .context(ChromeForTestingManagerError::WaitForBrowserStartup {
                path: chrome_executable.to_path_buf(),
            }),
        WaitForLineResult::StreamClosed | WaitForLineResult::Timeout => {
            log_recent_browser_startup_output(chrome_executable, &recent_output);
            Err(report!(
                ChromeForTestingManagerError::WaitForBrowserStartup {
                    path: chrome_executable.to_path_buf(),
                }
            ))
        }
    }
}

#[cfg(feature = "thirtyfour")]
fn push_recent_browser_output(recent_output: &RecentBrowserOutput, line: &str) {
    let mut recent_output = recent_output.lock().expect("not poisoned");
    if recent_output.len() == BROWSER_STARTUP_OUTPUT_LINES {
        recent_output.pop_front();
    }
    recent_output.push_back(line.to_owned());
}

#[cfg(feature = "thirtyfour")]
fn log_recent_browser_startup_output(
    chrome_executable: &Path,
    recent_output: &RecentBrowserOutput,
) {
    let recent_output = recent_output.lock().expect("not poisoned");
    if recent_output.is_empty() {
        tracing::error!(
            path = %chrome_executable.display(),
            "Chrome Headless Shell exited before DevTools startup and produced no captured stderr"
        );
        return;
    }

    tracing::error!(
        path = %chrome_executable.display(),
        "Chrome Headless Shell startup output before DevTools startup failed:\n{}",
        recent_output
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[cfg(feature = "thirtyfour")]
fn parse_devtools_address(line: &str) -> Option<String> {
    let (_, after_prefix) = line.split_once("DevTools listening on ws://")?;
    let (address, _) = after_prefix.split_once('/')?;
    if address.is_empty() {
        None
    } else {
        Some(address.to_owned())
    }
}

#[cfg(feature = "thirtyfour")]
async fn terminate_browser_after_startup_failure(
    browser_process: &mut ManagedProcessHandle,
    shutdown: GracefulShutdown,
) {
    if let Err(err) = browser_process.terminate(shutdown).await {
        tracing::warn!(
            error = %err,
            "failed to terminate browser after startup failure"
        );
    }
}

#[cfg(feature = "thirtyfour")]
impl ChromeForTestingManager {
    async fn create_initial_browser_page(
        &self,
        debugger_address: &str,
    ) -> Result<(), Report<ChromeForTestingManagerError>> {
        self.client
            .put(format!("http://{debugger_address}/json/new?about:blank"))
            .send()
            .await
            .context(ChromeForTestingManagerError::CreateInitialBrowserPage {
                debugger_address: debugger_address.to_owned(),
            })?
            .error_for_status()
            .context(ChromeForTestingManagerError::CreateInitialBrowserPage {
                debugger_address: debugger_address.to_owned(),
            })?;
        Ok(())
    }
}

fn unsupported_platform_error(err: impl std::fmt::Display) -> Report<ChromeForTestingManagerError> {
    report!(ChromeForTestingManagerError::UnsupportedPlatform)
        .attach(format!("chrome-for-testing error:\n{err}"))
}

fn request_versions_error(
    err: impl std::fmt::Display,
    version_request: &VersionRequest,
) -> Report<ChromeForTestingManagerError> {
    report!(ChromeForTestingManagerError::RequestVersions {
        version_request: version_request.clone(),
    })
    .attach(format!("chrome-for-testing error:\n{err}"))
}

#[cfg(test)]
mod tests {
    use crate::chromedriver::default_graceful_shutdown;
    use crate::mgr::{ChromeBinary, ChromeForTestingManager, LoadedBrowserPackage};
    use crate::mgr::{
        DEFAULT_HEADLESS_SHELL_REMOTE_DEBUGGING_ARG, RemoteDebuggingArg,
        classify_remote_debugging_arg, headless_shell_launch_args, parse_devtools_address,
    };
    use crate::port::Port;
    use crate::port::PortRequest;
    use crate::version::SelectedVersion;
    use crate::{Channel, Version, VersionRequest};
    use assertr::prelude::*;
    use rootcause::Report;
    use serial_test::serial;
    use std::path::{Path, PathBuf};
    use thirtyfour::ChromiumLikeCapabilities;

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
        let loaded = download_regular_chrome(&mgr, selected).await?;

        assert_that!(loaded.browser_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chromedriver_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chrome_binary()).is_equal_to(ChromeBinary::Chrome);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_latest() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = download_regular_chrome(&mgr, selected).await?;

        assert_that!(loaded.browser_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chromedriver_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chrome_binary()).is_equal_to(ChromeBinary::Chrome);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_latest_in_stable_channel() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr
            .resolve_version(VersionRequest::LatestIn(Channel::Stable))
            .await?;
        let loaded = download_regular_chrome(&mgr, selected).await?;

        assert_that!(loaded.browser_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chromedriver_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chrome_binary()).is_equal_to(ChromeBinary::Chrome);
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
        let loaded = download_regular_chrome(&mgr, selected).await?;

        assert_that!(loaded.browser_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chromedriver_executable())
            .exists()
            .is_a_file();
        assert_that!(loaded.chrome_binary()).is_equal_to(ChromeBinary::Chrome);
        Ok(())
    }

    #[test]
    fn loaded_browser_package_preserves_browser_type_and_paths() {
        let chrome_package = LoadedBrowserPackage::new(
            ChromeBinary::Chrome,
            PathBuf::from("/cache/chrome"),
            PathBuf::from("/cache/chromedriver"),
        );
        assert_that!(chrome_package.chrome_binary()).is_equal_to(ChromeBinary::Chrome);
        assert_that!(chrome_package.browser_executable()).is_equal_to(Path::new("/cache/chrome"));
        assert_that!(chrome_package.chromedriver_executable())
            .is_equal_to(Path::new("/cache/chromedriver"));
        assert_that!(matches!(chrome_package, LoadedBrowserPackage::Chrome(_))).is_true();

        let headless_shell_package = LoadedBrowserPackage::new(
            ChromeBinary::ChromeHeadlessShell,
            PathBuf::from("/cache/chrome-headless-shell"),
            PathBuf::from("/cache/chromedriver"),
        );
        assert_that!(headless_shell_package.chrome_binary())
            .is_equal_to(ChromeBinary::ChromeHeadlessShell);
        assert_that!(headless_shell_package.browser_executable())
            .is_equal_to(Path::new("/cache/chrome-headless-shell"));
        assert_that!(headless_shell_package.chromedriver_executable())
            .is_equal_to(Path::new("/cache/chromedriver"));
        assert_that!(matches!(
            headless_shell_package,
            LoadedBrowserPackage::ChromeHeadlessShell(_)
        ))
        .is_true();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_reports_missing_chrome_binary() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;

        assert_that!(
            mgr.download(&selected_without_downloads(), &[ChromeBinary::Chrome])
                .await
        )
        .is_err();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_reports_missing_chrome_headless_shell_binary() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;

        assert_that!(
            mgr.download(
                &selected_without_downloads(),
                &[ChromeBinary::ChromeHeadlessShell]
            )
            .await
        )
        .is_err();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_reports_missing_binary_for_combined_request() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;

        assert_that!(
            mgr.download(
                &selected_without_downloads(),
                &[ChromeBinary::Chrome, ChromeBinary::ChromeHeadlessShell]
            )
            .await
        )
        .is_err();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_reports_empty_browser_request_before_downloading() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let result = mgr.download(&selected_without_downloads(), &[]).await;

        assert_that!(result)
            .is_err()
            .derive(ToString::to_string)
            .contains("at least one Chrome binary must be requested");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn launch_chromedriver_on_specific_port() -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = download_regular_chrome(&mgr, selected).await?;
        let (chromedriver, port, _output_inspectors) = mgr
            .launch_chromedriver(
                &loaded,
                PortRequest::Specific(Port::new(3333)),
                None,
                default_graceful_shutdown(),
            )
            .await?;
        let _chromedriver = chromedriver.terminate_on_drop(default_graceful_shutdown());
        assert_that!(port).is_equal_to(Port::new(3333));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn download_and_launch_chromedriver_on_random_port_and_prepare_thirtyfour_webdriver()
    -> Result<(), Report> {
        let mgr = ChromeForTestingManager::new()?;
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = download_regular_chrome(&mgr, selected).await?;
        let (chromedriver, port, _output_inspectors) = mgr
            .launch_chromedriver(&loaded, PortRequest::Any, None, default_graceful_shutdown())
            .await?;
        let _chromedriver = chromedriver.terminate_on_drop(default_graceful_shutdown());

        let caps = mgr.prepare_caps(&loaded)?;
        let driver = thirtyfour::WebDriver::new(format!("http://localhost:{port}"), caps).await?;
        driver.goto("https://www.google.com").await?;

        let url = driver.current_url().await?;
        assert_that!(url).has_display_value("https://www.google.com/");

        driver.quit().await?;

        Ok(())
    }

    #[test]
    fn parse_devtools_address_extracts_http_debugger_address() {
        assert_that!(parse_devtools_address(
            "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc"
        ))
        .is_equal_to(Some(String::from("127.0.0.1:9222")));
    }

    #[test]
    fn remote_debugging_args_are_classified_for_headless_shell_sessions() {
        assert_that!(classify_remote_debugging_arg("--remote-debugging-pipe"))
            .is_equal_to(Some(RemoteDebuggingArg::Pipe));
        assert_that!(classify_remote_debugging_arg(
            "--remote-debugging-pipe=true"
        ))
        .is_equal_to(Some(RemoteDebuggingArg::Pipe));
        assert_that!(classify_remote_debugging_arg("--remote-debugging-port"))
            .is_equal_to(Some(RemoteDebuggingArg::InvalidPort));
        assert_that!(classify_remote_debugging_arg("--remote-debugging-port=0"))
            .is_equal_to(Some(RemoteDebuggingArg::Port));
        assert_that!(classify_remote_debugging_arg(
            "--remote-debugging-port=9222"
        ))
        .is_equal_to(Some(RemoteDebuggingArg::Port));
        assert_that!(classify_remote_debugging_arg("--remote-debugging-port="))
            .is_equal_to(Some(RemoteDebuggingArg::InvalidPort));
        assert_that!(classify_remote_debugging_arg(
            "--remote-debugging-port=localhost:9222"
        ))
        .is_equal_to(Some(RemoteDebuggingArg::InvalidPort));
        assert_that!(classify_remote_debugging_arg("--remote-debugging-portable"))
            .is_equal_to(None);
        assert_that!(classify_remote_debugging_arg("--headless")).is_equal_to(None);
    }

    #[test]
    fn headless_shell_launch_args_add_default_remote_debugging_port() -> Result<(), Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--headless=new")?;
        caps.add_arg("--disable-gpu")?;

        assert_that!(headless_shell_launch_args(&caps)?.as_slice()).contains_exactly([
            "--headless=new",
            "--disable-gpu",
            DEFAULT_HEADLESS_SHELL_REMOTE_DEBUGGING_ARG,
        ]);
        Ok(())
    }

    #[test]
    fn headless_shell_launch_args_use_configured_remote_debugging_port() -> Result<(), Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--headless=new")?;
        caps.add_arg("--remote-debugging-port=9222")?;

        assert_that!(headless_shell_launch_args(&caps)?.as_slice())
            .contains_exactly(["--headless=new", "--remote-debugging-port=9222"]);
        Ok(())
    }

    #[test]
    fn headless_shell_launch_args_reject_remote_debugging_pipe() -> Result<(), Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--remote-debugging-pipe")?;

        assert_that!(headless_shell_launch_args(&caps))
            .is_err()
            .derive(ToString::to_string)
            .contains("unsupported argument \"--remote-debugging-pipe\"");
        Ok(())
    }

    #[test]
    fn headless_shell_launch_args_reject_invalid_remote_debugging_port() -> Result<(), Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--remote-debugging-port")?;

        assert_that!(headless_shell_launch_args(&caps))
            .is_err()
            .derive(ToString::to_string)
            .contains("invalid argument \"--remote-debugging-port\"");
        Ok(())
    }

    #[test]
    fn headless_shell_launch_args_reject_conflicting_remote_debugging_ports() -> Result<(), Report>
    {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--remote-debugging-port=9222")?;
        caps.add_arg("--remote-debugging-port=9223")?;

        assert_that!(headless_shell_launch_args(&caps))
            .is_err()
            .derive(ToString::to_string)
            .contains(
                "conflicting arguments \"--remote-debugging-port=9222\" and \"--remote-debugging-port=9223\"",
            );
        Ok(())
    }

    fn selected_without_downloads() -> SelectedVersion {
        SelectedVersion {
            channel: None,
            version: Version {
                major: 135,
                minor: 0,
                patch: 7019,
                build: 0,
            },
            chrome: None,
            chrome_headless_shell: None,
            chromedriver: None,
        }
    }

    async fn download_regular_chrome(
        mgr: &ChromeForTestingManager,
        selected: SelectedVersion,
    ) -> Result<LoadedBrowserPackage, Report> {
        let loaded = mgr.download(&selected, &[ChromeBinary::Chrome]).await?;
        Ok(loaded
            .into_iter()
            .next()
            .expect("one requested binary returns one package"))
    }
}
