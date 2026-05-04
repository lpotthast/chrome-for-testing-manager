use chrome_for_testing::{
    Channel, Download, Platform, Version, VersionInChannel, VersionWithoutChannel,
};

/// How to pick which Chrome / `ChromeDriver` version to install and run.
///
/// See the named constructors ([`Self::stable`], [`Self::beta`], [`Self::dev`], [`Self::canary`])
/// and the `From<Channel>` / `From<Version>` impls for the most ergonomic forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionRequest {
    /// Uses the latest working version. Might not be stable yet.
    /// You may want to prefer variant [`VersionRequest::LatestIn`] instead.
    Latest,

    /// Use the latest release from the given [`Channel`],
    /// e.g. the one from the [`Channel::Stable`] channel.
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
/// Construct via [`crate::ChromeForTestingManager::resolve_version`] and pass into
/// [`crate::ChromeForTestingManager::download`].
#[derive(Debug)]
pub struct SelectedVersion {
    pub(crate) channel: Option<Channel>,
    pub(crate) version: Version,
    pub(crate) chrome: Option<Download>,
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

    /// Whether a Chrome download exists for this version on the detected platform.
    #[must_use]
    pub fn has_chrome_download(&self) -> bool {
        self.chrome.is_some()
    }

    /// Whether a `ChromeDriver` download exists for this version on the detected platform.
    #[must_use]
    pub fn has_chromedriver_download(&self) -> bool {
        self.chromedriver.is_some()
    }
}

impl From<(VersionWithoutChannel, Platform)> for SelectedVersion {
    fn from((v, p): (VersionWithoutChannel, Platform)) -> Self {
        SelectedVersion {
            channel: None,
            version: v.version,
            chrome: v.downloads.chrome_for_platform(p).cloned(),
            chromedriver: v.downloads.chromedriver_for_platform(p).cloned(),
        }
    }
}

impl From<(VersionInChannel, Platform)> for SelectedVersion {
    fn from((v, p): (VersionInChannel, Platform)) -> Self {
        let chrome_download = v.downloads.chrome_for_platform(p).cloned();
        let chromedriver_download = v.downloads.chromedriver_for_platform(p).cloned();

        SelectedVersion {
            channel: Some(v.channel),
            version: v.version,
            chrome: chrome_download,
            chromedriver: chromedriver_download,
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
