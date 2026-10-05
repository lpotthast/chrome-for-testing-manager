//! Cache directory ownership, leases, pruning, and mutation coordination.
//!
//! Cache operations retain typed leases while installed artifacts are in use. The underlying
//! file-lock state machine lives in the private [`lock`] module.

mod lock;

pub(crate) use self::lock::{ArtifactLock, CacheLease};
use self::lock::{CacheMutationGuard, TryAcquire, UnlockedFileLock};
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError};
use ::chrome_for_testing::{Platform, Version};
use rootcause::{Report, bail, option_ext::OptionExt, prelude::ResultExt};
use std::path::{Path, PathBuf};
use tokio::fs;

const LOCKS_DIR: &str = ".locks";
const CACHE_LOCK: &str = "cache.lock";

#[derive(Debug, Clone)]
pub(crate) struct CacheDir(PathBuf);

/// Summary returned after pruning unretained version directories from the cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachePruneResult {
    removed_versions: Vec<Version>,
}

impl CachePruneResult {
    /// Return the version directories removed by the prune operation.
    #[must_use]
    pub fn removed_versions(&self) -> &[Version] {
        &self.removed_versions
    }

    /// Return whether pruning found no unretained version directories.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.removed_versions.is_empty()
    }
}

impl CacheDir {
    pub fn get_or_create() -> Result<Self, Report<ChromeForTestingError>> {
        let project_dirs = directories::ProjectDirs::from("", "", "chrome-for-testing-manager")
            .context(ChromeForTestingError::DetermineCacheDir)?;
        Self::create_at(project_dirs.cache_dir().to_owned())
    }

    pub fn create_at(cache_dir: PathBuf) -> Result<Self, Report<ChromeForTestingError>> {
        std::fs::create_dir_all(cache_dir.join(LOCKS_DIR)).context(
            ChromeForTestingError::CreateCacheDir {
                cache_dir: cache_dir.clone(),
            },
        )?;
        Ok(Self(cache_dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub async fn acquire_shared(
        &self,
        cancellation: CancellationToken,
    ) -> Result<CacheLease, Report<ChromeForTestingError>> {
        UnlockedFileLock::open(self.lock_path(CacheLockKey::Cache))
            .await?
            .wait_shared(cancellation)
            .await
    }

    pub async fn acquire_artifact(
        &self,
        version: Version,
        platform: Platform,
        artifact: ChromeForTestingArtifact,
        cancellation: CancellationToken,
    ) -> Result<ArtifactLock, Report<ChromeForTestingError>> {
        UnlockedFileLock::open(self.lock_path(CacheLockKey::Artifact {
            version,
            platform,
            artifact,
        }))
        .await?
        .wait_exclusive(cancellation)
        .await
    }

    /// Clear installed artifacts without removing the lock namespace used to coordinate callers.
    pub async fn clear(&self) -> Result<(), Report<ChromeForTestingError>> {
        let _clear_guard = self.try_mutation_guard().await?;

        let mut entries =
            fs::read_dir(self.path())
                .await
                .context(ChromeForTestingError::ReadCacheDir {
                    cache_dir: self.0.clone(),
                })?;
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .context(ChromeForTestingError::ReadCacheDir {
                    cache_dir: self.0.clone(),
                })?
        {
            if entry.file_name() == LOCKS_DIR {
                continue;
            }

            Self::remove_entry(entry.path()).await?;
        }

        Ok(())
    }

    /// Remove cached version directories except for explicitly retained versions.
    pub async fn prune(
        &self,
        retained_versions: &[Version],
    ) -> Result<CachePruneResult, Report<ChromeForTestingError>> {
        let _prune_guard = self.try_mutation_guard().await?;
        let mut removed_versions = Vec::new();
        let mut entries =
            fs::read_dir(self.path())
                .await
                .context(ChromeForTestingError::ReadCacheDir {
                    cache_dir: self.0.clone(),
                })?;
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .context(ChromeForTestingError::ReadCacheDir {
                    cache_dir: self.0.clone(),
                })?
        {
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(version) = file_name.parse::<Version>() else {
                continue;
            };
            if retained_versions.contains(&version) {
                continue;
            }
            Self::remove_entry(entry.path()).await?;
            removed_versions.push(version);
        }
        removed_versions.sort_unstable();
        Ok(CachePruneResult { removed_versions })
    }

    async fn try_mutation_guard(
        &self,
    ) -> Result<CacheMutationGuard, Report<ChromeForTestingError>> {
        match UnlockedFileLock::open(self.lock_path(CacheLockKey::Cache))
            .await?
            .try_exclusive()?
        {
            TryAcquire::Acquired(guard) => Ok(guard),
            TryAcquire::Contended => {
                bail!(ChromeForTestingError::CacheInUse {
                    cache_dir: self.0.clone(),
                });
            }
        }
    }

    fn lock_path(&self, key: CacheLockKey) -> PathBuf {
        self.0.join(LOCKS_DIR).join(key.file_name())
    }

    async fn remove_entry(path: PathBuf) -> Result<(), Report<ChromeForTestingError>> {
        let metadata = fs::symlink_metadata(&path)
            .await
            .context(ChromeForTestingError::RemoveCacheEntry { path: path.clone() })?;
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        result.context(ChromeForTestingError::RemoveCacheEntry { path })
    }
}

#[derive(Debug, Clone, Copy)]
enum CacheLockKey {
    Cache,
    Artifact {
        version: Version,
        platform: Platform,
        artifact: ChromeForTestingArtifact,
    },
}

impl CacheLockKey {
    fn file_name(self) -> String {
        match self {
            Self::Cache => CACHE_LOCK.to_owned(),
            Self::Artifact {
                version,
                platform,
                artifact,
            } => format!("{version}-{platform}-{artifact}.lock"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CacheDir;
    use crate::test_support::TestDirectory;
    use crate::{CancellationToken, ChromeForTestingError, Version};
    use assertr::prelude::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_removes_only_unretained_version_directories() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("cache-prune")?;
        let cache = CacheDir::create_at(directory.path().to_owned())?;
        let retained: Version = "135.0.7019.0".parse()?;
        let removed: Version = "134.0.6998.0".parse()?;
        tokio::fs::create_dir_all(cache.path().join(retained.to_string())).await?;
        tokio::fs::create_dir_all(cache.path().join(removed.to_string())).await?;
        tokio::fs::write(cache.path().join("owner-note"), "preserve").await?;

        let result = cache.prune(&[retained]).await?;

        assert_that!(result.removed_versions()).contains_exactly([removed]);
        assert_that!(tokio::fs::try_exists(cache.path().join(retained.to_string())).await?)
            .is_true();
        assert_that!(tokio::fs::try_exists(cache.path().join(removed.to_string())).await?)
            .is_false();
        assert_that!(tokio::fs::try_exists(cache.path().join("owner-note")).await?).is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_lease_blocks_mutation_until_its_typed_guard_drops()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("cache-lease")?;
        let cache = CacheDir::create_at(directory.path().to_owned())?;
        let lease = cache.acquire_shared(CancellationToken::new()).await?;

        let error = cache
            .prune(&[])
            .await
            .expect_err("shared lease must exclude cache mutation");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::CacheInUse { .. }
        ))
        .is_true();

        drop(lease);
        assert_that!(cache.prune(&[]).await?.is_empty()).is_true();
        Ok(())
    }
}
