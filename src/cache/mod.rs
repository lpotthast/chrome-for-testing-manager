//! Cache directory ownership, leases, pruning, and mutation coordination.
//!
//! Cache operations retain typed leases while installed artifacts are in use. The underlying
//! file-lock state machine lives in the private [`lock`] module.
//!
//! Directory trees are removed by renaming them to a trash entry first (see [`remove_tree`]), so
//! an interrupted removal never leaves a partial version or package behind under its real name.
//!
//! Everything this crate stores lives in a layout directory ([`LAYOUT_DIR`]) beneath the cache
//! root. Versions of this crate with an incompatible on-disk layout therefore never share
//! directories, so none of them can mistake a package another one installed, and is possibly
//! running, for an incomplete one and replace it.

mod lock;

use self::lock::CacheMutationGuard;
pub(crate) use self::lock::{ArtifactLock, CacheLease};
use crate::{CancellationToken, ChromeForTestingArtifact, ChromeForTestingError};
use ::chrome_for_testing::{Platform, Version};
use rootcause::{Report, option_ext::OptionExt, prelude::ResultExt};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::fs;

/// Directory beneath the cache root holding versions and locks of the current on-disk layout.
///
/// Bump this whenever the layout or the completion marker changes incompatibly. Pre-0.13 releases
/// stored versions directly in the cache root; those directories are left untouched.
const LAYOUT_DIR: &str = "v1";
const LOCKS_DIR: &str = ".locks";
const CACHE_LOCK: &str = "cache.lock";
/// Prefix of version directories being removed from the cache root.
const VERSION_TRASH_PREFIX: &str = ".trash.";
/// Distinguishes the unique entry names this process creates.
static ENTRY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// How often a cache mutation tries to acquire the exclusive cache lock.
const MUTATION_LOCK_ATTEMPTS: u32 = 10;
/// Delay between attempts to acquire the exclusive cache lock.
const MUTATION_LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
/// How often a file-system operation is attempted while Windows reports the entry as locked.
const LOCKED_ENTRY_ATTEMPTS: u32 = if cfg!(windows) { 8 } else { 1 };
/// Initial delay between attempts on a locked entry; doubled after every attempt.
const LOCKED_ENTRY_RETRY_DELAY: Duration = Duration::from_millis(25);

/// The cache root chosen by the user, and the layout directory beneath it that holds the cache
/// contents.
#[derive(Debug, Clone)]
pub(crate) struct CacheDir {
    root: PathBuf,
    layout: PathBuf,
}

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
        let layout = cache_dir.join(LAYOUT_DIR);
        std::fs::create_dir_all(layout.join(LOCKS_DIR)).context(
            ChromeForTestingError::CreateCacheDir {
                cache_dir: cache_dir.clone(),
            },
        )?;
        Ok(Self {
            root: cache_dir,
            layout,
        })
    }

    /// The cache root chosen by the user.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The layout directory holding version directories and locks.
    pub fn path(&self) -> &Path {
        &self.layout
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
            .layout
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
        // Leftovers of interrupted removals. Best effort: an entry that cannot be removed yet (e.g.
        // on Windows, a browser still running from it) must not block every later clear or prune.
        if let Err(error) = remove_entries_with_prefix(&self.layout, VERSION_TRASH_PREFIX).await {
            tracing::warn!(
                dir = %self.layout.display(),
                %error,
                "failed to remove leftovers of an interrupted cache removal"
            );
        }

        let read_error = || ChromeForTestingError::ReadCacheDir {
            cache_dir: self.layout.clone(),
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
        let locks_dir = self.layout.join(LOCKS_DIR);
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
            cache_dir: self.root.clone(),
        })
    }

    fn cache_lock_path(&self) -> PathBuf {
        self.layout.join(LOCKS_DIR).join(CACHE_LOCK)
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
        match retry_while_locked(|| fs::rename(path, &trash)).await {
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
    retry_while_locked(|| fs::remove_dir_all(&trash)).await
}

/// Run a file-system operation, retrying with backoff while Windows reports the entry as locked.
///
/// Virus scanners and indexers briefly open freshly written files, which makes renaming or
/// removing their directory fail with `PermissionDenied` until they let go. Elsewhere, the
/// operation runs once.
pub(crate) async fn retry_while_locked<T, Fut>(mut operation: impl FnMut() -> Fut) -> io::Result<T>
where
    Fut: Future<Output = io::Result<T>>,
{
    let mut delay = LOCKED_ENTRY_RETRY_DELAY;
    let mut attempt = 1;
    loop {
        match operation().await {
            Err(error)
                if attempt < LOCKED_ENTRY_ATTEMPTS
                    && error.kind() == io::ErrorKind::PermissionDenied =>
            {
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
            result => return result,
        }
    }
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
///
/// An entry that cannot be removed does not stop the removal of the others; the first such error
/// is returned once every entry was attempted.
pub(crate) async fn remove_entries_with_prefix(dir: &Path, prefix: &str) -> io::Result<()> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) => return ignore_not_found(error),
    };
    let mut first_error = None;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        let path = entry.path();
        let result = match entry.file_type().await {
            Ok(file_type) if file_type.is_dir() => {
                retry_while_locked(|| fs::remove_dir_all(&path)).await
            }
            Ok(_) => retry_while_locked(|| fs::remove_file(&path)).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result.or_else(ignore_not_found) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
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
    async fn clear_leaves_version_directories_of_other_layouts_untouched()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("cache-clear-legacy-layout")?;
        let cache = CacheDir::create_at(directory.path().to_owned())?;
        let version: Version = "135.0.7019.0".parse()?;
        // Pre-0.13 releases stored versions directly in the cache root.
        let legacy_package = cache.root().join(version.to_string()).join("linux64");
        tokio::fs::create_dir_all(&legacy_package).await?;
        tokio::fs::create_dir_all(cache.path().join(version.to_string())).await?;

        cache.clear().await?;

        assert_that!(cache.path().starts_with(cache.root())).is_true();
        assert_that!(cache.path()).is_not_equal_to(cache.root());
        assert_that!(tokio::fs::try_exists(cache.path().join(version.to_string())).await?)
            .is_false();
        assert_that!(tokio::fs::try_exists(&legacy_package).await?).is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn prefix_removal_continues_past_entries_it_cannot_remove()
    -> Result<(), rootcause::Report> {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("cache-prefix-removal")?;
        let stuck = directory.path().join(".trash.stuck");
        tokio::fs::create_dir_all(stuck.join("child")).await?;
        tokio::fs::create_dir_all(directory.path().join(".trash.other")).await?;
        tokio::fs::write(directory.path().join("kept"), "").await?;
        // Without write access, the child of the stuck entry cannot be removed.
        tokio::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).await?;

        let result = super::remove_entries_with_prefix(directory.path(), ".trash.").await;
        let stuck_remains = tokio::fs::try_exists(&stuck).await?;
        if stuck_remains {
            tokio::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).await?;
        }

        // A privileged user can remove the stuck entry; the others must be removed either way.
        assert_that!(result.is_err()).is_equal_to(stuck_remains);
        assert_that!(tokio::fs::try_exists(directory.path().join(".trash.other")).await?)
            .is_false();
        assert_that!(tokio::fs::try_exists(directory.path().join("kept")).await?).is_true();
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
