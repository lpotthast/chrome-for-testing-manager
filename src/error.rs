use crate::{Port, VersionRequest};
use chrome_for_testing::{Platform, Version};
use std::{
    fmt::{Display, Formatter},
    path::PathBuf,
    time::Duration,
};
use thiserror::Error;
use tokio::runtime::RuntimeFlavor;

/// Convenience alias for `Result<T, rootcause::Report<ChromeForTestingManagerError>>`.
///
/// Use this in your application's signatures to avoid spelling out the wrapped error type:
///
/// ```no_run
/// use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig, Result};
///
/// async fn launch() -> Result<Chromedriver> {
///     Chromedriver::run(ChromedriverRunConfig::default()).await
/// }
/// ```
pub type Result<T> = std::result::Result<T, rootcause::Report<ChromeForTestingManagerError>>;

/// The chrome-for-testing artifact involved in an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChromeForTestingArtifact {
    /// The Chrome browser binary package.
    Chrome,

    /// The Chrome Headless Shell binary package.
    ChromeHeadlessShell,

    /// The Chromedriver package.
    ChromeDriver,
}

impl Display for ChromeForTestingArtifact {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Chrome => f.write_str("chrome"),
            Self::ChromeHeadlessShell => f.write_str("chrome-headless-shell"),
            Self::ChromeDriver => f.write_str("chromedriver"),
        }
    }
}

