//! Cache directory ownership, leases, pruning, and mutation coordination.
//!
//! Cache operations retain typed leases while installed artifacts are in use. The underlying
//! file-lock state machine lives in the private [`lock`] module.
//!
//! Directory trees are removed by renaming them to a trash entry first (see [`remove_tree`]), so
//! an interrupted removal never leaves a partial version or package behind under its real name.

mod lock;

use self::lock::CacheMutationGuard;
pub(crate) use self::lock::{ArtifactLock, CacheLease};
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError};
use ::chrome_for_testing::{Platform, Version};
use rootcause::{Report, option_ext::OptionExt, prelude::ResultExt};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;

const LOCKS_DIR: &str = ".locks";
const CACHE_LOCK: &str = "cache.lock";
/// Prefix of version directories being removed from the cache root.
const VERSION_TRASH_PREFIX: &str = ".trash.";
/// Distinguishes the unique entry names this process creates.
static ENTRY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// How often a cache mutation tries to acquire the exclusive cache lock.
const MUTATION_LOCK_ATTEMPTS: u32 = 10;
/// Delay between attempts to acquire the exclusive cache lock.
const MUTATION_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

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
        cancellation: &CancellationToken,
    ) -> Result<CacheLease, Report<ChromeForTestingError>> {
        lock::lease(&self.cache_lock_path(), cancellation).await
    }

    pub async fn acquire_artifact(
        &self,
        version: Version,
        platform: Platform,
        artifact: ChromeForTestingArtifact,
        cancellation: &CancellationToken,
    ) -> Result<ArtifactLock, Report<ChromeForTestingError>> {
        let lock_path = self
            .0
            .join(LOCKS_DIR)
            .join(format!("{version}-{platform}-{artifact}.lock"));
        lock::artifact_lock(&lock_path, cancellation).await
    }

    /// Remove every cached version directory.
    ///
    /// Entries that are not version directories (e.g. unrelated files in a custom cache directory)
    /// are kept, as is the lock namespace used to coordinate callers.
    pub async fn clear(&self) -> Result<(), Report<ChromeForTestingError>> {
        self.remove_versions(|_| true).await.map(|_| ())
    }

    /// Remove cached version directories except for explicitly retained versions.
    pub async fn prune(
        &self,
        retained_versions: &[Version],
    ) -> Result<CachePruneResult, Report<ChromeForTestingError>> {
        self.remove_versions(|version| !retained_versions.contains(version))
            .await
    }

    async fn remove_versions(
        &self,
        should_remove: impl Fn(&Version) -> bool,
    ) -> Result<CachePruneResult, Report<ChromeForTestingError>> {
        let _mutation_guard = self.try_mutation_guard().await?;
        // Leftovers of interrupted removals.
        remove_entries_with_prefix(&self.0, VERSION_TRASH_PREFIX)
            .await
            .context(ChromeForTestingError::RemoveCacheEntry {
                path: self.0.clone(),
            })?;

        let read_error = || ChromeForTestingError::ReadCacheDir {
            cache_dir: self.0.clone(),
        };
        let mut removed_versions = Vec::new();
        let mut entries = fs::read_dir(self.path()).await.context_with(read_error)?;
        while let Some(entry) = entries.next_entry().await.context_with(read_error)? {
            let Some(version) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<Version>().ok())
            else {
                continue;
            };
            if !should_remove(&version) {
                continue;
            }
            let path = entry.path();
            remove_tree(&path, VERSION_TRASH_PREFIX)
                .await
                .context(ChromeForTestingError::RemoveCacheEntry { path })?;
            self.remove_version_locks(version).await;
            removed_versions.push(version);
        }
        removed_versions.sort_unstable();
        Ok(CachePruneResult { removed_versions })
    }

    /// Remove the artifact lock files of a removed version.
    ///
    /// Artifact locks are only taken while holding a shared cache lease, which the caller's
    /// exclusive mutation guard rules out, so no lock file is in use. Best effort: a remaining
    /// lock file is harmless.
    async fn remove_version_locks(&self, version: Version) {
        let locks_dir = self.0.join(LOCKS_DIR);
        if let Err(error) = remove_entries_with_prefix(&locks_dir, &format!("{version}-")).await {
            tracing::warn!(
                %version,
                dir = %locks_dir.display(),
                %error,
                "failed to remove lock files of a removed version"
            );
        }
    }

    /// Acquire the exclusive cache lock, or fail with [`ChromeForTestingError::CacheInUse`] if a
    /// lease holds the cache.
    ///
    /// Contention is retried briefly: a lock released by dropping a lease stays held for a moment
    /// while a concurrently spawned child process still holds a copy of its file descriptor,
    /// until the child's `exec` closes it.
    async fn try_mutation_guard(
        &self,
    ) -> Result<CacheMutationGuard, Report<ChromeForTestingError>> {
        lock::try_mutation_guard(
            &self.cache_lock_path(),
            MUTATION_LOCK_ATTEMPTS,
            MUTATION_LOCK_RETRY_DELAY,
        )
        .await?
        .context(ChromeForTestingError::CacheInUse {
            cache_dir: self.0.clone(),
        })
    }

    fn cache_lock_path(&self) -> PathBuf {
        self.0.join(LOCKS_DIR).join(CACHE_LOCK)
    }
}

