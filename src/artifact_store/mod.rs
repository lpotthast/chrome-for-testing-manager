//! Resolution-aligned artifact installation and loaded-package construction.
//!
//! The store coordinates downloads, atomic installation, validation, and cache leases. Version
//! selection and process launch remain separate concerns.

mod download;
mod extract;
mod installation;

use self::installation::ArtifactRequest;
#[cfg(test)]
pub(crate) use self::installation::COMPLETION_MARKER;
use crate::browser::{BrowserArtifactRequest, ChromeBinary, LoadedBrowserPackage};
use crate::cache::CacheDir;
use crate::error::attach_child;
use crate::version::SelectedVersion;
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use ::chrome_for_testing::Platform;
use rootcause::{Report, option_ext::OptionExt, report};
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

    /// Install the `requested` browser packages of `selected`, plus the matching `ChromeDriver`,
    /// and return one package per requested browser (Chrome before Chrome Headless Shell).
    ///
    /// The artifacts install concurrently under one shared cache lease. A failing installation
    /// cancels the others, and every installation is drained before this returns.
    pub(crate) async fn install(
        &self,
        selected: &SelectedVersion,
        requested: BrowserArtifactRequest,
        cancellation: CancellationToken,
    ) -> Result<Vec<LoadedBrowserPackage>> {
        if selected.platform() != self.platform {
            return Err(report!(
                ChromeForTestingError::SelectedVersionPlatformMismatch {
                    selected: selected.platform(),
                    manager: self.platform,
                }
            ));
        }
        let driver_request =
            self.artifact_request(selected, ChromeForTestingArtifact::ChromeDriver)?;
        let browser_request = |binary: ChromeBinary| {
            requested
                .contains(binary)
                .then(|| self.artifact_request(selected, binary.artifact()))
                .transpose()
        };
        let chrome_request = browser_request(ChromeBinary::Chrome)?;
        let headless_request = browser_request(ChromeBinary::ChromeHeadlessShell)?;

        let cache_lease = self.cache_dir.acquire_shared(&cancellation).await?;
        // A failing installation cancels its siblings through this token, not the caller's.
        let siblings = cancellation.child_token();
        let version = selected.version();
        let (driver, chrome, headless) = tokio::join!(
            self.install_artifact_or_cancel(version, Some(driver_request), &cache_lease, &siblings),
            self.install_artifact_or_cancel(version, chrome_request, &cache_lease, &siblings),
            self.install_artifact_or_cancel(version, headless_request, &cache_lease, &siblings),
        );
        let (driver, chrome, headless) = match (driver, chrome, headless) {
            (Ok(Some(driver)), Ok(chrome), Ok(headless)) if !cancellation.is_cancelled() => {
                (driver, chrome, headless)
            }
            (driver, chrome, headless) => {
                let errors = [driver.err(), chrome.err(), headless.err()];
                return Err(combine_install_errors(
                    errors.into_iter().flatten().collect(),
                    cancellation.is_cancelled(),
                ));
            }
        };
        Ok([
            (ChromeBinary::Chrome, chrome),
            (ChromeBinary::ChromeHeadlessShell, headless),
        ]
        .into_iter()
        .filter_map(|(binary, browser)| {
            Some(LoadedBrowserPackage::new(
                binary,
                browser?,
                driver.clone(),
                cache_lease.clone(),
            ))
        })
        .collect())
    }

    /// The download and executable of `artifact` in `selected`.
    ///
    /// The resolver only selects versions providing every requested download; the error guards
    /// that invariant.
    fn artifact_request(
        &self,
        selected: &SelectedVersion,
        artifact: ChromeForTestingArtifact,
    ) -> Result<ArtifactRequest> {
        let (download, executable) = match artifact {
            ChromeForTestingArtifact::Chrome => {
                (&selected.chrome, self.platform.chrome_executable_path())
            }
            ChromeForTestingArtifact::ChromeHeadlessShell => (
                &selected.chrome_headless_shell,
                self.platform.chrome_headless_shell_executable_path(),
            ),
            ChromeForTestingArtifact::ChromeDriver => (
                &selected.chromedriver,
                self.platform.chromedriver_executable_path(),
            ),
        };
        let download = download
            .as_ref()
            .context(ChromeForTestingError::NoArtifactDownload {
                artifact,
                version: selected.version(),
                platform: self.platform,
            })?;
        Ok(ArtifactRequest {
            artifact,
            url: download.url.clone(),
            executable,
        })
    }
}

/// Combine the errors of a failed installation transaction.
///
/// Installations cancelled because of a sibling failure or caller cancellation only echo that
/// cause, so their `Cancelled` errors are dropped, keeping only what they carry beneath (e.g. a
/// failed rollback). Caller cancellation is primary; otherwise the first real failure is.
fn combine_install_errors(
    mut errors: Vec<Report<ChromeForTestingError>>,
    caller_cancelled: bool,
) -> Report<ChromeForTestingError> {
    let is_cancelled = |error: &Report<ChromeForTestingError>| {
        matches!(error.current_context(), ChromeForTestingError::Cancelled)
    };
    let mut primary = match errors.iter().position(|error| !is_cancelled(error)) {
        Some(index) if !caller_cancelled => errors.remove(index),
        _ => report!(ChromeForTestingError::Cancelled),
    };
    for mut error in errors {
        if is_cancelled(&error) {
            let mut children = Vec::new();
            while let Some(child) = error.children_mut().pop() {
                children.push(child);
            }
            for child in children.into_iter().rev() {
                primary.children_mut().push(child);
            }
        } else {
            attach_child(&mut primary, error);
        }
    }
    primary
}

#[cfg(test)]
mod tests {
    use super::combine_install_errors;
    use crate::ChromeForTestingError;
    use crate::error::attach_child;
    use assertr::prelude::*;
    use rootcause::report;
    use std::path::PathBuf;

    #[test]
    fn failures_beneath_derivative_cancellations_are_kept() {
        let mut cancelled_sibling = report!(ChromeForTestingError::Cancelled);
        attach_child(
            &mut cancelled_sibling,
            report!(ChromeForTestingError::RemoveStaleArtifact {
                path: PathBuf::from("staging"),
            }),
        );

        let error = combine_install_errors(
            vec![
                report!(ChromeForTestingError::DetermineCacheDir),
                cancelled_sibling,
            ],
            false,
        );

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::DetermineCacheDir
        ))
        .is_true();
        assert_that!(format!("{error:?}")).contains("RemoveStaleArtifact");
    }

    #[test]
    fn transaction_error_prefers_real_failure_over_derivative_cancellation() {
        let error = combine_install_errors(
            vec![
                report!(ChromeForTestingError::Cancelled),
                report!(ChromeForTestingError::DetermineCacheDir),
            ],
            false,
        );

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::DetermineCacheDir
        ))
        .is_true();
        assert_that!(error.children().into_iter().count()).is_equal_to(0);
    }

    #[test]
    fn caller_cancellation_is_primary_and_keeps_real_failures() {
        let error = combine_install_errors(
            vec![
                report!(ChromeForTestingError::Cancelled),
                report!(ChromeForTestingError::DetermineCacheDir),
            ],
            true,
        );

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        assert_that!(error.children().into_iter().count()).is_equal_to(1);
    }
}
