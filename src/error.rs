//! Typed error contexts for all fallible operations.
//!
//! Variants carry the operational evidence needed to understand a failure; underlying causes and
//! secondary cleanup failures are attached through `rootcause` report children.

use crate::{BrowserArtifactRequest, ChromeBinary, Port, VersionRequest};
use ::chrome_for_testing::{Platform, Version};
use rootcause::Report;
use std::{
    fmt::{Display, Formatter},
    path::PathBuf,
    time::Duration,
};
use thiserror::Error;
use tokio::runtime::RuntimeFlavor;

/// Convenience alias for `Result<T, rootcause::Report<ChromeForTestingError>>`.
///
/// Use this in your application's signatures to avoid spelling out the wrapped error type:
///
/// ```no_run
/// use chrome_for_testing_manager::{ChromeForTesting, ChromeForTestingConfig, Result};
///
/// async fn launch() -> Result<ChromeForTesting> {
///     ChromeForTesting::launch(ChromeForTestingConfig::default()).await
/// }
/// ```
pub type Result<T> = std::result::Result<T, Report<ChromeForTestingError>>;

/// The chrome-for-testing artifact involved in an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChromeForTestingArtifact {
    /// The Chrome browser binary package.
    Chrome,

    /// The Chrome Headless Shell binary package.
    ChromeHeadlessShell,

    /// The `ChromeDriver` package.
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
pub enum ChromeForTestingError {
    /// The requested operation was cancelled before it completed.
    #[error("operation cancelled")]
    Cancelled,

    /* Runtime and platform. */
    /// No Tokio runtime is active on the calling task.
    #[error("Chrome for Testing requires an active multi-threaded Tokio runtime; none was found")]
    MissingRuntime,

