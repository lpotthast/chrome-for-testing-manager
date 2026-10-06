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

/// What an HTTP client that failed to build was meant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpClientPurpose {
    /// Requests to the Chrome for Testing release index and artifact downloads.
    ChromeForTesting,

    /// Loopback requests to `ChromeDriver` status and Chrome `DevTools` endpoints.
    LocalProcess,

    /// `WebDriver` commands of managed sessions.
    WebDriver,
}

impl Display for HttpClientPurpose {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChromeForTesting => f.write_str("Chrome for Testing"),
            Self::LocalProcess => f.write_str("local process"),
            Self::WebDriver => f.write_str("WebDriver"),
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
    /// No Tokio runtime is active on the calling task. Every asynchronous operation requires one.
    #[error("Chrome for Testing requires an active Tokio runtime; none was found")]
    MissingRuntime,

    /// The current Tokio runtime is not multi-threaded.
    ///
    /// Guarded processes terminate on drop by blocking a runtime worker, which a current-thread
    /// runtime cannot provide.
    #[error(
        "Chrome for Testing requires a multi-threaded Tokio runtime; detected {runtime_flavor:?}"
    )]
    #[non_exhaustive]
    UnsupportedRuntime {
        /// The detected runtime flavor.
        runtime_flavor: RuntimeFlavor,
    },

    /// Chrome for Testing publishes no builds for the current OS and architecture.
    #[error("Chrome for Testing does not support this platform (os: {os}, arch: {arch})")]
    #[non_exhaustive]
    UnsupportedPlatform {
        /// The detected operating system, as in [`std::env::consts::OS`].
        os: &'static str,
        /// The detected CPU architecture, as in [`std::env::consts::ARCH`].
        arch: &'static str,
    },

    /// An HTTP client could not be built.
    #[error("failed to build {purpose} HTTP client")]
    #[non_exhaustive]
    BuildHttpClient {
        /// What the client is used for.
        purpose: HttpClientPurpose,
    },

    /* Cache. */
    /// The platform's per-user cache directory could not be determined.
    #[error(
        "failed to determine the per-user cache directory; configure a cache directory explicitly"
    )]
    #[non_exhaustive]
    DetermineCacheDir,

    /// The cache directory could not be created.
    #[error("failed to create cache directory {}", .cache_dir.display())]
    #[non_exhaustive]
    CreateCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// A cache coordination lock file could not be opened.
    #[error("failed to open cache lock file {}", .path.display())]
    #[non_exhaustive]
    OpenLockFile {
        /// The lock file path.
        path: PathBuf,
    },

    /// A cache coordination lock could not be acquired.
    #[error("failed to acquire cache lock {}", .path.display())]
    #[non_exhaustive]
    AcquireCacheLock {
        /// The lock file path.
        path: PathBuf,
    },

    /// The cache contains loaded or installing artifacts and cannot currently be cleared or pruned.
    #[error("cache is in use and cannot be cleared or pruned: {}", .cache_dir.display())]
    #[non_exhaustive]
    CacheInUse {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// Entries in the cache directory could not be enumerated.
    #[error("failed to read cache directory {}", .cache_dir.display())]
    #[non_exhaustive]
    ReadCacheDir {
        /// The cache directory path.
        cache_dir: PathBuf,
    },

    /// A cache entry could not be removed while clearing the cache.
    #[error("failed to remove cache entry {}", .path.display())]
    #[non_exhaustive]
    RemoveCacheEntry {
        /// The cache entry path.
        path: PathBuf,
    },

    /* Version resolution. */
    /// The known-good version manifest could not be requested.
    #[error("failed to request the release manifest to resolve version {version_request}")]
    #[non_exhaustive]
    RequestVersions {
        /// The requested version selection.
        version_request: VersionRequest,
    },

    /// No known-good version matched the requested selection.
    #[error(
        "no release matching version {version_request} provides {requested_artifacts} downloads for {platform}"
    )]
    #[non_exhaustive]
    NoMatchingVersion {
        /// The requested version selection.
        version_request: VersionRequest,
        /// The platform the version must provide downloads for.
        platform: Platform,
        /// Browser artifacts required by the resolution.
        requested_artifacts: BrowserArtifactRequest,
    },

    /// A browser package was requested that was not part of the resolved artifact set.
    #[error("{chrome_binary} was not resolved for version {version} on {platform}")]
    #[non_exhaustive]
    BrowserArtifactNotResolved {
        /// The browser package requested by the caller.
        chrome_binary: ChromeBinary,
        /// The selected Chrome version.
        version: Version,
        /// The selected platform.
        platform: Platform,
    },

    /// A selected version was passed to a manager targeting another platform.
    ///
    /// Defensive: every manager targets the detected platform, so a [`crate::SelectedVersion`]
    /// obtained through the public API cannot trigger this.
    #[error("selected version targets {selected}, but this manager targets {manager}")]
    #[non_exhaustive]
    SelectedVersionPlatformMismatch {
        /// Platform stored in the selected version.
        selected: Platform,
        /// Platform targeted by the manager.
        manager: Platform,
    },

    /// No download exists for the artifact at the selected version and platform.
    ///
    /// Defensive: resolution only selects versions providing every requested download, so a
    /// [`crate::SelectedVersion`] obtained through the public API cannot trigger this.
    #[error("no {artifact} download for version {version} on {platform}")]
    #[non_exhaustive]
    NoArtifactDownload {
        /// The artifact missing a download.
        artifact: ChromeForTestingArtifact,
        /// The selected Chrome version.
        version: Version,
        /// The detected platform.
        platform: Platform,
    },

    /* Installation. */
    /// An installed package could not be checked for completeness.
    #[error("failed to validate installed package {}", .path.display())]
    #[non_exhaustive]
    ValidateInstalledPackage {
        /// Installed package directory.
        path: PathBuf,
    },

    /// An artifact's unique staging directory could not be created.
    #[error("failed to create staging directory {}", .path.display())]
    #[non_exhaustive]
    CreateStagingDir {
        /// The staging directory path.
        path: PathBuf,
    },

    /// Stale staging data or an incomplete package could not be removed.
    #[error("failed to remove stale artifact data {}", .path.display())]
    #[non_exhaustive]
    RemoveStaleArtifact {
        /// The stale artifact path.
        path: PathBuf,
    },

    /// The expected package root could not be derived from an executable path.
    #[error("invalid package executable path {}", .path.display())]
    #[non_exhaustive]
    InvalidPackageExecutablePath {
        /// The invalid relative executable path.
        path: PathBuf,
    },

    /// Extraction completed without producing the expected executable.
    #[error("extracted package is missing executable {}", .path.display())]
    #[non_exhaustive]
    MissingExtractedExecutable {
        /// The expected executable path.
        path: PathBuf,
    },

    /// An artifact completion marker could not be written.
    #[error("failed to write artifact completion marker {}", .path.display())]
    #[non_exhaustive]
    WriteCompletionMarker {
        /// The marker path.
        path: PathBuf,
    },

    /// A completed package could not be atomically installed.
    #[error("failed to atomically install package from {} to {}", .from.display(), .to.display())]
    #[non_exhaustive]
    InstallCompletedPackage {
        /// The completed staging package path.
        from: PathBuf,
        /// The final package path.
        to: PathBuf,
    },

    /* Downloads and archives. */
    /// The download request failed or returned a non-success status.
    #[error("failed to download {artifact} from {url}")]
    #[non_exhaustive]
    Download {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The download URL.
        url: String,
    },

    /// The downloaded archive could not be written to disk.
    #[error("failed to write {artifact} download file {}", .path.display())]
    #[non_exhaustive]
    WriteDownloadFile {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
    },

    /// The downloaded archive exceeded the download size safety limit.
    #[error("{artifact} download from {url} exceeds the safety limit of {max_size} bytes")]
    #[non_exhaustive]
    DownloadTooLarge {
        /// The artifact being downloaded.
        artifact: ChromeForTestingArtifact,
        /// The download URL.
        url: String,
        /// The configured maximum download size in bytes.
        max_size: u64,
    },

    /// The downloaded archive could not be opened or was not a valid ZIP file.
    #[error("downloaded {artifact} file {} is not a readable ZIP archive", .path.display())]
    #[non_exhaustive]
    InvalidZip {
        /// The artifact whose archive is invalid.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
    },

    /// The downloaded archive exceeded the decompressed size safety limit.
    #[error(
        "downloaded {artifact} ZIP archive {} decompressed size {size} exceeds safety limit {max_size}",
        .path.display()
    )]
    #[non_exhaustive]
    ZipTooLarge {
        /// The artifact whose archive is too large.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
        /// The decompressed size in bytes extracted before the limit was hit.
        size: u64,
        /// The configured maximum decompressed size in bytes.
        max_size: u64,
    },

    /// The downloaded archive contained more entries than the safety limit allows.
    #[error(
        "downloaded {artifact} ZIP archive {} contains {entries} entries, exceeding safety limit {max_entries}",
        .path.display()
    )]
    #[non_exhaustive]
    ZipTooManyEntries {
        /// The artifact whose archive has too many entries.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
        /// The number of entries reported by the archive.
        entries: u64,
        /// The configured maximum number of entries.
        max_entries: u64,
    },

    /// The downloaded archive could not be extracted.
    #[error(
        "failed to extract {artifact} ZIP archive {} to {}",
        .path.display(),
        .unpack_dir.display()
    )]
    #[non_exhaustive]
    ExtractZip {
        /// The artifact whose archive could not be extracted.
        artifact: ChromeForTestingArtifact,
        /// The archive path.
        path: PathBuf,
        /// The destination directory.
        unpack_dir: PathBuf,
    },

    /* Process lifecycle. */
    /// A managed child process could not be spawned.
    #[error("failed to spawn {artifact} process {}", .path.display())]
    #[non_exhaustive]
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
    #[non_exhaustive]
    InvalidHeadlessShellRemoteDebuggingArg {
        /// The unsupported browser argument.
        arg: String,
    },

    /// A Chrome Headless Shell session was configured with multiple remote debugging port args.
    #[error(
        "Chrome Headless Shell sessions require exactly one TCP remote debugging port; conflicting arguments {first_arg:?} and {second_arg:?}"
    )]
    #[non_exhaustive]
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
    #[non_exhaustive]
    UnsupportedHeadlessShellCapability {
        /// Unsupported Chrome option name.
        option: String,
    },

    /// A managed child process closed its startup output before reporting readiness, but its exit
    /// could not be observed shortly afterwards. If the exit is observed,
    /// [`Self::ExitedDuringStartup`] is reported instead.
    #[error("{artifact} {} closed its startup output before reporting readiness", .path.display())]
    #[non_exhaustive]
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
    #[non_exhaustive]
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
    #[non_exhaustive]
    ExitedDuringStartup {
        /// The artifact whose process exited.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
        /// Exit status observed during readiness probing.
        status: std::process::ExitStatus,
    },

    /// A managed child process did not print its startup line before the timeout.
    #[error("{artifact} {} did not report startup within {timeout:?}", .path.display())]
    #[non_exhaustive]
    WaitForStartup {
        /// The artifact whose process did not start in time.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
        /// The startup deadline.
        timeout: Duration,
    },

    /// The output of a managed child process could not be read during startup.
    #[error("failed to read startup output of {artifact} {}", .path.display())]
    #[non_exhaustive]
    ReadStartupOutput {
        /// The artifact whose output could not be read.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
    },

    /// A managed child process printed its startup line but no recognizable value in it.
    #[error("{artifact} {} reported startup in an unrecognized format: {line:?}", .path.display())]
    #[non_exhaustive]
    UnrecognizedStartupOutput {
        /// The artifact whose startup line could not be parsed.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
        /// The unparsable startup line.
        line: String,
    },

    /// `ChromeDriver` reported startup, but its status endpoint did not report readiness before the
    /// startup deadline.
    #[error(
        "chromedriver {} on port {port} did not report a ready /status within {timeout:?}",
        .path.display()
    )]
    #[non_exhaustive]
    ChromeDriverNotReady {
        /// The chromedriver executable path.
        path: PathBuf,
        /// The port the status endpoint was probed on.
        port: Port,
        /// The startup deadline.
        timeout: Duration,
    },

    /// An initial browser page could not be created through the `DevTools` endpoint.
    #[error("failed to create initial browser page through DevTools at {debugger_address}")]
    #[non_exhaustive]
    CreateInitialBrowserPage {
        /// The `DevTools` HTTP endpoint address.
        debugger_address: String,
    },

    /// A managed child process could not be terminated, neither gracefully nor by force.
    #[error("failed to terminate {artifact} process {}", .path.display())]
    #[non_exhaustive]
    TerminateProcess {
        /// The artifact whose process could not be terminated.
        artifact: ChromeForTestingArtifact,
        /// The executable path.
        path: PathBuf,
    },

    /* Session lifecycle. */
    /// Chrome capabilities could not be prepared.
    #[error(
        "failed to prepare Chrome capabilities for {}",
        .browser_executable.display()
    )]
    #[non_exhaustive]
    PrepareChromeCapabilities {
        /// The browser executable path.
        browser_executable: PathBuf,
    },

    /// User-provided capability setup failed.
    #[error("failed to configure Chrome capabilities")]
    #[non_exhaustive]
    ConfigureSessionCapabilities,

    /// The `WebDriver` session could not be started.
    #[error("failed to start WebDriver session on port {port}")]
    #[non_exhaustive]
    StartWebDriverSession {
        /// The chromedriver port.
        port: Port,
    },

    /// User-provided session callback returned an error.
    #[error("session callback failed")]
    #[non_exhaustive]
    RunSessionCallback,

    /// The `WebDriver` session could not be closed.
    #[error("failed to quit WebDriver session")]
    #[non_exhaustive]
    QuitSession,

    /// The `WebDriver` session did not answer the quit request within the
    /// `session_cleanup_timeout` of [`crate::LifecyclePolicy`]; it was abandoned.
    #[error("WebDriver session did not quit within {timeout:?}")]
    #[non_exhaustive]
    QuitSessionTimeout {
        /// The session cleanup deadline.
        timeout: Duration,
    },

    /// Cleanups that dropped session runs handed to the runtime failed. Each failure is attached.
    #[error("failed to clean up {failures} dropped session run(s)")]
    #[non_exhaustive]
    DroppedSessionCleanup {
        /// The number of failed cleanups.
        failures: usize,
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
                attach_child(&mut operation_err, cleanup_err);
            }
            Err(operation_err)
        }
    }
}

/// Attach `child` beneath `report` as a secondary failure.
pub(crate) fn attach_child<C: ?Sized>(
    report: &mut Report<ChromeForTestingError>,
    child: Report<C>,
) {
    report
        .children_mut()
        .push(child.into_dynamic().into_cloneable());
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
