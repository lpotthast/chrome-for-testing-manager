//! Artifact-aware Chrome for Testing release-manifest resolution.
//!
//! Candidate versions are filtered by platform and requested browser artifacts before selection,
//! preventing later installation from requesting unavailable packages.

use crate::version::{SelectedVersion, VersionRequest};
use crate::{
    BrowserArtifactRequest, CancellationToken, ChromeBinary, ChromeForTestingError, Result,
};
use ::chrome_for_testing::{KnownGoodVersions, LastKnownGoodVersions, Platform};
use rootcause::{option_ext::OptionExt, report};

/// The known-good and per-channel manifests use two structurally identical but distinct
/// `Downloads` types; this expands the same artifact-availability check for either.
macro_rules! downloads_support {
    ($downloads:expr, $platform:expr, $request:expr) => {
        VersionResolver::supports_artifacts(
            $request,
            $downloads.chromedriver_for_platform($platform).is_some(),
            $downloads.chrome_for_platform($platform).is_some(),
            $downloads
                .chrome_headless_shell_for_platform($platform)
                .is_some(),
        )
    };
}

#[derive(Debug, Clone)]
pub(crate) struct VersionResolver {
    client: reqwest::Client,
    platform: Platform,
    #[cfg(test)]
    manifest_base_url: Option<reqwest::Url>,
}

impl VersionResolver {
    pub(crate) fn new(client: reqwest::Client, platform: Platform) -> Self {
        Self {
            client,
            platform,
            #[cfg(test)]
            manifest_base_url: None,
        }
    }

    pub(crate) async fn resolve(
        &self,
        version_request: VersionRequest,
        requested_artifacts: BrowserArtifactRequest,
        cancellation: CancellationToken,
    ) -> Result<SelectedVersion> {
        crate::check_cancelled(&cancellation)?;

        let selected = match &version_request {
            VersionRequest::Latest => {
                let all =
                    crate::await_or_cancelled(&cancellation, self.fetch_known_good_versions())
                        .await?
                        .map_err(|error| Self::request_versions_error(error, &version_request))?;
                all.versions
                    .into_iter()
                    .filter(|version| {
                        downloads_support!(version.downloads, self.platform, requested_artifacts)
                    })
                    .max_by_key(|version| version.version)
                    .map(|version| {
                        SelectedVersion::from_version(&version, self.platform, requested_artifacts)
                    })
            }
            VersionRequest::LatestIn(channel) => {
                let all =
                    crate::await_or_cancelled(&cancellation, self.fetch_last_known_good_versions())
                        .await?
                        .map_err(|error| Self::request_versions_error(error, &version_request))?;
                all.channel(channel)
                    .filter(|version| {
                        downloads_support!(version.downloads, self.platform, requested_artifacts)
                    })
                    .cloned()
                    .map(|version| {
                        SelectedVersion::from_channel_version(
                            version,
                            self.platform,
                            requested_artifacts,
                        )
                    })
            }
            VersionRequest::Fixed(requested_version) => {
                let all =
                    crate::await_or_cancelled(&cancellation, self.fetch_known_good_versions())
                        .await?
                        .map_err(|error| Self::request_versions_error(error, &version_request))?;
                all.versions
                    .into_iter()
                    .find(|version| {
                        version.version == *requested_version
                            && downloads_support!(
                                version.downloads,
                                self.platform,
                                requested_artifacts
                            )
                    })
                    .map(|version| {
                        SelectedVersion::from_version(&version, self.platform, requested_artifacts)
                    })
            }
        };

        crate::check_cancelled(&cancellation)?;
        selected.context(ChromeForTestingError::NoMatchingVersion {
            version_request,
            requested_artifacts,
        })
    }

    async fn fetch_known_good_versions(&self) -> chrome_for_testing::Result<KnownGoodVersions> {
        #[cfg(test)]
        if let Some(base_url) = &self.manifest_base_url {
            return KnownGoodVersions::fetch_with_base_url(&self.client, base_url).await;
        }
        KnownGoodVersions::fetch(&self.client).await
    }

    async fn fetch_last_known_good_versions(
        &self,
    ) -> chrome_for_testing::Result<LastKnownGoodVersions> {
        #[cfg(test)]
        if let Some(base_url) = &self.manifest_base_url {
            return LastKnownGoodVersions::fetch_with_base_url(&self.client, base_url).await;
        }
        LastKnownGoodVersions::fetch(&self.client).await
    }

    #[cfg(test)]
    pub(crate) fn set_manifest_base_url(&mut self, base_url: reqwest::Url) {
        self.manifest_base_url = Some(base_url);
    }

    /// Whether a release providing the given artifacts satisfies `request`.
    const fn supports_artifacts(
        request: BrowserArtifactRequest,
        has_chromedriver: bool,
        has_chrome: bool,
        has_chrome_headless_shell: bool,
    ) -> bool {
        has_chromedriver
            && (!request.contains(ChromeBinary::Chrome) || has_chrome)
            && (!request.contains(ChromeBinary::ChromeHeadlessShell) || has_chrome_headless_shell)
    }

    fn request_versions_error(
        error: impl std::fmt::Display,
        version_request: &VersionRequest,
    ) -> rootcause::Report<ChromeForTestingError> {
        report!(ChromeForTestingError::RequestVersions {
            version_request: version_request.clone(),
        })
        .attach(format!("chrome-for-testing error:\n{error}"))
    }
}
