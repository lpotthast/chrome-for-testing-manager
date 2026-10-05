//! Artifact-aware Chrome for Testing release-manifest resolution.
//!
//! Candidate versions are filtered by platform and requested browser artifacts before selection,
//! preventing later installation from requesting unavailable packages.

use crate::version::{ReleaseDownloads, SelectedVersion, VersionRequest};
use crate::{BrowserArtifactRequest, CancellationToken, ChromeForTestingError, Result};
use ::chrome_for_testing::{KnownGoodVersions, LastKnownGoodVersions, Platform};
use rootcause::{option_ext::OptionExt, prelude::ResultExt};

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
        let request_error = || ChromeForTestingError::RequestVersions {
            version_request: version_request.clone(),
        };
        let platform = self.platform;

        let selected = match &version_request {
            VersionRequest::LatestIn(channel) => {
                let manifest =
                    crate::await_or_cancelled(&cancellation, self.fetch_last_known_good_versions())
                        .await?
                        .context_with(request_error)?;
                manifest.channel(channel).and_then(|release| {
                    let downloads = ReleaseDownloads::of_channel(release, platform);
                    downloads.supports(requested_artifacts).then(|| {
                        SelectedVersion::new(
                            Some(release.channel.clone()),
                            release.version,
                            platform,
                            requested_artifacts,
                            &downloads,
                        )
                    })
                })
            }
            VersionRequest::Latest | VersionRequest::Fixed(_) => {
                let manifest =
                    crate::await_or_cancelled(&cancellation, self.fetch_known_good_versions())
                        .await?
                        .context_with(request_error)?;
                manifest
                    .versions
                    .iter()
                    .filter(|release| match &version_request {
                        VersionRequest::Fixed(version) => release.version == *version,
                        _ => true,
                    })
                    .map(|release| (release, ReleaseDownloads::of_known_good(release, platform)))
                    .filter(|(_, downloads)| downloads.supports(requested_artifacts))
                    .max_by_key(|(release, _)| release.version)
                    .map(|(release, downloads)| {
                        SelectedVersion::new(
                            None,
                            release.version,
                            platform,
                            requested_artifacts,
                            &downloads,
                        )
                    })
            }
        };

        selected.context(ChromeForTestingError::NoMatchingVersion {
            version_request,
            platform,
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
}
