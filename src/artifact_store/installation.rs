//! Atomic installation transactions for individual browser or driver artifacts.
//!
//! Each installer validates already-installed packages through cheap metadata (completion marker
//! plus executable size) before taking any lock. Actual installation holds an artifact-specific
//! exclusive lock, stages and validates content, then publishes with a same-filesystem rename.

use super::{download, extract};
use crate::cache::CacheDir;
use crate::error::operation_result_with_cleanup;
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Result};
use ::chrome_for_testing::{Platform, Version};
use rootcause::{Report, bail, prelude::ResultExt};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::fs;

const COMPLETION_MARKER: &str = ".chrome-for-testing-manager-complete";
const MARKER_SCHEMA: u32 = 2;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ArtifactInstaller<'a> {
    pub cache_dir: &'a CacheDir,
    pub client: &'a reqwest::Client,
    pub platform: Platform,
    pub artifact_timeout: Duration,
}

pub(super) struct ArtifactRequest<'a> {
    pub(super) artifact: ChromeForTestingArtifact,
    pub(super) url: &'a str,
    pub(super) executable: &'static Path,
}

impl ArtifactInstaller<'_> {
    /// Install one package transactionally and return its final executable path.
    pub async fn install(
        &self,
        version: Version,
        artifact: ChromeForTestingArtifact,
        url: &str,
        relative_executable: &Path,
        cancellation: CancellationToken,
    ) -> Result<PathBuf> {
        let platform_dir = self
            .cache_dir
            .path()
            .join(version.to_string())
            .join(self.platform.to_string());
        let package_root = Self::package_root(relative_executable)?;
        let final_package = platform_dir.join(package_root);
        let final_executable = platform_dir.join(relative_executable);
        let expected_identity = Self::marker_identity(version, self.platform, artifact, url);

        // Fast path: an already-installed package is validated through metadata only, without
        // taking the artifact lock, so concurrent launches never serialize on cache hits.
        if Self::package_is_complete(&final_package, &final_executable, &expected_identity).await {
            return Ok(final_executable);
        }

        let _artifact_lock = self
            .cache_dir
            .acquire_artifact(version, self.platform, artifact, cancellation.clone())
            .await?;
        crate::check_cancelled(&cancellation)?;

        fs::create_dir_all(&platform_dir)
            .await
            .context(ChromeForTestingError::CreateCacheDir {
                cache_dir: platform_dir.clone(),
            })?;
        Self::remove_stale_staging_dirs(&platform_dir, artifact).await?;

        // Re-check under the lock: a concurrent installer may have completed the package while
        // this caller waited for the lock.
        if Self::package_is_complete(&final_package, &final_executable, &expected_identity).await {
            tracing::info!(
                artifact = %artifact,
                %version,
                path = %final_executable.display(),
                "artifact already installed"
            );
            return Ok(final_executable);
        }

        Self::remove_incomplete_package(&final_package).await?;
        crate::check_cancelled(&cancellation)?;

        let staging = Self::create_unique_staging_dir(&platform_dir, artifact).await?;
        let install_result = self
            .install_in_staging(
                version,
                artifact,
                url,
                relative_executable,
                package_root,
                &expected_identity,
                &staging,
                &final_package,
                &cancellation,
            )
            .await;
        let cleanup_result = Self::remove_staging_dir(&staging).await;
        operation_result_with_cleanup(install_result, cleanup_result)?;
        Ok(final_executable)
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_in_staging(
        &self,
        version: Version,
        artifact: ChromeForTestingArtifact,
        url: &str,
        relative_executable: &Path,
        package_root: &Path,
        expected_identity: &str,
        staging: &Path,
        final_package: &Path,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        tracing::info!(artifact = %artifact, %version, "installing artifact");
        let archive_path = staging.join(format!("{artifact}.zip"));
        download::download_artifact_archive(
            self.client,
            url,
            &archive_path,
            artifact,
            self.artifact_timeout,
            cancellation,
        )
        .await?;

        let unpack_dir = staging.join("unpacked");
        extract::extract_zip(
            archive_path.clone(),
            unpack_dir.clone(),
            package_root.to_owned(),
            cancellation.clone(),
        )
        .await?;

        // Successful extraction is the commit point: the remaining validation and publication
        // steps are cheap, so the transaction completes even when cancellation arrives now,
        // making the installed artifact reusable by the next run.
        let staged_executable = unpack_dir.join(relative_executable);
        let executable_metadata = match fs::metadata(&staged_executable).await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => bail!(ChromeForTestingError::MissingExtractedExecutable {
                path: staged_executable,
            }),
        };

        let staged_package = unpack_dir.join(package_root);
        let package_metadata = fs::symlink_metadata(&staged_package).await.context(
            ChromeForTestingError::MissingExtractedExecutable {
                path: staged_package.clone(),
            },
        )?;
        if !package_metadata.is_dir() || package_metadata.file_type().is_symlink() {
            bail!(ChromeForTestingError::InvalidPackageExecutablePath {
                path: relative_executable.to_owned(),
            });
        }

        let marker_path = staged_package.join(COMPLETION_MARKER);
        fs::write(
            &marker_path,
            Self::marker_contents(expected_identity, executable_metadata.len()),
        )
        .await
        .context(ChromeForTestingError::WriteCompletionMarker {
            path: marker_path.clone(),
        })?;

        fs::remove_file(&archive_path).await.context(
            ChromeForTestingError::RemoveStaleArtifact {
                path: archive_path.clone(),
            },
        )?;

        fs::rename(&staged_package, final_package).await.context(
            ChromeForTestingError::InstallCompletedPackage {
                from: staged_package,
                to: final_package.to_owned(),
            },
        )?;
        tracing::info!(
            artifact = %artifact,
            %version,
            path = %final_package.display(),
            "artifact installation complete"
        );
        Ok(())
    }

    pub(super) async fn install_and_cancel_siblings(
        &self,
        version: Version,
        request: ArtifactRequest<'_>,
        cancellation: CancellationToken,
    ) -> Result<PathBuf> {
        let result = self
            .install(
                version,
                request.artifact,
                request.url,
                request.executable,
                cancellation.clone(),
            )
            .await;
        if result.is_err() {
            cancellation.cancel();
        }
        result
    }

    pub(super) async fn install_optional_and_cancel_siblings(
        &self,
        version: Version,
        request: Option<ArtifactRequest<'_>>,
        cancellation: CancellationToken,
    ) -> Result<Option<PathBuf>> {
        match request {
            Some(request) => self
                .install_and_cancel_siblings(version, request, cancellation)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    fn package_root(relative_executable: &Path) -> Result<&Path> {
        match relative_executable.components().next() {
            Some(Component::Normal(root)) => Ok(Path::new(root)),
            _ => {
                bail!(ChromeForTestingError::InvalidPackageExecutablePath {
                    path: relative_executable.to_owned(),
                });
            }
        }
    }

    /// Cheap metadata validation of an installed package: the executable must exist as a regular
    /// file whose size matches the completion marker written at install time. Deliberately no
    /// content hashing: the marker is written only after a fully successful extraction, and
    /// re-hashing hundreds of megabytes on every cache hit would defeat the lock-free fast path.
    /// The cache is a per-user directory; this validation detects incomplete installs, not
    /// deliberate tampering by an actor who could equally rewrite the marker.
    async fn package_is_complete(
        package: &Path,
        executable: &Path,
        expected_identity: &str,
    ) -> bool {
        let Ok(metadata) = fs::metadata(executable).await else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        let Ok(marker) = fs::read_to_string(package.join(COMPLETION_MARKER)).await else {
            return false;
        };
        let Some(validation) = marker.strip_prefix(expected_identity) else {
            return false;
        };
        let mut executable_size = None;
        for line in validation.lines() {
            if let Some(value) = line.strip_prefix("executable_size=") {
                executable_size = value.parse::<u64>().ok();
            }
        }
        executable_size == Some(metadata.len())
    }

    async fn remove_incomplete_package(package: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(package).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(Report::new_sendsync(error).context(
                    ChromeForTestingError::RemoveStaleArtifact {
                        path: package.to_owned(),
                    },
                ));
            }
        };
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(package).await
        } else {
            fs::remove_file(package).await
        };
        result.context(ChromeForTestingError::RemoveStaleArtifact {
            path: package.to_owned(),
        })
    }

    async fn create_unique_staging_dir(
        platform_dir: &Path,
        artifact: ChromeForTestingArtifact,
    ) -> Result<PathBuf> {
        loop {
            let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = platform_dir.join(format!(
                ".staging-{artifact}-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path).await {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(Report::new_sendsync(error)
                        .context(ChromeForTestingError::CreateStagingDir { path }));
                }
            }
        }
    }

    async fn remove_staging_dir(staging: &Path) -> Result<()> {
        match fs::remove_dir_all(staging).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Report::new_sendsync(error).context(
                ChromeForTestingError::RemoveStaleArtifact {
                    path: staging.to_owned(),
                },
            )),
        }
    }

    async fn remove_stale_staging_dirs(
        platform_dir: &Path,
        artifact: ChromeForTestingArtifact,
    ) -> Result<()> {
        let prefix = format!(".staging-{artifact}-");
        let read_error = || ChromeForTestingError::ReadCacheDir {
            cache_dir: platform_dir.to_owned(),
        };
        let mut entries = fs::read_dir(platform_dir).await.context_with(read_error)?;
        while let Some(entry) = entries.next_entry().await.context_with(read_error)? {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                Self::remove_staging_dir(&entry.path()).await?;
            }
        }
        Ok(())
    }

    fn marker_identity(
        version: Version,
        platform: Platform,
        artifact: ChromeForTestingArtifact,
        url: &str,
    ) -> String {
        format!(
            "schema={MARKER_SCHEMA}\nartifact={artifact}\nversion={version}\nplatform={platform}\nsource_url={url}\n"
        )
    }

    fn marker_contents(identity: &str, executable_size: u64) -> String {
        format!("{identity}executable_size={executable_size}\n")
    }
}

#[cfg(test)]
pub(crate) fn completion_marker_name() -> &'static str {
    COMPLETION_MARKER
}
