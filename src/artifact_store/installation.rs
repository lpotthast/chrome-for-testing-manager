//! Atomic installation transactions for individual browser or driver artifacts.
//!
//! Each installation validates an already-installed package through cheap metadata (completion
//! marker plus executable size) before taking its artifact lock. Actual installation holds an
//! artifact-specific exclusive lock, stages and validates content, then publishes with a
//! same-filesystem rename.
//!
//! Every temporary entry in a platform directory carries its artifact name between `.`
//! delimiters (`.staging.<artifact>.<pid>.<seq>`, `.trash.<artifact>.<pid>.<seq>`). An installation
//! only ever touches entries of its own artifact, which it may do because it holds that artifact's
//! lock. Packages are removed by renaming them to a trash entry first, so an interrupted removal
//! never leaves a partial package that could pass validation.
//!
//! The locked part of an installation runs as a Tokio task owning the artifact lock and a cache
//! lease. A dropped caller cancels it through its token, but cannot release the lock or the lease
//! while the transaction's file-system work (including its blocking extraction worker) is still in
//! flight: the task rolls back first, so the next holder of the lock never races a detached writer.

use super::{ArtifactStore, download, extract};
use crate::cache::{self, CacheLease};
use crate::error::operation_result_with_cleanup;
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use ::chrome_for_testing::{Platform, Version};
use rootcause::{Report, bail, prelude::ResultExt};
use std::io;
use std::path::{Component, Path, PathBuf};
use tokio::fs;

pub(crate) const COMPLETION_MARKER: &str = ".chrome-for-testing-manager-complete";
/// Version of the completion marker format. A different marker invalidates installed packages, so
/// prefer bumping the cache layout directory over bumping this within one layout.
const MARKER_SCHEMA: u32 = 3;

/// One artifact to install: where to download it from and where its executable lives inside the
/// package (relative to the platform directory, starting with the package's root directory).
pub(super) struct ArtifactRequest {
    pub(super) artifact: ChromeForTestingArtifact,
    pub(super) url: String,
    pub(super) executable: &'static Path,
}

/// Paths and identity of one package installation, derived once from its request.
struct InstallPlan<'a> {
    version: Version,
    request: &'a ArtifactRequest,
    platform_dir: PathBuf,
    /// The package's top-level directory, relative to the platform directory. Always a single
    /// normal path component.
    package_root: &'a Path,
    final_package: PathBuf,
    final_executable: PathBuf,
    identity: String,
}

impl<'a> InstallPlan<'a> {
    fn new(
        cache_dir: &Path,
        platform: Platform,
        version: Version,
        request: &'a ArtifactRequest,
    ) -> Result<Self> {
        let package_root = match request.executable.components().next() {
            Some(Component::Normal(root)) => Path::new(root),
            _ => bail!(ChromeForTestingError::InvalidPackageExecutablePath {
                path: request.executable.to_owned(),
            }),
        };
        let platform_dir = cache_dir
            .join(version.to_string())
            .join(platform.to_string());
        Ok(Self {
            version,
            request,
            final_package: platform_dir.join(package_root),
            final_executable: platform_dir.join(request.executable),
            platform_dir,
            package_root,
            identity: format!(
                "schema={MARKER_SCHEMA}\nartifact={}\nversion={version}\nplatform={platform}\n",
                request.artifact
            ),
        })
    }

    fn artifact(&self) -> ChromeForTestingArtifact {
        self.request.artifact
    }

    /// Prefix of this artifact's staging directories. The trailing delimiter keeps `chrome` from
    /// matching `chrome-headless-shell`.
    fn staging_prefix(&self) -> String {
        format!(".staging.{}.", self.artifact())
    }

    /// Prefix of this artifact's trash entries.
    fn trash_prefix(&self) -> String {
        format!(".trash.{}.", self.artifact())
    }