/// Remove the directory tree or file at `path`, if present.
///
/// A directory is first renamed to a unique `<trash_prefix>...` sibling and only then deleted, so
/// an interrupted removal leaves a trash entry behind, never a partial tree at `path`. Callers
/// remove leftover trash with [`remove_entries_with_prefix`].
pub(crate) async fn remove_tree(path: &Path, trash_prefix: &str) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) => return ignore_not_found(error),
    };
    if !metadata.is_dir() {
        return fs::remove_file(path).await;
    }
    let trash = loop {
        let trash = path.with_file_name(unique_entry_name(trash_prefix));
        match fs::rename(path, &trash).await {
            Ok(()) => break trash,
            // A leftover of a crashed process with a reused pid occupies the name.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error),
        }
    };
    fs::remove_dir_all(&trash).await
}

/// Create a new directory in `parent` whose name starts with `prefix` and is unique.
pub(crate) async fn create_unique_dir(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    loop {
        let path = parent.join(unique_entry_name(prefix));
        match fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            // A leftover of a crashed process with a reused pid occupies the name.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// `<prefix><pid>.<sequence>`: unique among the entries this process creates.
fn unique_entry_name(prefix: &str) -> String {
    let sequence = ENTRY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{}.{sequence}", std::process::id())
}

/// Remove every entry of `dir` whose name starts with `prefix`. A missing `dir` has none.
pub(crate) async fn remove_entries_with_prefix(dir: &Path, prefix: &str) -> io::Result<()> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) => return ignore_not_found(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        let path = entry.path();
        let result = if entry.file_type().await?.is_dir() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        result.or_else(ignore_not_found)?;
    }
    Ok(())
}

/// Treat a missing entry as already removed.
pub(crate) fn ignore_not_found(error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
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
    async fn clear_removes_versions_trash_and_their_locks_only() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("cache-clear")?;
        let cache = CacheDir::create_at(directory.path().to_owned())?;
        let version: Version = "135.0.7019.0".parse()?;
        let version_dir = cache.path().join(version.to_string());
        tokio::fs::create_dir_all(version_dir.join("linux64/chrome-linux64")).await?;
        tokio::fs::create_dir_all(cache.path().join(".trash.1.0/leftover")).await?;
        tokio::fs::write(cache.path().join("owner-note"), "preserve").await?;
        let lock = cache
            .path()
            .join(".locks")
            .join(format!("{version}-linux64-chrome.lock"));
        tokio::fs::write(&lock, "").await?;

        cache.clear().await?;

        assert_that!(tokio::fs::try_exists(&version_dir).await?).is_false();
        assert_that!(tokio::fs::try_exists(cache.path().join(".trash.1.0")).await?).is_false();
        assert_that!(tokio::fs::try_exists(&lock).await?).is_false();
        assert_that!(tokio::fs::try_exists(cache.path().join("owner-note")).await?).is_true();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_lease_blocks_mutation_until_its_typed_guard_drops()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("cache-lease")?;
        let cache = CacheDir::create_at(directory.path().to_owned())?;
        let lease = cache.acquire_shared(&CancellationToken::new()).await?;

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
