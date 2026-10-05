//! Version requests and artifact-aware resolved release descriptions.
//!
//! The resolver submodule performs network lookup; the types here record the exact platform and
//! artifacts later installation steps must honor.

pub(crate) mod resolver;

use crate::{BrowserArtifactRequest, ChromeBinary};
use ::chrome_for_testing::{
    Channel, Download, Platform, Version, VersionInChannel, VersionWithoutChannel,
};

/// How to pick which Chrome / `ChromeDriver` version to install and run.
///
/// See the named constructors ([`Self::stable`], [`Self::beta`], [`Self::dev`], [`Self::canary`])
/// and the `From<Channel>` / `From<Version>` impls for the most ergonomic forms.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionRequest {
    /// Uses the latest working version. Might not be stable yet.
    /// You may want to prefer variant [`VersionRequest::LatestIn`] instead.
    Latest,

    /// Use the latest release from the given [`Channel`],
    /// e.g. the one from the [`Channel::Stable`] channel.
    ///
    /// Only the channel's current release is considered. If it lacks a requested artifact (for
    /// example Chrome Headless Shell) on the target platform, resolution fails with
    /// [`crate::ChromeForTestingError::NoMatchingVersion`] instead of falling back to an older
    /// release.
    LatestIn(Channel),

    /// Pin a specific version to use.
    Fixed(Version),
}

impl From<Channel> for VersionRequest {
    fn from(channel: Channel) -> Self {
        Self::LatestIn(channel)
    }
}

impl From<Version> for VersionRequest {
    fn from(version: Version) -> Self {
        Self::Fixed(version)
    }
}

impl VersionRequest {
    /// Latest release from the [`Channel::Stable`] channel.
    #[must_use]
    pub fn stable() -> Self {
        Self::LatestIn(Channel::Stable)
    }

    /// Latest release from the [`Channel::Beta`] channel.
    #[must_use]
    pub fn beta() -> Self {
        Self::LatestIn(Channel::Beta)
    }

    /// Latest release from the [`Channel::Dev`] channel.
    #[must_use]
    pub fn dev() -> Self {
        Self::LatestIn(Channel::Dev)
    }

    /// Latest release from the [`Channel::Canary`] channel.
    #[must_use]
    pub fn canary() -> Self {
        Self::LatestIn(Channel::Canary)
    }
}

/// A version of Chrome and `ChromeDriver` that has been resolved against the
/// chrome-for-testing release index but not yet downloaded.
///
/// Construct via [`crate::ChromeForTestingManager::resolve_version`]. The selected value records
/// its non-empty [`crate::BrowserArtifactRequest`], and [`crate::ChromeForTestingManager::download`]
/// installs exactly that resolved set.
#[derive(Debug, Clone)]
pub struct SelectedVersion {
    pub(crate) channel: Option<Channel>,
    pub(crate) version: Version,
    pub(crate) platform: Platform,
    pub(crate) requested_artifacts: BrowserArtifactRequest,
    pub(crate) chrome: Option<Download>,
    pub(crate) chrome_headless_shell: Option<Download>,
    pub(crate) chromedriver: Option<Download>,
}

impl SelectedVersion {
    /// The release channel this version was resolved through, if any.
    /// `None` for versions resolved by [`VersionRequest::Latest`] or [`VersionRequest::Fixed`].
    #[must_use]
    pub fn channel(&self) -> Option<&Channel> {
        self.channel.as_ref()
    }

    /// The pinned [`Version`] that will be downloaded.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// The platform for which this version was resolved.
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// The non-empty browser artifact set guaranteed by this resolution.
    #[must_use]
    pub const fn requested_artifacts(&self) -> BrowserArtifactRequest {
        self.requested_artifacts
    }

    /// Whether the release index lists a Chrome download for this version and platform.
    ///
    /// This reports availability, not what will be installed: a download is installed only if it
    /// is part of [`Self::requested_artifacts`], and every requested artifact is guaranteed to be
    /// available.
    #[must_use]
    pub fn has_chrome_download(&self) -> bool {
        self.chrome.is_some()
    }