    /// The current Tokio runtime does not support async drop cleanup.
    #[error(
        "Chrome for Testing requires a multi-threaded Tokio runtime; detected {runtime_flavor:?}"
    )]
    UnsupportedRuntime {
        /// The detected runtime flavor.
        runtime_flavor: RuntimeFlavor,
    },

    /// The current platform is unsupported by chrome-for-testing.
    #[error("unsupported chrome-for-testing platform")]
    UnsupportedPlatform,

    /// An internally owned operation task could not be joined.
    #[error("failed to join {operation} operation task")]
    JoinOperationTask {
        /// The operation performed by the task.
        operation: &'static str,
    },

    /// An HTTP client could not be built.
    #[error("failed to build {purpose} HTTP client")]
    BuildHttpClient {
        /// The operation performed by the client.
        purpose: &'static str,
    },

    /* Cache. */
    /// The cache directory could not be determined.
    #[error("failed to determine cache directory; is $HOME set?")]
    DetermineCacheDir,

    /// The cache directory could not be created.
    #[error("failed to create cache directory {}", .cache_dir.display())]
    CreateCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// A cache coordination lock file could not be opened.
    #[error("failed to open cache lock file {}", .path.display())]
    OpenLockFile {
        /// The lock file path.
        path: PathBuf,
    },

    /// A cache coordination lock could not be acquired.
    #[error("failed to acquire cache lock {}", .path.display())]
    AcquireCacheLock {
        /// The lock file path.
        path: PathBuf,
    },

    /// The cache contains loaded or installing artifacts and cannot currently be cleared.
    #[error("cache is in use and cannot be cleared: {}", .cache_dir.display())]
    CacheInUse {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// Entries in the cache directory could not be enumerated.
    #[error("failed to read cache directory {}", .cache_dir.display())]
    ReadCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// A cache entry could not be removed while clearing the cache.
    #[error("failed to remove cache entry {}", .path.display())]
    RemoveCacheEntry {
        /// The cache entry path.
        path: PathBuf,
    },

    /* Version resolution. */
    /// The known-good version manifest could not be requested.
    #[error("failed to request versions for {version_request:?}")]
    RequestVersions {
        /// The requested version selection.
        version_request: VersionRequest,
    },

    /// No known-good version matched the requested selection.
    #[error(
        "could not determine a version for {version_request:?} satisfying {requested_artifacts:?}"
    )]
    NoMatchingVersion {
        /// The requested version selection.
        version_request: VersionRequest,
        /// Browser artifacts required by the resolution.
        requested_artifacts: BrowserArtifactRequest,
    },

    /// A browser package was requested that was not part of the resolved artifact set.
    #[error("{chrome_binary:?} was not resolved for version {version} on {platform}")]
    BrowserArtifactNotResolved {
        /// The browser package requested by the caller.
        chrome_binary: ChromeBinary,
        /// The selected Chrome version.
        version: Version,
        /// The selected platform.
        platform: Platform,
    },

    /// A selected version was passed to a manager targeting another platform.
    #[error("selected version targets {selected}, but this manager targets {manager}")]
    SelectedVersionPlatformMismatch {
        /// Platform stored in the selected version.
        selected: Platform,
        /// Platform targeted by the manager.
        manager: Platform,
    },

    /// No download exists for the artifact at the selected version and platform.
    #[error("no {artifact} download for version {version} on {platform}")]
    NoArtifactDownload {
        /// The artifact missing a download.
        artifact: ChromeForTestingArtifact,
        /// The selected Chrome version.
        version: Version,
        /// The detected platform.
        platform: Platform,
    },

    /* Installation. */
    /// An artifact's unique staging directory could not be created.
    #[error("failed to create staging directory {}", .path.display())]
    CreateStagingDir {
        /// The staging directory path.
        path: PathBuf,
    },

    /// Stale staging data or an incomplete package could not be removed.
    #[error("failed to remove stale artifact data {}", .path.display())]
    RemoveStaleArtifact {
        /// The stale artifact path.
        path: PathBuf,
    },

    /// The expected package root could not be derived from an executable path.
    #[error("invalid package executable path {}", .path.display())]
    InvalidPackageExecutablePath {
        /// The invalid relative executable path.
        path: PathBuf,
    },

    /// Extraction completed without producing the expected executable.
    #[error("extracted package is missing executable {}", .path.display())]
    MissingExtractedExecutable {
        /// The expected executable path.
        path: PathBuf,
    },

    /// An artifact completion marker could not be written.
    #[error("failed to write artifact completion marker {}", .path.display())]
    WriteCompletionMarker {
        /// The marker path.
        path: PathBuf,
    },

    /// A completed package could not be atomically installed.
    #[error("failed to atomically install package from {} to {}", .from.display(), .to.display())]
    InstallCompletedPackage {
        /// The completed staging package path.
        from: PathBuf,
        /// The final package path.
        to: PathBuf,
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

    /// The downloaded archive could not be written to disk.
    #[error("failed to write {artifact} download file {}", .path.display())]
    WriteDownloadFile {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
    },

    /// The downloaded archive exceeded the download size safety limit.
    #[error("{artifact} download from {url} exceeds the safety limit of {max_size} bytes")]
    DownloadTooLarge {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The download URL.
        url: String,
        /// The configured maximum download size in bytes.
        max_size: u64,
    },

    /// The downloaded archive could not be opened or was not a valid ZIP file.
    #[error("downloaded file {} is not a readable ZIP archive", .path.display())]
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
        size: u64,
        /// The configured maximum decompressed size in bytes.
        max_size: u64,
    },

    /// The downloaded archive contained more entries than the safety limit allows.
    #[error(
        "downloaded ZIP archive {} contains {entries} entries, exceeding safety limit {max_entries}",
        .path.display()
    )]
    ZipTooManyEntries {
        /// The archive path.
        path: PathBuf,
        /// The number of entries reported by the archive.
        entries: u64,
        /// The configured maximum number of entries.
        max_entries: u64,
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

    /* Process lifecycle. */
    /// A managed child process could not be spawned.
    #[error("failed to spawn {artifact} process {}", .path.display())]
    SpawnProcess {
        /// The artifact whose process could not be spawned.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
    },

    /// A Chrome Headless Shell session was configured with an unusable remote debugging arg.
    #[error(
        "Chrome Headless Shell sessions require --remote-debugging-port=<0-65535> over TCP; unsupported argument {arg:?}"
    )]
    InvalidHeadlessShellRemoteDebuggingArg {
        /// The unsupported browser argument.
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

    /// A Chrome option cannot be applied when attaching to an already-running Headless Shell.
    #[error(
        "Chrome Headless Shell cannot apply goog:chromeOptions.{option} through an attached session"
    )]
    UnsupportedHeadlessShellCapability {
        /// Unsupported Chrome option name.
        option: String,
    },

    /// A managed child process closed its startup output before reporting readiness.
    #[error("{artifact} {} closed its startup output before reporting readiness", .path.display())]
    StartupOutputClosed {
        /// The artifact whose process closed its output.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
    },

    /// `ChromeDriver` reported a different port than the one requested.
    #[error(
        "chromedriver {} reported port {reported}, but port {requested} was requested",
        .path.display()
    )]
    ChromeDriverPortMismatch {
        /// The chromedriver executable path.
        path: PathBuf,
        /// The fixed port supplied to `ChromeDriver`.
        requested: Port,
        /// The port reported by the spawned process.
        reported: Port,
    },

    /// A managed child process exited before it became ready.
    #[error("{artifact} {} exited during startup with {status}", .path.display())]
    ExitedDuringStartup {
        /// The artifact whose process exited.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
        /// Exit status observed during readiness probing.
        status: std::process::ExitStatus,
    },

    /// A managed child process did not report startup before the timeout.
    #[error("{artifact} {} did not report startup within {timeout:?}", .path.display())]
    WaitForStartup {
        /// The artifact whose process did not start in time.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
        /// The startup deadline.
        timeout: Duration,
    },

    /// An initial browser page could not be created through the `DevTools` endpoint.
    #[error("failed to create initial browser page through DevTools at {debugger_address}")]
    CreateInitialBrowserPage {
        /// The `DevTools` HTTP endpoint address.
        debugger_address: String,
    },

    /// A spawned process could not be terminated during startup cleanup.
    #[error("failed to terminate {artifact} during startup cleanup for {}", .path.display())]
    TerminateDuringStartup {
        /// The artifact whose process could not be terminated.
        artifact: ChromeForTestingArtifact,
        /// The spawned executable path.
        path: PathBuf,
    },

    /// A managed child process could not be terminated.
    #[error("failed to terminate {artifact} process")]
    TerminateProcess {
        /// The artifact whose process could not be terminated.
        artifact: ChromeForTestingArtifact,
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

    /// Browser-session cleanup exceeded its independent lifecycle deadline.
    #[error("browser-session cleanup exceeded {timeout:?}")]
    SessionCleanupTimeout {
        /// Cleanup deadline that elapsed.
        timeout: Duration,
    },
}

pub(crate) fn operation_result_with_cleanup<T, U>(
    operation_result: Result<T>,
    cleanup_result: Result<U>,
) -> Result<T> {
    match operation_result {
        Ok(value) => cleanup_result.map(|_| value),
        Err(mut operation_err) => {
            if let Err(cleanup_err) = cleanup_result {
                operation_err
                    .children_mut()
                    .push(cleanup_err.into_dynamic().into_cloneable());
            }
            Err(operation_err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChromeForTestingError, operation_result_with_cleanup};
    use assertr::prelude::*;
    use rootcause::report;
    use std::path::PathBuf;

    #[test]
    fn cancellation_remains_primary_when_cleanup_fails() {
        let result = operation_result_with_cleanup::<(), ()>(
            Err(report!(ChromeForTestingError::Cancelled)),
            Err(report!(ChromeForTestingError::RemoveStaleArtifact {
                path: PathBuf::from("staging"),
            })),
        );
        let error = result.expect_err("operation and cleanup both failed");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        assert_that!(error.children().into_iter().count()).is_equal_to(1);
    }
}