    /// Whether the published package is complete.
    ///
    /// Cheap metadata validation: the executable must exist as a regular file whose size matches
    /// the completion marker written at install time. Deliberately no content hashing: the marker
    /// is written only after a fully successful extraction, and re-hashing hundreds of megabytes on
    /// every cache hit would defeat the lock-free fast path. The cache is a per-user directory;
    /// this validation detects incomplete installs, not deliberate tampering by an actor who could
    /// equally rewrite the marker.
    ///
    /// Missing, mistyped (e.g. a file where a directory belongs), or mismatching entries mean
    /// "incomplete", so the package is replaced. Other I/O errors are returned, so that a transient
    /// failure never causes a package in use to be replaced.
    async fn package_is_complete(&self) -> io::Result<bool> {
        let incomplete = |error: &io::Error| {
            matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::NotADirectory
                    | io::ErrorKind::IsADirectory
                    | io::ErrorKind::InvalidData
            )
        };
        let metadata = match fs::metadata(&self.final_executable).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(false),
            Err(error) if incomplete(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let marker_path = self.final_package.join(COMPLETION_MARKER);
        // Check the entry type first: Windows reports reading a directory as `PermissionDenied`,
        // which must not be mistaken for an incomplete package.
        match fs::metadata(&marker_path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Ok(false),
            Err(error) if incomplete(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
        let marker = match fs::read_to_string(&marker_path).await {
            Ok(marker) => marker,
            Err(error) if incomplete(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(validation) = marker.strip_prefix(&self.identity) else {
            return Ok(false);
        };
        let executable_size = validation
            .lines()
            .find_map(|line| line.strip_prefix("executable_size="))
            .and_then(|value| value.parse::<u64>().ok());
        Ok(executable_size == Some(metadata.len()))
    }

    fn marker_contents(&self, executable_size: u64) -> String {
        format!("{}executable_size={executable_size}\n", self.identity)
    }
}

impl ArtifactStore {
    /// Install `request` if present. On failure, cancel the sibling installations sharing
    /// `siblings`, so that a transaction fails fast.
    ///
    /// The installation runs as a task holding a clone of `cache_lease`; see the module docs.
    pub(super) async fn install_artifact_or_cancel(
        &self,
        version: Version,
        request: Option<ArtifactRequest>,
        cache_lease: &CacheLease,
        siblings: &CancellationToken,
    ) -> Result<Option<PathBuf>> {
        let Some(request) = request else {
            return Ok(None);
        };
        let store = self.clone();
        let cache_lease = cache_lease.clone();
        let cancellation = siblings.clone();
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let installation = tokio::spawn(async move {
            let result = store
                .install_artifact(version, &request, cancellation)
                .await;
            drop(cache_lease);
            if let Err(Err(error)) = result_sender.send(result) {
                // The installation future was dropped, so nobody receives the rollback's outcome.
                // A plain cancellation is the expected outcome. Anything else would be lost.
                let plain_cancellation =
                    matches!(error.current_context(), ChromeForTestingError::Cancelled)
                        && error.children().is_empty();
                if !plain_cancellation {
                    tracing::warn!(
                        artifact = %request.artifact,
                        %version,
                        %error,
                        "background rollback of a dropped installation failed"
                    );
                }
            }
        });
        let result = match installation.await {
            Ok(()) => result_receiver
                .await
                .expect("a completed installation task sends its result"),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            // Only possible while the runtime shuts down.
            Err(error) => {
                Err(Report::new_sendsync(error).context(ChromeForTestingError::Cancelled))
            }
        };
        if result.is_err() {
            siblings.cancel();
        }
        result.map(Some)
    }

    /// Install one package transactionally and return its final executable path.
    async fn install_artifact(
        &self,
        version: Version,
        request: &ArtifactRequest,
        cancellation: CancellationToken,
    ) -> Result<PathBuf> {
        let plan = InstallPlan::new(self.cache_dir.path(), self.platform, version, request)?;

        // Fast path: an already-installed package is validated through metadata only, without
        // taking the artifact lock, so concurrent launches never serialize on cache hits. Errors
        // fall through to the locked re-check, which reports them.
        if plan.package_is_complete().await.unwrap_or(false) {
            return Ok(plan.final_executable);
        }

        let _artifact_lock = self
            .cache_dir
            .acquire_artifact(version, self.platform, plan.artifact(), &cancellation)
            .await?;
        crate::check_cancelled(&cancellation)?;

        // Re-check under the lock: a concurrent installer may have completed the package while
        // this caller waited for the lock.
        let complete = plan.package_is_complete().await.context(
            ChromeForTestingError::ValidateInstalledPackage {
                path: plan.final_package.clone(),
            },
        )?;
        if complete {
            tracing::info!(
                artifact = %plan.artifact(),
                %version,
                path = %plan.final_executable.display(),
                "artifact already installed"
            );
            return Ok(plan.final_executable);
        }

        fs::create_dir_all(&plan.platform_dir).await.context(
            ChromeForTestingError::CreateCacheDir {
                cache_dir: plan.platform_dir.clone(),
            },
        )?;
        Self::remove_leftovers(&plan).await;
        cache::remove_tree(&plan.final_package, &plan.trash_prefix())
            .await
            .context(ChromeForTestingError::RemoveStaleArtifact {
                path: plan.final_package.clone(),
            })?;
        crate::check_cancelled(&cancellation)?;

        let staging = cache::create_unique_dir(&plan.platform_dir, &plan.staging_prefix())
            .await
            .context(ChromeForTestingError::CreateStagingDir {
                path: plan.platform_dir.clone(),
            })?;
        let install_result = self
            .install_in_staging(&plan, &staging, &cancellation)
            .await;
        let cleanup_result = cache::retry_while_locked(|| fs::remove_dir_all(&staging))
            .await
            .or_else(cache::ignore_not_found);
        match (install_result, cleanup_result) {
            (Ok(()), Err(error)) => {
                // The package is published already; the leftover only occupies disk space until
                // the next installation of this artifact or a cache clear removes it.
                tracing::warn!(
                    path = %staging.display(),
                    %error,
                    "failed to remove the staging directory of an installed package"
                );
            }
            (install_result, cleanup_result) => operation_result_with_cleanup(
                install_result,
                cleanup_result
                    .context(ChromeForTestingError::RemoveStaleArtifact { path: staging }),
            )?,
        }
        Ok(plan.final_executable)
    }

    async fn install_in_staging(
        &self,
        plan: &InstallPlan<'_>,
        staging: &Path,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let artifact = plan.artifact();
        let version = plan.version;
        tracing::info!(artifact = %artifact, %version, "installing artifact");
        let archive_path = staging.join(format!("{artifact}.zip"));
        download::download_artifact_archive(
            &self.client,
            &plan.request.url,
            &archive_path,
            artifact,
            self.artifact_timeout,
            cancellation,
        )
        .await?;

        let unpack_dir = staging.join("unpacked");
        extract::extract_zip(
            artifact,
            archive_path.clone(),
            unpack_dir.clone(),
            plan.package_root.to_owned(),
            cancellation.clone(),
        )
        .await?;
        // Free the archive's disk space right away. Best effort: the staging cleanup removes it
        // too, but a crash after publication would leave it behind until the cache is cleared.
        if let Err(error) = cache::retry_while_locked(|| fs::remove_file(&archive_path)).await {
            tracing::debug!(path = %archive_path.display(), %error, "failed to remove extracted archive");
        }

        // Successful extraction is the commit point: the remaining validation and publication
        // steps are cheap, so the transaction completes even when cancellation arrives now,
        // making the installed artifact reusable by the next run.
        let staged_executable = unpack_dir.join(plan.request.executable);
        let missing_executable = || ChromeForTestingError::MissingExtractedExecutable {
            path: staged_executable.clone(),
        };
        let executable_metadata = match fs::metadata(&staged_executable).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => bail!(missing_executable()),
            Err(error) => return Err(Report::new_sendsync(error).context(missing_executable())),
        };

        // Extraction rejects a symlink at the package root and never creates an entry beneath a
        // symlink, so the package root holding the executable is a real directory.
        let staged_package = unpack_dir.join(plan.package_root);
        let marker_path = staged_package.join(COMPLETION_MARKER);
        fs::write(
            &marker_path,
            plan.marker_contents(executable_metadata.len()),
        )
        .await
        .context(ChromeForTestingError::WriteCompletionMarker {
            path: marker_path.clone(),
        })?;

        cache::retry_while_locked(|| fs::rename(&staged_package, &plan.final_package))
            .await
            .context(ChromeForTestingError::InstallCompletedPackage {
                from: staged_package,
                to: plan.final_package.clone(),
            })?;
        tracing::info!(
            artifact = %artifact,
            %version,
            path = %plan.final_package.display(),
            "artifact installation complete"
        );
        Ok(())
    }

    /// Remove staging and trash entries that interrupted installs of this artifact left behind.
    ///
    /// Best effort: a leftover that cannot be removed (e.g. a file still open on Windows) must not
    /// block installing the package.
    async fn remove_leftovers(plan: &InstallPlan<'_>) {
        for prefix in [plan.staging_prefix(), plan.trash_prefix()] {
            if let Err(error) = cache::remove_entries_with_prefix(&plan.platform_dir, &prefix).await
            {
                tracing::warn!(
                    artifact = %plan.artifact(),
                    dir = %plan.platform_dir.display(),
                    %error,
                    "failed to remove leftovers of an interrupted installation"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ArtifactRequest, COMPLETION_MARKER, InstallPlan};
    use crate::ChromeForTestingArtifact;
    use crate::artifact_store::ArtifactStore;
    use crate::test_support::TestDirectory;
    use ::chrome_for_testing::Platform;
    use assertr::prelude::*;
    use std::path::Path;

    fn request(artifact: ChromeForTestingArtifact) -> ArtifactRequest {
        ArtifactRequest {
            artifact,
            url: "https://example.invalid/artifact.zip".to_owned(),
            executable: Path::new("package/executable"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chrome_leftover_cleanup_spares_headless_shell_staging() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("installation-leftover-prefixes")?;
        let chrome_request = request(ChromeForTestingArtifact::Chrome);
        let headless_request = request(ChromeForTestingArtifact::ChromeHeadlessShell);
        let version = "135.0.7019.0".parse()?;
        let chrome = InstallPlan::new(
            directory.path(),
            Platform::Linux64,
            version,
            &chrome_request,
        )?;
        let headless = InstallPlan::new(
            directory.path(),
            Platform::Linux64,
            version,
            &headless_request,
        )?;
        let chrome_staging = chrome
            .platform_dir
            .join(format!("{}1.0", chrome.staging_prefix()));
        let headless_staging = headless
            .platform_dir
            .join(format!("{}1.0", headless.staging_prefix()));
        tokio::fs::create_dir_all(&chrome_staging).await?;
        tokio::fs::create_dir_all(&headless_staging).await?;

        ArtifactStore::remove_leftovers(&chrome).await;

        assert_that!(tokio::fs::try_exists(&chrome_staging).await?).is_false();
        assert_that!(tokio::fs::try_exists(&headless_staging).await?).is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mistyped_package_entries_count_as_incomplete() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("installation-mistyped-package")?;
        let chrome_request = request(ChromeForTestingArtifact::Chrome);
        let plan = InstallPlan::new(
            directory.path(),
            Platform::Linux64,
            "135.0.7019.0".parse()?,
            &chrome_request,
        )?;
        tokio::fs::create_dir_all(&plan.platform_dir).await?;

        // A file where the package directory belongs.
        tokio::fs::write(&plan.final_package, "not a directory").await?;
        assert_that!(plan.package_is_complete().await?).is_false();

        // A directory where the marker file belongs.
        tokio::fs::remove_file(&plan.final_package).await?;
        tokio::fs::create_dir_all(plan.final_package.join(COMPLETION_MARKER)).await?;
        tokio::fs::write(&plan.final_executable, "browser").await?;
        assert_that!(plan.package_is_complete().await?).is_false();
        Ok(())
    }
}
