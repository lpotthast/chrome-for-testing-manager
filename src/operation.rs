//! In-process drop safety for asynchronous operations that cannot safely be abandoned.
//!
//! # The problem
//!
//! Rust cancels a future by dropping it. Drop is synchronous, so an ordinary async function cannot
//! await rollback after its caller stops polling it. That matters here at boundaries where the
//! operation may already have created state that only later awaits can identify or release:
//!
//! - extraction can have an unabortable blocking worker writing into a staging directory;
//! - cache mutation must retain its exclusive guard until filesystem work has stopped;
//! - process startup may already have spawned a child that must be terminated; and
//! - `POST /session` may have created a server-side `WebDriver` session before the response reveals
//!   the session id needed to close it.
//!
//! # What `AbortSafeOperation` does
//!
//! [`AbortSafeOperation::run`] transfers the operation future to a Tokio task. The Tokio runtime,
//! rather than the public future, then owns and polls the operation. The public future only awaits
//! the task's [`JoinHandle`]. If that public future is dropped (including because its caller task is
//! aborted), [`OperationTask::drop`] cancels a child [`CancellationToken`]. Dropping the join handle
//! *detaches* the owner task; it does not abort it. While the runtime remains alive, the owner can
//! observe the token, stop acquiring new work, await in-flight work, and perform its own cleanup.
//!
//! "Abort-safe" therefore means safe against aborting or dropping the *calling future while the
//! same Tokio runtime continues running*. It does not mean that the owner task itself is impossible
//! to abort.
//!
//! # Limits
//!
//! - Cancellation is cooperative. This type only signals a token; the operation must check or
//!   await it and must implement every required cleanup step itself.
//! - The detached task is not durable. Tokio runtime shutdown can abort it, and process exit,
//!   crashes, `abort(3)`, or `SIGKILL` cannot run or await async cleanup.
//! - Once the public future is dropped, nobody can join the owner task. Its return value and any
//!   cleanup error in that value are discarded. Await the public future when the outcome matters.
//! - If the public future is dropped before its first poll, `run` never executes, no owner task is
//!   spawned, and the `start` closure is dropped normally. Resource acquisition must therefore
//!   happen inside the returned operation future; anything captured by `start` beforehand must
//!   already have a synchronous, drop-safe fallback.
//! - An operation that ignores cancellation or never completes can remain detached for the rest of
//!   the runtime's lifetime.
//!
//! This mechanism is deliberately different from `await_or_cancelled`, which drops its operation
//! future when cancellation wins and is therefore suitable only when abandonment is already safe.

use crate::{CancellationToken, ChromeForTestingError, Result};
use rootcause::prelude::ResultExt;
use tokio::task::JoinHandle;

/// Namespace for transferring a cleanup-sensitive future to a runtime-owned task.
///
/// See the [module-level documentation](self) for its exact, runtime-scoped guarantee.
pub(crate) struct AbortSafeOperation;

impl AbortSafeOperation {
    /// Run a cleanup-sensitive operation in a task that can outlive the returned public future.
    ///
    /// `parent_cancellation` and dropping the returned future both cancel the child token passed to
    /// `start`. Neither event forcibly aborts the owner task. The owner must cooperate with the
    /// token and must not return until any required rollback has been drained.
    ///
    /// This guarantee requires the same Tokio runtime to remain alive. Dropping the returned future
    /// also gives up the ability to observe the operation's final value or error.
    pub(crate) async fn run<T, F, Fut>(
        operation: &'static str,
        parent_cancellation: CancellationToken,
        start: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        Self::spawn(&parent_cancellation, start)
            .join(operation)
            .await
    }