    /// Whether the release index lists a Chrome Headless Shell download for this version and
    /// platform.
    ///
    /// Like [`Self::has_chrome_download`], this reports availability, not what will be installed.
    #[must_use]
    pub fn has_chrome_headless_shell_download(&self) -> bool {
        self.chrome_headless_shell.is_some()
    }

    /// Whether the release index lists a `ChromeDriver` download for this version and platform.
    #[deprecated(
        since = "0.13.0",
        note = "always true: resolution only selects releases providing ChromeDriver"
    )]
    #[must_use]
    pub fn has_chromedriver_download(&self) -> bool {
        self.chromedriver.is_some()
    }
}

/// One release's downloads for the target platform.
///
/// The known-good and per-channel manifests use structurally identical but distinct release
/// types; both convert into this view, which holds the availability rules in one place.
pub(crate) struct ReleaseDownloads<'a> {
    chrome: Option<&'a Download>,
    chrome_headless_shell: Option<&'a Download>,
    chromedriver: Option<&'a Download>,
}

impl<'a> ReleaseDownloads<'a> {
    pub(crate) fn of_known_good(release: &'a VersionWithoutChannel, platform: Platform) -> Self {
        let downloads = &release.downloads;
        Self {
            chrome: downloads.chrome_for_platform(platform),
            chrome_headless_shell: downloads.chrome_headless_shell_for_platform(platform),
            chromedriver: downloads.chromedriver_for_platform(platform),
        }
    }

    pub(crate) fn of_channel(release: &'a VersionInChannel, platform: Platform) -> Self {
        let downloads = &release.downloads;
        Self {
            chrome: downloads.chrome_for_platform(platform),
            chrome_headless_shell: downloads.chrome_headless_shell_for_platform(platform),
            chromedriver: downloads.chromedriver_for_platform(platform),
        }
    }

    /// Whether the release provides `ChromeDriver` and every requested browser.
    pub(crate) fn supports(&self, requested: BrowserArtifactRequest) -> bool {
        self.chromedriver.is_some()
            && (!requested.contains(ChromeBinary::Chrome) || self.chrome.is_some())
            && (!requested.contains(ChromeBinary::ChromeHeadlessShell)
                || self.chrome_headless_shell.is_some())
    }
}

impl SelectedVersion {
    pub(crate) fn new(
        channel: Option<Channel>,
        version: Version,
        platform: Platform,
        requested_artifacts: BrowserArtifactRequest,
        downloads: &ReleaseDownloads<'_>,
    ) -> Self {
        Self {
            channel,
            version,
            platform,
            requested_artifacts,
            chrome: downloads.chrome.cloned(),
            chrome_headless_shell: downloads.chrome_headless_shell.cloned(),
            chromedriver: downloads.chromedriver.cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;

    mod version_request {
        use super::*;

        #[test]
        fn from_channel_resolves_to_latest_in_channel() {
            assert_that!(VersionRequest::from(Channel::Stable))
                .is_equal_to(VersionRequest::LatestIn(Channel::Stable));
        }

        #[test]
        fn named_constructors_match_explicit_variants() {
            assert_that!(VersionRequest::stable())
                .is_equal_to(VersionRequest::LatestIn(Channel::Stable));
            assert_that!(VersionRequest::beta())
                .is_equal_to(VersionRequest::LatestIn(Channel::Beta));
            assert_that!(VersionRequest::dev()).is_equal_to(VersionRequest::LatestIn(Channel::Dev));
            assert_that!(VersionRequest::canary())
                .is_equal_to(VersionRequest::LatestIn(Channel::Canary));
        }

        #[test]
        fn from_parsed_version_resolves_to_fixed() {
            let v: Version = "135.0.7019.0".parse().expect("valid version literal");
            assert_that!(VersionRequest::from(v)).is_equal_to(VersionRequest::Fixed(v));
        }
    }
}
