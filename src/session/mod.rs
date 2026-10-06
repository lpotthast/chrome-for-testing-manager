//! Scoped `thirtyfour` sessions and auxiliary browser lifecycle support.
//!
//! Session operations own connection and cleanup ordering, including the separate Chrome Headless
//! Shell process required for attached sessions.

mod builder;
pub(crate) mod headless_shell;

pub use builder::SessionBuilder;

use crate::ChromeForTestingError;
use rootcause::prelude::ResultExt;
use rootcause::{Report, report};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::task::TaskTracker;

/// Cleanups that dropped session runs handed to the runtime, and the failures they reported.
///
/// Clones share their tasks and failures.
#[derive(Debug, Clone, Default)]
pub(crate) struct BackgroundCleanups {
    tracker: TaskTracker,
    failures: Arc<Mutex<Vec<Report<ChromeForTestingError>>>>,
}

impl BackgroundCleanups {
    /// Run `cleanup` on `runtime`, recording its failure.
    pub(crate) fn spawn_on(
        &self,
        cleanup: impl Future<Output = Result<(), Report<ChromeForTestingError>>> + Send + 'static,
        runtime: &tokio::runtime::Handle,
    ) {
        let failures = Arc::clone(&self.failures);
        self.tracker.spawn_on(
            async move {
                if let Err(error) = cleanup.await {
                    // Logged as well: the environment may never be shut down explicitly.
                    tracing::error!(%error, "failed to clean up dropped browser session");
                    failures
                        .lock()
                        .expect("cleanup failure mutex is not poisoned")
                        .push(error);
                }
            },
            runtime,
        );
    }

    /// Stop accepting cleanups, wait for the running ones, and report their failures.
    pub(crate) async fn finish(&self) -> Result<(), Report<ChromeForTestingError>> {
        self.tracker.close();
        self.tracker.wait().await;
        let failures = std::mem::take(
            &mut *self
                .failures
                .lock()
                .expect("cleanup failure mutex is not poisoned"),
        );
        if failures.is_empty() {
            return Ok(());
        }
        let mut error = report!(ChromeForTestingError::DroppedSessionCleanup {
            failures: failures.len(),
        });
        for failure in failures {
            crate::error::attach_child(&mut error, failure);
        }
        Err(error)
    }
}

/// A browser session, handed to the closure of [`SessionBuilder::run`].
///
/// Dereferences to [`thirtyfour::WebDriver`], so the session can be used as the driver. Use
/// [`Self::driver`] (or clone it) where an owned or explicitly typed driver is needed.
#[derive(Debug)]
pub struct Session {
    pub(crate) driver: thirtyfour::WebDriver,
}

impl Session {
    /// Return the `WebDriver` controlling this session's browser.
    #[must_use]
    pub const fn driver(&self) -> &thirtyfour::WebDriver {
        &self.driver
    }

    /// Quit the browser session, waiting at most `timeout`.
    ///
    /// If quitting fails or times out, the driver handle is leaked instead of dropped:
    /// `thirtyfour` would otherwise retry the quit synchronously while dropping the handle,
    /// blocking a runtime worker. The error is reported either way.
    pub(crate) async fn quit_within(
        self,
        timeout: Duration,
    ) -> Result<(), Report<ChromeForTestingError>> {
        let handle = self.driver.clone();
        let result = match tokio::time::timeout(timeout, self.driver.quit()).await {
            Ok(result) => result.context(ChromeForTestingError::QuitSession),
            Err(_) => Err(report!(ChromeForTestingError::QuitSessionTimeout {
                timeout
            })),
        };
        if result.is_err() {
            // The handle cannot have quit successfully, so leaking cannot fail.
            let _ = handle.leak();
        }
        result
    }
}

impl Deref for Session {
    type Target = thirtyfour::WebDriver;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}

impl AsRef<thirtyfour::WebDriver> for Session {
    fn as_ref(&self) -> &thirtyfour::WebDriver {
        &self.driver
    }
}
