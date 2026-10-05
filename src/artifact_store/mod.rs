//! Resolution-aligned artifact installation and loaded-package construction.
//!
//! The store coordinates downloads, atomic installation, validation, and cache leases. Version
//! selection and process launch remain separate concerns.

mod download;
mod extract;
mod installation;

#[cfg(test)]
pub(super) use self::installation::completion_marker_name;
use self::installation::{ArtifactInstaller, ArtifactRequest};
use crate::browser::{ChromeBinary, LoadedBrowserPackage};
use crate::cache::{CacheDir, CacheLease, CachePruneResult};
use crate::version::SelectedVersion;
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use ::chrome_for_testing::{Download, Platform, Version};
use rootcause::{Report, option_ext::OptionExt, report};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct ArtifactStore {
    cache_dir: CacheDir,
    client: reqwest::Client,
    platform: Platform,
    artifact_timeout: Duration,
}

impl ArtifactStore {
    pub(crate) fn new(
        cache_dir: CacheDir,
        client: reqwest::Client,
        artifact_timeout: Duration,
        platform: Platform,
    ) -> Self {
        Self {
            cache_dir,
            client,
            platform,
            artifact_timeout,
        }
    }

    pub(crate) fn cache_dir(&self) -> &CacheDir {
        &self.cache_dir
    }

    pub(crate) async fn clear(&self) -> Result<()> {
        self.cache_dir.clear().await
    }

    pub(crate) async fn prune(&self, retained_versions: &[Version]) -> Result<CachePruneResult> {
        self.cache_dir.prune(retained_versions).await
    }

    pub(crate) async fn download(
        &self,
        selected: &SelectedVersion,
        cancellation: CancellationToken,
    ) -> Result<Vec<LoadedBrowserPackage>> {
        let artifacts = self
            .download_requested_artifacts(selected, cancellation)
            .await?;
        selected
            .requested_artifacts()
            .binaries()
            .map(|binary| artifacts.package_for(binary, selected.version(), self.platform))
            .collect()
    }

    pub(crate) async fn download_for(
        &self,
        selected: &SelectedVersion,
        chrome_binary: ChromeBinary,
        cancellation: CancellationToken,
    ) -> Result<LoadedBrowserPackage> {
        if !selected.requested_artifacts().contains(chrome_binary) {
            return Err(report!(ChromeForTestingError::BrowserArtifactNotResolved {
                chrome_binary,
                version: selected.version(),
                platform: selected.platform(),
            }));
        }
        self.download_requested_artifacts(selected, cancellation)
            .await?
            .package_for(chrome_binary, selected.version(), self.platform)
    }

    async fn download_requested_artifacts(
        &self,
        selected: &SelectedVersion,
        cancellation: CancellationToken,
    ) -> Result<DownloadedBrowserArtifacts> {
        let downloads = self.downloads_for(selected)?;
        let cache_lease = self.cache_dir.acquire_shared(cancellation.clone()).await?;
        let transaction_cancellation = cancellation.child_token();
        let installer = ArtifactInstaller {
            cache_dir: &self.cache_dir,
            client: &self.client,
            platform: self.platform,
            artifact_timeout: self.artifact_timeout,
        };
        let driver_request = ArtifactRequest {
            artifact: ChromeForTestingArtifact::ChromeDriver,
            url: &downloads.chromedriver.url,
            executable: self.platform.chromedriver_executable_path(),
        };
        let chrome_request = downloads.chrome.map(|download| ArtifactRequest {
            artifact: ChromeForTestingArtifact::Chrome,
            url: &download.url,
            executable: self.platform.chrome_executable_path(),
        });
        let headless_request = downloads
            .chrome_headless_shell
            .map(|download| ArtifactRequest {
                artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                url: &download.url,
                executable: self.platform.chrome_headless_shell_executable_path(),
            });

        let driver = installer.install_and_cancel_siblings(
            selected.version(),
            driver_request,
            transaction_cancellation.clone(),
        );
        let chrome = installer.install_optional_and_cancel_siblings(
            selected.version(),
            chrome_request,
            transaction_cancellation.clone(),
        );
        let headless = installer.install_optional_and_cancel_siblings(
            selected.version(),
            headless_request,
            transaction_cancellation,
        );
        let (driver, chrome, chrome_headless_shell) = tokio::join!(driver, chrome, headless);
        ArtifactInstallResults {
            chromedriver: driver,
            chrome,
            chrome_headless_shell,
        }
        .finish(cache_lease, cancellation.is_cancelled())
    }