/// Error contexts reported by chrome-for-testing-manager operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChromeForTestingManagerError {
    /* Runtime and platform. */
    /// The current Tokio runtime does not support async drop cleanup.
    #[error("chromedriver requires a multi-threaded Tokio runtime; detected {runtime_flavor:?}")]
    UnsupportedRuntime {
        /// The detected runtime flavor.
        runtime_flavor: RuntimeFlavor,
    },

    /// The current platform is unsupported by chrome-for-testing.
    #[error("unsupported chrome-for-testing platform")]
    UnsupportedPlatform,

    // Cache and version resolution.
    /// The cache directory could not be determined.
    #[error("failed to determine cache directory; is $HOME set?")]
    DetermineCacheDir,

    /// The cache directory could not be created.
    #[error("failed to create cache directory {}", .cache_dir.display())]
    CreateCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// The cache directory could not be removed.
    #[error("failed to remove cache directory {}", .cache_dir.display())]
    RemoveCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// The cache directory could not be recreated.
    #[error("failed to recreate cache directory {}", .cache_dir.display())]
    RecreateCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// The known-good version manifest could not be requested.
    #[error("failed to request versions for {version_request:?}")]
    RequestVersions {
        /// The requested version selection.
        version_request: VersionRequest,
    },

    /// No known-good version matched the requested selection.
    #[error("could not determine a version for {version_request:?}")]
    NoMatchingVersion {
        /// The requested version selection.
        version_request: VersionRequest,
    },

    /// No Chrome binary was requested for a download operation.
    #[error("at least one Chrome binary must be requested")]
    EmptyChromeBinaryDownloadRequest,

    /// No Chrome download exists for the selected version and platform.
    #[error("no chrome download for version {version} on {platform}")]
    NoChromeDownload {
        /// The selected Chrome version.
        version: Version,
        /// The detected platform.
        platform: Platform,
    },

    /// No Chrome Headless Shell download exists for the selected version and platform.
    #[error("no chrome-headless-shell download for version {version} on {platform}")]
    NoChromeHeadlessShellDownload {
        /// The selected Chrome version.
        version: Version,
        /// The detected platform.
        platform: Platform,
    },

    /// No Chromedriver download exists for the selected version and platform.
    #[error("no chromedriver download for version {version} on {platform}")]
    NoChromedriverDownload {
        /// The selected Chrome version.
        version: Version,
        /// The detected platform.
        platform: Platform,
    },

    /// The platform-specific package directory could not be created.
    #[error("failed to create platform directory {}", .platform_dir.display())]
    CreatePlatformDir {
        /// The platform-specific package directory.
        platform_dir: PathBuf,
    },

    /* Downloads and archives. */
    /// The download request failed or returned a non-success status.
    #[error("failed to download {artifact} from {url}")]
    Download {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The download URL.
        url: String,
    },

    /// The downloaded archive file could not be created.
    #[error("failed to create {artifact} download file {}", .path.display())]
    CreateDownloadFile {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
    },

    /// A chunk could not be written into the downloaded archive.
    #[error("failed to write {artifact} download chunk")]
    WriteDownloadFile {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
    },

    /// The downloaded archive file could not be flushed.
    #[error("failed to flush {artifact} download file")]
    FlushDownloadFile {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
    },

    /// The download stalled for too long.
    #[error(
        "{artifact} download timed out after {consecutive_stalls} consecutive stalls of {chunk_timeout:?}"
    )]
    DownloadStalled {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The number of consecutive stalls observed.
        consecutive_stalls: u32,
        /// The timeout for each stall.
        chunk_timeout: Duration,
    },

    /// The downloaded archive could not be opened.
    #[error("failed to open downloaded ZIP archive {}", .path.display())]
    OpenDownloadedZip {
        /// The archive path.
        path: PathBuf,
    },

    /// The downloaded archive was not a valid ZIP file.
    #[error("downloaded file {} is not a valid ZIP archive", .path.display())]
    InvalidZip {
        /// The archive path.
        path: PathBuf,
    },

    /// The downloaded archive exceeded the decompressed size safety limit.
    #[error(
        "downloaded ZIP archive {} decompressed size {size} exceeds safety limit {max_size}",
        .path.display()
    )]
    ZipTooLarge {
        /// The archive path.
        path: PathBuf,
        /// The reported decompressed size in bytes.
        size: u128,
        /// The configured maximum decompressed size in bytes.
        max_size: u128,
    },

    /// The downloaded archive could not be extracted.
    #[error(
        "failed to extract ZIP archive {} to {}",
        .path.display(),
        .unpack_dir.display()
    )]
    ExtractZip {
        /// The archive path.
        path: PathBuf,
        /// The destination directory.
        unpack_dir: PathBuf,
    },

    /// The downloaded archive could not be removed after extraction.
    #[error("failed to remove downloaded ZIP archive {}", .path.display())]
    RemoveDownloadedZip {
        /// The archive path.
        path: PathBuf,
    },

    /* Chromedriver process lifecycle. */
    /// The chromedriver process could not be spawned.
    #[error("failed to spawn chromedriver process {}", .path.display())]
    SpawnChromedriver {
        /// The chromedriver executable path.
        path: PathBuf,
    },

    /// The browser process could not be spawned.
    #[error("failed to spawn browser process {}", .path.display())]
    SpawnBrowser {
        /// The browser executable path.
        path: PathBuf,
    },

    /// A Chrome Headless Shell session was configured with an unsupported remote debugging arg.
    #[error(
        "Chrome Headless Shell sessions require a TCP remote debugging port; unsupported argument {arg:?}"
    )]
    UnsupportedHeadlessShellRemoteDebuggingArg {
        /// The unsupported browser argument.
        arg: String,
    },

    /// A Chrome Headless Shell session was configured with an invalid remote debugging port arg.
    #[error(
        "Chrome Headless Shell sessions require --remote-debugging-port=<0-65535>; invalid argument {arg:?}"
    )]
    InvalidHeadlessShellRemoteDebuggingPortArg {
        /// The invalid browser argument.
        arg: String,
    },

    /// A Chrome Headless Shell session was configured with multiple remote debugging port args.
    #[error(
        "Chrome Headless Shell sessions require exactly one TCP remote debugging port; conflicting arguments {first_arg:?} and {second_arg:?}"
    )]
    ConflictingHeadlessShellRemoteDebuggingArgs {
        /// The first configured remote debugging port argument.
        first_arg: String,
        /// The second configured remote debugging port argument.
        second_arg: String,
    },

    /// Chromedriver did not report startup before the timeout.
    #[error("failed while waiting for chromedriver {} to start", .path.display())]
    WaitForChromedriverStartup {
        /// The chromedriver executable path.
        path: PathBuf,
    },

    /// The browser process did not report `DevTools` startup before the timeout.
    #[error("failed while waiting for browser {} to expose DevTools", .path.display())]
    WaitForBrowserStartup {
        /// The browser executable path.
        path: PathBuf,
    },

    /// An initial browser page could not be created through the `DevTools` endpoint.
    #[error("failed to create initial browser page through DevTools at {debugger_address}")]
    CreateInitialBrowserPage {
        /// The `DevTools` HTTP endpoint address.
        debugger_address: String,
    },

    /// The chromedriver process could not be terminated.
    #[error("failed to terminate chromedriver process on port {port}")]
    TerminateChromedriver {
        /// The chromedriver port.
        port: Port,
    },

    /// The browser process could not be terminated.
    #[error("failed to terminate browser process attached at {debugger_address}")]
    TerminateBrowser {
        /// The `DevTools` HTTP endpoint address.
        debugger_address: String,
    },

    /* Session lifecycle. */
    /// Chrome capabilities could not be prepared.
    #[error(
        "failed to prepare Chrome capabilities for {}",
        .browser_executable.display()
    )]
    PrepareChromeCapabilities {
        /// The browser executable path.
        browser_executable: PathBuf,
    },

    /// User-provided capability setup failed.
    #[error("failed to configure Chrome capabilities")]
    ConfigureSessionCapabilities,

    /// The `WebDriver` session could not be started.
    #[error("failed to start WebDriver session on port {port}")]
    StartWebDriverSession {
        /// The chromedriver port.
        port: Port,
    },

    /// User-provided session callback returned an error.
    #[error("session callback failed")]
    RunSessionCallback,

    /// The `WebDriver` session could not be closed.
    #[error("failed to quit WebDriver session")]
    QuitSession,
}