    /// Run a cleanup-sensitive operation in an owner task, but bound how long its caller waits.
    ///
    /// When the timeout elapses, the join handle is detached. The operation future remains owned
    /// and polled by the Tokio runtime, so cleanup continues to completion. The outer error
    /// reports only that the caller's wait elapsed; the detached operation's eventual result is
    /// unobservable.
    #[cfg(feature = "thirtyfour")]
    pub(crate) async fn run_bounded<T>(
        operation: &'static str,
        timeout: std::time::Duration,
        operation_future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> std::result::Result<Result<T>, tokio::time::error::Elapsed>
    where
        T: Send + 'static,
    {
        // Spawning is the ownership transfer: the runtime keeps polling the operation after the
        // timeout elapses and the join handle is dropped (detached, not aborted).
        let handle = tokio::spawn(operation_future);
        tokio::time::timeout(timeout, async move {
            handle
                .await
                .context(ChromeForTestingError::JoinOperationTask { operation })?
        })
        .await
    }

    fn spawn<T, F, Fut>(parent_cancellation: &CancellationToken, start: F) -> OperationTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        // Parent cancellation flows into the operation, while cancellation caused by dropping this
        // wrapper stays local and does not unexpectedly cancel the caller's parent token.
        let cancellation = parent_cancellation.child_token();

        // Spawning is the ownership transfer: after this point the runtime owns `Fut` and polls it
        // independently of the public future. Do not replace this with directly awaiting `start`;
        // doing so would drop `Fut` before it could perform asynchronous cleanup.
        let handle = tokio::spawn(start(cancellation.clone()));

        // `OperationTask` must remain in the public future's state until the join finishes. Its Drop
        // implementation is what translates abandonment of the public future into cooperative
        // cancellation of the owner task.
        OperationTask {
            cancellation,
            handle: Some(handle),
        }
    }
}

struct OperationTask<T> {
    cancellation: CancellationToken,
    handle: Option<JoinHandle<Result<T>>>,
}

impl<T> OperationTask<T> {
    async fn join(mut self, operation: &'static str) -> Result<T> {
        // The handle becomes a local in this async frame while `self` stays alive beside it. If this
        // join future is dropped, Tokio detaches the handle rather than aborting its task, and then
        // `self` requests cooperative cancellation through its Drop implementation. We
        // intentionally never call `JoinHandle::abort`, because that would drop the owner future at
        // an arbitrary await and recreate the resource leak this type exists to prevent.
        let handle = self
            .handle
            .take()
            .expect("operation task owns its join handle until join");
        handle
            .await
            .context(ChromeForTestingError::JoinOperationTask { operation })?
    }
}

impl<T> Drop for OperationTask<T> {
    fn drop(&mut self) {
        // This also runs after a normal join. Cancellation is then harmless because the owner task
        // has already completed. On early drop it wakes the still-running owner task.
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::AbortSafeOperation;
    use crate::CancellationToken;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_public_future_cancels_but_does_not_abort_owner_task() {
        let started = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let owner_started = Arc::clone(&started);
        let owner_finished = Arc::clone(&finished);
        let public_task = tokio::spawn(AbortSafeOperation::run(
            "drop-safety test",
            CancellationToken::new(),
            move |cancellation| async move {
                owner_started.notify_one();
                cancellation.cancelled().await;
                owner_finished.notify_one();
                Ok(())
            },
        ));

        started.notified().await;
        public_task.abort();
        let _ = public_task.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), finished.notified())
            .await
            .expect("detached owner task drained after its public future was dropped");
    }

    #[cfg(feature = "thirtyfour")]
    #[tokio::test(flavor = "multi_thread")]
    async fn bounded_wait_times_out_without_aborting_owner_task() {
        let release = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let owner_release = Arc::clone(&release);
        let owner_finished = Arc::clone(&finished);

        let result = AbortSafeOperation::run_bounded(
            "bounded drop-safety test",
            std::time::Duration::from_millis(20),
            async move {
                owner_release.notified().await;
                owner_finished.notify_one();
                Ok(())
            },
        )
        .await;

        assert!(result.is_err(), "the caller wait should reach its deadline");
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), finished.notified())
            .await
            .expect("the detached owner task continued after the timeout");
    }
}
