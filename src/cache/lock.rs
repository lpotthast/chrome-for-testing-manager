//! Typed file-lock states used to coordinate cache readers, installers, and mutations.
//!
//! Raw files never escape this module. Successful acquisition yields a purpose-specific RAII
//! guard, preventing callers from confusing shared cache leases, artifact transactions, and
//! cache-wide mutation ownership.

use crate::{CancellationToken, ChromeForTestingError};
use rootcause::{Report, prelude::ResultExt};
use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone)]
struct SharedFileLock {
    _file: Arc<File>,
}

struct ExclusiveFileLock {
    _file: File,
}

/// A shared, cache-wide lease retained while installed paths are in use.
#[derive(Clone)]
pub(crate) struct CacheLease {
    _lock: SharedFileLock,
}

impl std::fmt::Debug for CacheLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheLease").finish_non_exhaustive()
    }
}

/// An exclusive lock for one `(version, platform, artifact)` installation transaction.
pub(crate) struct ArtifactLock {
    _lock: ExclusiveFileLock,
}

impl std::fmt::Debug for ArtifactLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactLock").finish_non_exhaustive()
    }
}

pub(super) struct CacheMutationGuard(#[allow(dead_code)] ExclusiveFileLock);

pub(super) struct UnlockedFileLock {
    file: File,
    path: PathBuf,
}

pub(super) enum TryAcquire<T> {
    Acquired(T),
    Contended,
}

impl UnlockedFileLock {
    pub(super) async fn open(path: PathBuf) -> Result<Self, Report<ChromeForTestingError>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .await
            .context(ChromeForTestingError::OpenLockFile { path: path.clone() })?;
        Ok(Self {
            file: file.into_std().await,
            path,
        })
    }

    pub(super) async fn wait_shared(
        self,
        cancellation: CancellationToken,
    ) -> Result<CacheLease, Report<ChromeForTestingError>> {
        self.wait(cancellation, File::try_lock_shared)
            .await
            .map(|file| CacheLease {
                _lock: SharedFileLock {
                    _file: Arc::new(file),
                },
            })
    }

    pub(super) async fn wait_exclusive(
        self,
        cancellation: CancellationToken,
    ) -> Result<ArtifactLock, Report<ChromeForTestingError>> {
        self.wait(cancellation, File::try_lock)
            .await
            .map(|file| ArtifactLock {
                _lock: ExclusiveFileLock { _file: file },
            })
    }

    pub(super) fn try_exclusive(
        self,
    ) -> Result<TryAcquire<CacheMutationGuard>, Report<ChromeForTestingError>> {
        match self.file.try_lock() {
            Ok(()) => Ok(TryAcquire::Acquired(CacheMutationGuard(
                ExclusiveFileLock { _file: self.file },
            ))),
            Err(TryLockError::WouldBlock) => Ok(TryAcquire::Contended),
            Err(TryLockError::Error(error)) => Err(Report::new_sendsync(error)
                .context(ChromeForTestingError::AcquireCacheLock { path: self.path })),
        }
    }

    /// Poll the non-blocking lock acquisition until it succeeds or the token is cancelled.
    async fn wait(
        self,
        cancellation: CancellationToken,
        try_lock: fn(&File) -> std::result::Result<(), TryLockError>,
    ) -> Result<File, Report<ChromeForTestingError>> {
        let Self { file, path } = self;
        loop {
            crate::check_cancelled(&cancellation)?;

            match try_lock(&file) {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {}
                        () = tokio::time::sleep(LOCK_POLL_INTERVAL) => {}
                    }
                }
                Err(TryLockError::Error(error)) => {
                    return Err(Report::new_sendsync(error)
                        .context(ChromeForTestingError::AcquireCacheLock { path }));
                }
            }
        }
    }
}
