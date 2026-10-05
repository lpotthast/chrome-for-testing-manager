//! File locks coordinating cache readers, installers, and mutations.
//!
//! Raw files never escape this module. Each acquisition yields a purpose-specific RAII guard, so
//! shared cache leases, artifact transactions, and cache-wide mutation ownership cannot be
//! confused. A lock is released when its guard (and, for a lease, every clone) is dropped.

use crate::{CancellationToken, ChromeForTestingError};
use rootcause::{Report, prelude::ResultExt};
use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);

type Result<T> = std::result::Result<T, Report<ChromeForTestingError>>;

/// A shared, cache-wide lease retained while installed paths are in use.
#[derive(Clone)]
pub(crate) struct CacheLease {
    _file: Arc<File>,
}

impl std::fmt::Debug for CacheLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheLease").finish_non_exhaustive()
    }
}

/// An exclusive lock for one `(version, platform, artifact)` installation transaction.
pub(crate) struct ArtifactLock {
    _file: File,
}

/// Exclusive ownership of the whole cache, excluding every lease.
pub(super) struct CacheMutationGuard {
    _file: File,
}

/// Wait for a shared lock on `path`.
pub(super) async fn lease(path: &Path, cancellation: &CancellationToken) -> Result<CacheLease> {
    let file = wait(path, cancellation, File::try_lock_shared).await?;
    Ok(CacheLease {
        _file: Arc::new(file),
    })
}

/// Wait for an exclusive lock on `path`.
pub(super) async fn artifact_lock(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<ArtifactLock> {
    let file = wait(path, cancellation, File::try_lock).await?;
    Ok(ArtifactLock { _file: file })
}

/// Try to take an exclusive lock on `path`, making `attempts` attempts `delay` apart. Returns
/// `None` while the lock stays contended.
pub(super) async fn try_mutation_guard(
    path: &Path,
    attempts: u32,
    delay: Duration,
) -> Result<Option<CacheMutationGuard>> {
    let file = open(path).await?;
    for attempt in 1..=attempts {
        match file.try_lock() {
            Ok(()) => return Ok(Some(CacheMutationGuard { _file: file })),
            Err(TryLockError::WouldBlock) if attempt < attempts => tokio::time::sleep(delay).await,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(acquire_error(error, path)),
        }
    }
    Ok(None)
}

/// Poll the non-blocking lock acquisition until it succeeds or the token is cancelled.
async fn wait(
    path: &Path,
    cancellation: &CancellationToken,
    try_lock: fn(&File) -> std::result::Result<(), TryLockError>,
) -> Result<File> {
    let file = open(path).await?;
    loop {
        crate::check_cancelled(cancellation)?;
        match try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {}
                    () = tokio::time::sleep(LOCK_POLL_INTERVAL) => {}
                }
            }
            Err(TryLockError::Error(error)) => return Err(acquire_error(error, path)),
        }
    }
}

async fn open(path: &Path) -> Result<File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(path)
        .await
        .context(ChromeForTestingError::OpenLockFile {
            path: path.to_owned(),
        })?;
    Ok(file.into_std().await)
}

fn acquire_error(error: std::io::Error, path: &Path) -> Report<ChromeForTestingError> {
    Report::new_sendsync(error).context(ChromeForTestingError::AcquireCacheLock {
        path: path.to_owned(),
    })
}
