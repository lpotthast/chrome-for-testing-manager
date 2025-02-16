use crate::cache::CacheDir;
use crate::download;
use crate::port::{Port, PortRequest};
use anyhow::Context;
use chrome_for_testing::api::channel::Channel;
use chrome_for_testing::api::known_good_versions::VersionWithoutChannel;
use chrome_for_testing::api::last_known_good_versions::VersionInChannel;
use chrome_for_testing::api::platform::Platform;
use chrome_for_testing::api::version::Version;
use chrome_for_testing::api::{Download, HasVersion};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU16;
use std::sync::Arc;
use tokio::fs;
use tokio::process::Command;
use tokio_process_tools::{ProcessHandle, TerminateOnDrop};

#[derive(Debug)]
pub(crate) enum Artifact {
    Chrome,
    ChromeDriver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionRequest {
    /// Uses the latest working version. Might not be stable yet.
    /// You may want to prefer variant [VersionRequest::LatestIn] instead.
    Latest,

    /// Use the latest release from the given [chrome_for_testing::channel::Channel],
    /// e.g. the one from the [chrome_for_testing::channel::Channel::Stable] channel.
    LatestIn(Channel),

    /// Pin a specific version to use.
    Fixed(Version),
}

#[derive(Debug)]
pub struct SelectedVersion {
    channel: Option<Channel>,
    version: Version,
    #[expect(unused)]
    revision: String,
    chrome: Option<Download>,
    chromedriver: Option<Download>,
}

impl From<(VersionWithoutChannel, Platform)> for SelectedVersion {
    fn from((v, p): (VersionWithoutChannel, Platform)) -> Self {
        let chrome_download = v.downloads.chrome.iter().find(|d| d.platform == p).cloned();
        let chromedriver_download = v
            .downloads
            .chromedriver
            .map(|it| it.iter().find(|d| d.platform == p).unwrap().to_owned());

        SelectedVersion {
            channel: None,
            version: v.version,
            revision: v.revision,
            chrome: chrome_download,
            chromedriver: chromedriver_download,
        }
    }
}

impl From<(VersionInChannel, Platform)> for SelectedVersion {
    fn from((v, p): (VersionInChannel, Platform)) -> Self {
        let chrome_download = v.downloads.chrome.iter().find(|d| d.platform == p).cloned();
        let chromedriver_download = v
            .downloads
            .chromedriver
            .iter()
            .find(|d| d.platform == p)
            .cloned();

        SelectedVersion {
            channel: Some(v.channel),
            version: v.version,
            revision: v.revision,
            chrome: chrome_download,
            chromedriver: chromedriver_download,
        }
    }
}

#[derive(Debug)]
pub struct LoadedChromePackage {
    #[expect(unused)]
    pub chrome_executable: PathBuf,
    pub chromedriver_executable: PathBuf,
}

#[derive(Debug)]
pub struct ChromeForTestingManager {
    client: reqwest::Client,
    cache_dir: CacheDir,
    platform: Platform,
}

impl Default for ChromeForTestingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ChromeForTestingManager {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            cache_dir: CacheDir::get_or_create(),
            platform: Platform::detect(),
        }
    }

    fn version_dir(&self, version: Version) -> PathBuf {
        self.cache_dir.path().join(version.to_string())
    }

    pub async fn clear_cache(&self) -> anyhow::Result<()> {
        self.cache_dir.clear().await
    }

    pub(crate) async fn resolve_version(
        &self,
        version_selection: VersionRequest,
    ) -> Result<SelectedVersion, anyhow::Error> {
        let selected = match version_selection {
            VersionRequest::Latest => {
                fn get_latest<T: HasVersion + Clone>(options: &[T]) -> Option<T> {
                    if options.is_empty() {
                        return None;
                    }

                    let mut latest: &T = &options[0];

                    for option in &options[1..] {
                        if option.version() > latest.version() {
                            latest = option;
                        }
                    }

                    Some(latest.clone())
                }

                let all =
                    chrome_for_testing::api::known_good_versions::request(self.client.clone())
                        .await
                        .context("Failed to request latest versions.")?;
                // TODO: Search for latest version with both chrome and chromedriver available!
                get_latest(&all.versions).map(|v| SelectedVersion::from((v, self.platform)))
            }
            VersionRequest::LatestIn(channel) => {
                let all =
                    chrome_for_testing::api::last_known_good_versions::request(self.client.clone())
                        .await
                        .context("Failed to request latest versions.")?;
                all.channels
                    .get(&channel)
                    .cloned()
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
            VersionRequest::Fixed(version) => {
                let all =
                    chrome_for_testing::api::known_good_versions::request(self.client.clone())
                        .await
                        .context("Failed to request latest versions.")?;
                all.versions
                    .into_iter()
                    .find(|v| v.version == version)
                    .map(|v| SelectedVersion::from((v, self.platform)))
            }
        };

        let selected = selected.context("Could not determine version to use")?;

        Ok(selected)
    }

    pub(crate) async fn download(
        &self,
        selected: SelectedVersion,
    ) -> Result<LoadedChromePackage, anyhow::Error> {
        let selected_chrome_download = match selected.chrome.clone() {
            Some(download) => download,
            None => {
                return Err(anyhow::anyhow!(
                    "No chrome download found for selection {selected:?} using platform {}",
                    self.platform
                ))
            }
        };

        let selected_chromedriver_download = match selected.chromedriver.clone() {
            Some(download) => download,
            None => {
                return Err(anyhow::anyhow!(
                    "No chromedriver download found for {selected:?} using platform {}",
                    self.platform
                ))
            }
        };

        // Check if download is necessary.
        let version_dir = self.version_dir(selected.version);
        let platform_dir = version_dir.join(self.platform.to_string());
        fs::create_dir_all(&platform_dir).await?;

        fn determine_chrome_executable(platform_dir: &Path, platform: Platform) -> PathBuf {
            let unpack_dir = platform_dir.join(format!("chrome-{}", platform));
            match platform {
                Platform::Linux64 | Platform::MacX64 => unpack_dir.join("chrome"),
                Platform::MacArm64 => unpack_dir
                    .join("Google Chrome for Testing.app")
                    .join("Contents")
                    .join("MacOS")
                    .join("Google Chrome for Testing"),
                Platform::Win32 | Platform::Win64 => unpack_dir.join("chrome.exe"),
            }
        }

        let chrome_executable = determine_chrome_executable(&platform_dir, self.platform);
        let chromedriver_executable = platform_dir
            .join(format!("chromedriver-{}", self.platform))
            .join(self.platform.chromedriver_binary_name());

        // Download chrome if necessary.
        let is_chrome_downloaded = chrome_executable.exists() && chrome_executable.is_file();
        if !is_chrome_downloaded {
            tracing::info!(
                "Installing {} Chrome {}",
                match selected.channel {
                    None => "".to_string(),
                    Some(channel) => channel.to_string(),
                },
                selected.version,
            );
            download::download_zip(
                &self.client,
                &selected_chrome_download.url,
                &platform_dir,
                &platform_dir,
                Artifact::Chrome,
            )
            .await?;
        } else {
            tracing::info!(
                "Chrome {} already installed at {chrome_executable:?}...",
                selected.version
            );
        }

        // Download chromedriver if necessary.
        let is_chromedriver_downloaded =
            chromedriver_executable.exists() && chromedriver_executable.is_file();
        if !is_chromedriver_downloaded {
            tracing::info!(
                "Installing {} Chromedriver {}",
                match selected.channel {
                    None => "".to_string(),
                    Some(channel) => channel.to_string(),
                },
                selected.version,
            );
            download::download_zip(
                &self.client,
                &selected_chromedriver_download.url,
                &platform_dir,
                &platform_dir,
                Artifact::ChromeDriver,
            )
            .await?;
        } else {
            tracing::info!(
                "Chromedriver {} already installed at {chromedriver_executable:?}...",
                selected.version
            );
        }

        Ok(LoadedChromePackage {
            chrome_executable,
            chromedriver_executable,
        })
    }

    pub(crate) async fn launch_chromedriver(
        &self,
        loaded: &LoadedChromePackage,
        port: PortRequest,
    ) -> Result<(TerminateOnDrop, Port), anyhow::Error> {
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
                command.arg(format!("--port={}", port));
            }
        };
        let loglevel = chrome_for_testing::chromedriver::LogLevel::Info;
        command.arg(format!("--log-level={loglevel}"));

        self.apply_chromedriver_creation_flags(&mut command);

        let chromedriver_process = ProcessHandle::spawn("chromedriver", command)
            .context("Failed to spawn chromedriver process.")?;

        let _out_inspector = chromedriver_process.stdout().inspect(|stdout_line| {
            tracing::debug!(stdout_line, "chromedriver log");
        });
        let _err_inspector = chromedriver_process.stdout().inspect(|stderr_line| {
            tracing::debug!(stderr_line, "chromedriver log");
        });

        tracing::info!("Waiting for chromedriver to start...");
        let started_on_port = Arc::new(AtomicU16::new(0));
        let started_on_port_clone = started_on_port.clone();
        chromedriver_process
            .stdout()
            .wait_for_with_timeout(
                move |line| {
                    if line.contains("started successfully on port") {
                        let port = line
                            .trim()
                            .trim_matches('"')
                            .trim_end_matches('.')
                            .split(' ')
                            .last()
                            .expect("port as segment after last space")
                            .parse::<u16>()
                            .expect("port to be a u16");
                        started_on_port_clone.store(port, std::sync::atomic::Ordering::Release);
                        true
                    } else {
                        false
                    }
                },
                std::time::Duration::from_secs(10),
            )
            .await?;

        Ok((
            chromedriver_process.terminate_on_drop(
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(10),
            ),
            Port(Arc::into_inner(started_on_port).unwrap().into_inner()),
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
    fn apply_chromedriver_creation_flags<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
    }

    #[cfg(feature = "thirtyfour")]
    pub(crate) async fn prepare_caps(
        &self,
        loaded: &LoadedChromePackage,
    ) -> Result<thirtyfour::ChromeCapabilities, anyhow::Error> {
        tracing::info!(
            "Registering {:?} in capabilities.",
            loaded.chrome_executable
        );
        use thirtyfour::ChromiumLikeCapabilities;
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_headless()?;
        caps.set_binary(loaded.chrome_executable.to_str().expect("valid unicode"))?;
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use crate::mgr::ChromeForTestingManager;
    use crate::port::Port;
    use crate::port::PortRequest;
    use crate::prelude::*;
    use assertr::prelude::*;
    use chrome_for_testing::api::channel::Channel;
    use serial_test::serial;

    #[ctor::ctor]
    fn init_test_tracing() {
        tracing_subscriber::fmt().with_test_writer().try_init().ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clear_cache_and_download_new() -> anyhow::Result<()> {
        let mgr = ChromeForTestingManager::new();
        mgr.clear_cache().await?;
        let selected = mgr
            .resolve_version(VersionRequest::LatestIn(Channel::Stable))
            .await?;
        let loaded = mgr.download(selected).await?;

        assert_that(loaded.chrome_executable).exists().is_a_file();
        assert_that(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_latest() -> anyhow::Result<()> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = mgr.download(selected).await?;

        assert_that(loaded.chrome_executable).exists().is_a_file();
        assert_that(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_latest_in_stable_channel() -> anyhow::Result<()> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr
            .resolve_version(VersionRequest::LatestIn(Channel::Stable))
            .await?;
        let loaded = mgr.download(selected).await?;

        assert_that(loaded.chrome_executable).exists().is_a_file();
        assert_that(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn resolve_and_download_specific() -> anyhow::Result<()> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr
            .resolve_version(VersionRequest::Fixed(Version {
                major: 135,
                minor: 0,
                patch: 7019,
                build: 0,
            }))
            .await?;
        let loaded = mgr.download(selected).await?;

        assert_that(loaded.chrome_executable).exists().is_a_file();
        assert_that(loaded.chromedriver_executable)
            .exists()
            .is_a_file();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn launch_chromedriver_on_specific_port() -> anyhow::Result<()> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = mgr.download(selected).await?;
        let (_chromedriver, port) = mgr
            .launch_chromedriver(&loaded, PortRequest::Specific(Port(3333)))
            .await?;
        assert_that(port).is_equal_to(Port(3333));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn download_and_launch_chromedriver_on_random_port_and_prepare_thirtyfour_webdriver(
    ) -> anyhow::Result<()> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(VersionRequest::Latest).await?;
        let loaded = mgr.download(selected).await?;
        let (_chromedriver, port) = mgr.launch_chromedriver(&loaded, PortRequest::Any).await?;

        let caps = mgr.prepare_caps(&loaded).await?;
        let driver = thirtyfour::WebDriver::new(format!("http://localhost:{port}"), caps).await?;
        driver.goto("https://www.google.com").await?;

        let url = driver.current_url().await?;
        assert_that(url).has_display_value("https://www.google.com/");

        driver.quit().await?;

        Ok(())
    }
}
