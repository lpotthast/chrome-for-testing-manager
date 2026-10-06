//! Cleanups that dropped operations hand to the Tokio runtime.
//!
//! A dropped future or handle cannot await its cleanup: a dropped session run cannot quit its
//! session, a dropped process handle cannot gracefully terminate its process, and a dropped
//! installation cannot observe its rollback. They hand that work to [`BackgroundTasks`], so that
//! shutting down can wait for it and report what failed.

use crate::{ChromeForTestingError, Result};
use rootcause::{Report, report};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

/// Background cleanups of dropped operations, and the failures they reported.
///
/// Clones share their tasks and failures.
#[derive(Debug, Clone)]
pub(crate) struct BackgroundTasks {
    /// Closed from the start: a closed tracker still accepts tasks, and waiting on it completes
    /// whenever it is empty instead of requiring a close first.
    tracker: TaskTracker,
    failures: Arc<Mutex<Vec<Report<ChromeForTestingError>>>>,
}

impl Default for BackgroundTasks {
    fn default() -> Self {
        let tracker = TaskTracker::new();
        tracker.close();
        Self {
            tracker,
            failures: Arc::default(),
        }
    }
}

impl BackgroundTasks {
    /// Run `cleanup` on `runtime`, recording its failure.
    pub(crate) fn spawn_cleanup(
        &self,
        cleanup: impl Future<Output = Result<()>> + Send + 'static,
        runtime: &tokio::runtime::Handle,
    ) {
        let tasks = self.clone();
        self.tracker.spawn_on(
            async move {
                if let Err(error) = cleanup.await {
                    tasks.record_failure(error);
                }
            },
            runtime,
        );
    }

    /// Run `task` on the current runtime. The task reports its own failures through
    /// [`Self::record_failure`].
    pub(crate) fn spawn<T: Send + 'static>(
        &self,
        task: impl Future<Output = T> + Send + 'static,
    ) -> JoinHandle<T> {
        self.tracker.spawn(task)
    }

    /// Record the failure of a background cleanup, to be reported by [`Self::wait`].
    pub(crate) fn record_failure(&self, error: Report<ChromeForTestingError>) {
        // Logged as well: the failure is lost if nobody waits for the background tasks.
        tracing::error!(%error, "background cleanup of a dropped operation failed");
        self.failures
            .lock()
            .expect("background failure mutex is not poisoned")
            .push(error);
    }

    /// Wait until no background task is running, including tasks spawned while waiting, and
    /// report the failures recorded since the last wait.
    pub(crate) async fn wait(&self) -> Result<()> {
        self.tracker.wait().await;
        let failures = std::mem::take(
            &mut *self
                .failures
                .lock()
                .expect("background failure mutex is not poisoned"),
        );
        if failures.is_empty() {
            return Ok(());
        }
        let mut error = report!(ChromeForTestingError::BackgroundCleanup {
            failures: failures.len(),
        });
        for failure in failures {
            crate::error::attach_child(&mut error, failure);
        }
        Err(error)
    }
}
