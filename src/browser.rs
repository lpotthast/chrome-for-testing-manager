//! Browser artifact selection and cache-backed loaded package handles.
//!
//! Loaded packages retain a shared cache lease so executable paths remain valid for their entire
//! lifetime.

use crate::ChromeForTestingArtifact;
use crate::cache::CacheLease;
use std::path::{Path, PathBuf};

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

/// Non-empty browser artifact requirement used during version resolution and installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BrowserArtifactRequest {
    /// Require regular Chrome and its matching `ChromeDriver`.
    Chrome,

    /// Require Chrome Headless Shell and its matching `ChromeDriver`.
    ChromeHeadlessShell,

    /// Require both browser variants and their matching `ChromeDriver`.
    Both,
}

impl ChromeBinary {
    /// The artifact holding this browser package.
    pub(crate) const fn artifact(self) -> ChromeForTestingArtifact {
        match self {
            Self::Chrome => ChromeForTestingArtifact::Chrome,
            Self::ChromeHeadlessShell => ChromeForTestingArtifact::ChromeHeadlessShell,
        }
    }
}

impl BrowserArtifactRequest {
    pub(crate) const fn contains(self, binary: ChromeBinary) -> bool {
        matches!(
            (self, binary),
            (Self::Chrome, ChromeBinary::Chrome)
                | (Self::ChromeHeadlessShell, ChromeBinary::ChromeHeadlessShell)
                | (Self::Both, _)
        )
    }
}

impl From<ChromeBinary> for BrowserArtifactRequest {
    fn from(binary: ChromeBinary) -> Self {
        match binary {
            ChromeBinary::Chrome => Self::Chrome,
            ChromeBinary::ChromeHeadlessShell => Self::ChromeHeadlessShell,
        }
    }
}

/// A downloaded browser package paired with a matching `ChromeDriver`.
///
/// The package was published by a fully completed installation transaction. Later cache hits are
/// revalidated through cheap metadata only (completion marker plus executable size), so external
/// modification of already-installed files is not detected beyond an executable size change.
///
/// The hidden cache lease keeps the cache alive for as long as this value or any clone exists, so
/// [`crate::ChromeForTestingManager::clear_cache`] returns
/// [`crate::ChromeForTestingError::CacheInUse`] instead of invalidating these paths.
#[derive(Debug, Clone)]
pub struct LoadedBrowserPackage {
    chrome_binary: ChromeBinary,
    browser_executable: PathBuf,
    chromedriver_executable: PathBuf,
    cache_lease: CacheLease,
}

impl LoadedBrowserPackage {
    pub(crate) fn new(
        chrome_binary: ChromeBinary,
        browser_executable: PathBuf,
        chromedriver_executable: PathBuf,
        cache_lease: CacheLease,
    ) -> Self {
        Self {
            chrome_binary,
            browser_executable,
            chromedriver_executable,
            cache_lease,
        }
    }

    /// Return the selected browser binary variant.
    #[must_use]
    pub const fn chrome_binary(&self) -> ChromeBinary {
        self.chrome_binary
    }

    /// Return the cached browser executable path.
    #[must_use]
    pub fn browser_executable(&self) -> &Path {
        &self.browser_executable
    }

    /// Return the cached `ChromeDriver` executable path.
    #[must_use]
    pub fn chromedriver_executable(&self) -> &Path {
        &self.chromedriver_executable
    }

    pub(crate) fn cache_lease(&self) -> CacheLease {
        self.cache_lease.clone()
    }
}