    fn downloads_for<'a>(&self, selected: &'a SelectedVersion) -> Result<ResolvedDownloads<'a>> {
        if selected.platform() != self.platform {
            return Err(report!(
                ChromeForTestingError::SelectedVersionPlatformMismatch {
                    selected: selected.platform(),
                    manager: self.platform,
                }
            ));
        }

        let requested = selected.requested_artifacts();
        let chromedriver =
            selected
                .chromedriver
                .as_ref()
                .context(ChromeForTestingError::NoArtifactDownload {
                    artifact: ChromeForTestingArtifact::ChromeDriver,
                    version: selected.version(),
                    platform: self.platform,
                })?;
        let chrome =
            if requested.contains(ChromeBinary::Chrome) {
                Some(selected.chrome.as_ref().context(
                    ChromeForTestingError::NoArtifactDownload {
                        artifact: ChromeForTestingArtifact::Chrome,
                        version: selected.version(),
                        platform: self.platform,
                    },
                )?)
            } else {
                None
            };
        let chrome_headless_shell = if requested.contains(ChromeBinary::ChromeHeadlessShell) {
            Some(selected.chrome_headless_shell.as_ref().context(
                ChromeForTestingError::NoArtifactDownload {
                    artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                    version: selected.version(),
                    platform: self.platform,
                },
            )?)
        } else {
            None
        };

        Ok(ResolvedDownloads {
            chromedriver,
            chrome,
            chrome_headless_shell,
        })
    }
}

struct ResolvedDownloads<'a> {
    chromedriver: &'a Download,
    chrome: Option<&'a Download>,
    chrome_headless_shell: Option<&'a Download>,
}

pub(super) struct ArtifactInstallResults {
    chromedriver: Result<PathBuf>,
    chrome: Result<Option<PathBuf>>,
    chrome_headless_shell: Result<Option<PathBuf>>,
}

impl ArtifactInstallResults {
    fn finish(
        self,
        cache_lease: CacheLease,
        caller_cancelled: bool,
    ) -> Result<DownloadedBrowserArtifacts> {
        if caller_cancelled {
            let mut cancellation_error = report!(ChromeForTestingError::Cancelled);
            Self::attach_secondary_errors(&mut cancellation_error, self.into_errors());
            return Err(cancellation_error);
        }

        match (self.chromedriver, self.chrome, self.chrome_headless_shell) {
            (Ok(chromedriver), Ok(chrome), Ok(chrome_headless_shell)) => {
                Ok(DownloadedBrowserArtifacts {
                    chromedriver,
                    chrome,
                    chrome_headless_shell,
                    cache_lease,
                })
            }
            (chromedriver, chrome, chrome_headless_shell) => Err(Self::combine_errors(
                chromedriver
                    .err()
                    .into_iter()
                    .chain(chrome.err())
                    .chain(chrome_headless_shell.err()),
            )),
        }
    }

    fn into_errors(self) -> impl Iterator<Item = Report<ChromeForTestingError>> {
        self.chromedriver
            .err()
            .into_iter()
            .chain(self.chrome.err())
            .chain(self.chrome_headless_shell.err())
    }

    fn combine_errors(
        errors: impl IntoIterator<Item = Report<ChromeForTestingError>>,
    ) -> Report<ChromeForTestingError> {
        let mut errors = errors.into_iter().collect::<Vec<_>>();
        let primary_index = errors
            .iter()
            .position(|error| !matches!(error.current_context(), ChromeForTestingError::Cancelled))
            .or((!errors.is_empty()).then_some(0))
            .expect("caller only combines failed transactions");
        let mut primary = errors.remove(primary_index);
        Self::attach_secondary_errors(&mut primary, errors);
        primary
    }

    fn attach_secondary_errors(
        primary: &mut Report<ChromeForTestingError>,
        errors: impl IntoIterator<Item = Report<ChromeForTestingError>>,
    ) {
        for secondary in errors {
            primary
                .children_mut()
                .push(secondary.into_dynamic().into_cloneable());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ArtifactInstallResults;
    use crate::ChromeForTestingError;
    use assertr::prelude::*;
    use rootcause::report;

    #[test]
    fn transaction_error_prefers_real_failure_over_derivative_cancellation() {
        let error = ArtifactInstallResults::combine_errors([
            report!(ChromeForTestingError::Cancelled),
            report!(ChromeForTestingError::UnsupportedPlatform),
        ]);

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::UnsupportedPlatform
        ))
        .is_true();
        assert_that!(error.children().into_iter().count()).is_equal_to(1);
    }
}

#[derive(Debug)]
struct DownloadedBrowserArtifacts {
    chromedriver: PathBuf,
    chrome: Option<PathBuf>,
    chrome_headless_shell: Option<PathBuf>,
    cache_lease: CacheLease,
}

impl DownloadedBrowserArtifacts {
    fn package_for(
        &self,
        chrome_binary: ChromeBinary,
        version: Version,
        platform: Platform,
    ) -> Result<LoadedBrowserPackage> {
        let browser_executable = match chrome_binary {
            ChromeBinary::Chrome => {
                self.chrome
                    .clone()
                    .context(ChromeForTestingError::NoArtifactDownload {
                        artifact: ChromeForTestingArtifact::Chrome,
                        version,
                        platform,
                    })?
            }
            ChromeBinary::ChromeHeadlessShell => self.chrome_headless_shell.clone().context(
                ChromeForTestingError::NoArtifactDownload {
                    artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                    version,
                    platform,
                },
            )?,
        };
        Ok(LoadedBrowserPackage::new(
            chrome_binary,
            browser_executable,
            self.chromedriver.clone(),
            self.cache_lease.clone(),
        ))
    }
}
