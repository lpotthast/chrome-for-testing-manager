//! Builder for scoped, panic-safe `WebDriver` session execution.
//!
//! Capability and client configuration are applied before connection. User callbacks always flow
//! through ordered session and browser cleanup.

use super::Session;
use super::headless_shell::HeadlessShellSession;
use crate::browser::{ChromeBinary, LoadedBrowserPackage};
use crate::error::operation_result_with_cleanup;
use crate::manager::ChromeForTestingManager;
use crate::operation::AbortSafeOperation;
use crate::{CancellationToken, ChromeForTestingError, Port};
use rootcause::prelude::ResultExt;
use rootcause::{IntoReportCollection, Report, markers::SendSync, report};
use std::time::Duration;
use thirtyfour::prelude::WebDriverError;
use thirtyfour::{ChromeCapabilities, WebDriverBuilder};

type CapsSetup = Box<dyn FnOnce(&mut ChromeCapabilities) -> Result<(), WebDriverError> + Send>;
type ConfigSetup = Box<dyn FnOnce(WebDriverBuilder) -> WebDriverBuilder + Send>;

/// A scoped, chainable builder for opening a `thirtyfour` [`Session`] against a running
/// [`crate::ChromeForTesting`].
///
/// Obtained via [`crate::ChromeForTesting::session`]. Optional setup steps:
///
/// - [`Self::with_cancellation`] opts into cooperative cancellation of connection, callback, and
///   cleanup.
/// - [`Self::with_caps`] mutates the [`ChromeCapabilities`] before the session opens (e.g. unset
///   headless, add Chrome args).
/// - [`Self::with_config`] receives the [`WebDriverBuilder`] and may configure the element poller,
///   request timeout, user-agent, or keep-alive flag.
///
/// Call [`Self::run`] to open the session and execute the user closure inside scoped, panic-safe
/// cleanup that always calls `WebDriver::quit().await`.
pub struct SessionBuilder<'a> {
    manager: &'a ChromeForTestingManager,
    loaded: &'a LoadedBrowserPackage,
    driver_port: Port,
    cancellation: Option<CancellationToken>,
    caps_setup: Option<CapsSetup>,
    config_setup: Option<ConfigSetup>,
}

impl<'a> SessionBuilder<'a> {
    pub(crate) fn new(
        manager: &'a ChromeForTestingManager,
        loaded: &'a LoadedBrowserPackage,
        driver_port: Port,
    ) -> Self {
        Self {
            manager,
            loaded,
            driver_port,
            cancellation: None,
            caps_setup: None,
            config_setup: None,
        }
    }

    /// Provide a token for cooperative cancellation of this session run.
    ///
    /// See the [crate-level cancellation section](crate#cancellation-and-drop-safety).
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Provide a closure that mutates the [`ChromeCapabilities`] used to create the session.
    ///
    /// For Chrome Headless Shell, browser arguments are applied to the separately launched shell.
    /// Other `goog:chromeOptions` entries are rejected because `ChromeDriver` cannot apply them
    /// after attaching to that already-running process.
    #[must_use]
    pub fn with_caps<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ChromeCapabilities) -> Result<(), WebDriverError> + Send + 'static,
    {
        self.caps_setup = Some(Box::new(f));
        self
    }

    /// Provide a closure that configures the [`WebDriverBuilder`] before the session is opened.
    #[must_use]
    pub fn with_config<F>(mut self, f: F) -> Self
    where
        F: FnOnce(WebDriverBuilder) -> WebDriverBuilder + Send + 'static,
    {
        self.config_setup = Some(Box::new(f));
        self
    }

    /// Open a [`Session`], hand it to the user closure, and tear it down once the closure resolves
    /// or panics.
    ///
    /// Cleanup runs regardless of outcome. A panic in the user closure is caught, cleanup is
    /// attempted, and the original panic is always resumed.
    ///
    /// If cancellation is requested while `WebDriver` connection is in flight, connection is driven
    /// to completion because dropping it could leave an unreachable server-side session. A newly
    /// connected session is then cleaned up before cancellation is reported. Cancellation during
    /// the user closure drops its future before cleanup begins. See the
    /// [crate-level cancellation section](crate#cancellation-and-drop-safety) for the drop-safety
    /// guarantee behind connection and cleanup.
    ///
    /// The cleanup deadline bounds how long this method waits, not how long the cleanup owner may
    /// run. If the deadline elapses, the runtime-owned quit task remains alive to avoid invoking
    /// `WebDriver`'s blocking synchronous drop fallback.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::Cancelled`] if cancellation is requested and cleanup
    /// succeeds. Other errors cover capability setup, session creation, the user closure, or
    /// cleanup. An operation error remains primary when cleanup also fails. Connection and quit
    /// honor the request behavior configured through [`Self::with_config`].
    pub async fn run<T, E, F>(self, f: F) -> Result<T, Report<ChromeForTestingError>>
    where
        F: for<'b> AsyncFnOnce(&'b Session) -> Result<T, E>,
        E: IntoReportCollection<SendSync>,
    {
        use futures::FutureExt;

        let port = self.driver_port;
        let cancellation = self.cancellation.unwrap_or_default();
        crate::check_cancelled(&cancellation)?;

        let mut caps = self.manager.prepare_caps(self.loaded)?;
        if let Some(caps_setup) = self.caps_setup {
            caps_setup(&mut caps).context(ChromeForTestingError::ConfigureSessionCapabilities)?;
        }
        let headless_shell = match self.loaded.chrome_binary() {
            ChromeBinary::Chrome => None,
            ChromeBinary::ChromeHeadlessShell => Some(
                self.manager
                    .launch_headless_shell_session(self.loaded, &mut caps, &cancellation)
                    .await?,
            ),
        };
        let builder = thirtyfour::WebDriver::builder(format!("http://127.0.0.1:{port}"), caps);
        let builder = match self.config_setup {
            Some(config_setup) => config_setup(builder),
            None => builder,
        };
        let cleanup_timeout = self.manager.session_cleanup_timeout();
        let resources = SessionResources::connect(
            builder,
            headless_shell,
            port,
            cleanup_timeout,
            cancellation.clone(),
        )
        .await?;
        let session_guard = SessionCleanupGuard::new(resources);

        let callback_result = {
            let mut callback = std::pin::pin!(
                core::panic::AssertUnwindSafe(f(session_guard.session())).catch_unwind()
            );
            tokio::select! {
                // Give an already-requested cancellation deterministic precedence over callback
                // completion so cleanup begins instead of returning a test result.
                biased;
                () = cancellation.cancelled() => None,
                result = &mut callback => Some(result),
            }
        };

        let cleanup_result = session_guard.cleanup().await;
        match callback_result {
            None => operation_result_with_cleanup(
                Err(report!(ChromeForTestingError::Cancelled)),
                cleanup_result,
            ),
            Some(Err(payload)) => {
                if let Err(cleanup_err) = cleanup_result {
                    tracing::error!(
                        error = %cleanup_err,
                        "failed to clean up browser session after callback panic"
                    );
                }
                std::panic::resume_unwind(payload);
            }
            Some(Ok(callback_result)) => {
                let callback_result =
                    callback_result.context(ChromeForTestingError::RunSessionCallback);
                operation_result_with_cleanup(callback_result, cleanup_result)
            }
        }
    }
}

impl SessionResources {
    async fn connect(
        builder: WebDriverBuilder,
        headless_shell: Option<HeadlessShellSession>,
        port: crate::Port,
        cleanup_timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<Self, Report<ChromeForTestingError>> {
        AbortSafeOperation::run(
            "WebDriver connection",
            cancellation,
            move |operation_cancellation| async move {
                if operation_cancellation.is_cancelled() {
                    return operation_result_with_cleanup(
                        Err(report!(ChromeForTestingError::Cancelled)),
                        Self::terminate_headless_shell_bounded(headless_shell, cleanup_timeout)
                            .await,
                    );
                }

                // `POST /session` is not safe to abandon. ChromeDriver can create the remote
                // session before the HTTP response gives us the session id required by
                // `DELETE /session/{id}`. The owner therefore drives the handshake to completion
                // even after its public caller is dropped. The cancelled branch below then either
                // closes the newly identified session or reports the failed handshake alongside
                // Headless Shell cleanup. This continuation is useful only while the Tokio runtime
                // remains alive; it is not protection against application termination.
                let connection_result = builder
                    .connect()
                    .await
                    .context(ChromeForTestingError::StartWebDriverSession { port });
                if operation_cancellation.is_cancelled() {
                    return match connection_result {
                        Ok(driver) => operation_result_with_cleanup(
                            Err(report!(ChromeForTestingError::Cancelled)),
                            Self {
                                session: Session { driver },
                                headless_shell,
                                cleanup_timeout,
                            }
                            .cleanup()
                            .await,
                        ),
                        Err(connection_error) => {
                            let mut cancellation_error = report!(ChromeForTestingError::Cancelled);
                            cancellation_error
                                .children_mut()
                                .push(connection_error.into_dynamic().into_cloneable());
                            operation_result_with_cleanup(
                                Err(cancellation_error),
                                Self::terminate_headless_shell_bounded(
                                    headless_shell,
                                    cleanup_timeout,
                                )
                                .await,
                            )
                        }
                    };
                }

                match connection_result {
                    Ok(driver) => Ok(Self {
                        session: Session { driver },
                        headless_shell,
                        cleanup_timeout,
                    }),
                    Err(error) => operation_result_with_cleanup(
                        Err(error),
                        Self::terminate_headless_shell_bounded(headless_shell, cleanup_timeout)
                            .await,
                    ),
                }
            },
        )
        .await
    }

    async fn cleanup(self) -> Result<(), Report<ChromeForTestingError>> {
        let timeout = self.cleanup_timeout;
        bounded_cleanup(
            "browser-session cleanup",
            timeout,
            self.cleanup_to_completion(),
        )
        .await
    }

    async fn cleanup_to_completion(self) -> Result<(), Report<ChromeForTestingError>> {
        let quit_result = self.session.quit().await;
        let browser_result = Self::terminate_headless_shell(self.headless_shell).await;
        operation_result_with_cleanup(quit_result, browser_result)
    }

    async fn terminate_headless_shell_bounded(
        headless_shell: Option<HeadlessShellSession>,
        timeout: Duration,
    ) -> Result<(), Report<ChromeForTestingError>> {
        bounded_cleanup(
            "Headless Shell cleanup",
            timeout,
            Self::terminate_headless_shell(headless_shell),
        )
        .await
    }

    async fn terminate_headless_shell(
        headless_shell: Option<HeadlessShellSession>,
    ) -> Result<(), Report<ChromeForTestingError>> {
        if let Some(headless_shell) = headless_shell {
            headless_shell.terminate().await?;
        }
        Ok(())
    }
}

/// Bound how long the caller waits for a runtime-owned cleanup future, reporting an elapsed wait
/// as [`ChromeForTestingError::SessionCleanupTimeout`]. The detached cleanup runs to completion.
async fn bounded_cleanup<T: Send + 'static>(
    operation: &'static str,
    timeout: Duration,
    cleanup: impl Future<Output = Result<T, Report<ChromeForTestingError>>> + Send + 'static,
) -> Result<T, Report<ChromeForTestingError>> {
    AbortSafeOperation::run_bounded(operation, timeout, cleanup)
        .await
        .map_err(|_| report!(ChromeForTestingError::SessionCleanupTimeout { timeout }))?
}

struct SessionResources {
    session: Session,
    headless_shell: Option<HeadlessShellSession>,
    cleanup_timeout: Duration,
}

struct SessionCleanupGuard {
    resources: Option<SessionResources>,
}

impl SessionCleanupGuard {
    const fn new(resources: SessionResources) -> Self {
        Self {
            resources: Some(resources),
        }
    }

    fn session(&self) -> &Session {
        &self
            .resources
            .as_ref()
            .expect("session guard owns resources until cleanup")
            .session
    }

    async fn cleanup(mut self) -> Result<(), Report<ChromeForTestingError>> {
        self.resources
            .take()
            .expect("session guard owns resources until cleanup")
            .cleanup()
            .await
    }
}

impl Drop for SessionCleanupGuard {
    fn drop(&mut self) {
        let Some(resources) = self.resources.take() else {
            return;
        };

        // Drop cannot await `WebDriver::quit`, so transfer the acquired resources to the active
        // runtime. As with `AbortSafeOperation`, dropping this JoinHandle detaches cleanup rather
        // than aborting it. This is best-effort in-process cleanup only: without a live runtime it
        // cannot run, and after detaching there is no caller to receive a cleanup error.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            if let Err(error) = resources.cleanup().await {
                tracing::error!(%error, "failed to clean up dropped browser session");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheDir;
    use crate::facade::ChromeForTesting;
    use crate::test_support::{FixtureServer, ResponseSpec, TestDirectory};
    use crate::version::SelectedVersion;
    use crate::{
        ChromeBinary, ChromeDriverConfig, ChromeForTestingManager, ChromeForTestingManagerConfig,
        LoadedBrowserPackage, Port,
    };
    use assertr::prelude::*;
    use axum::body::Bytes;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_timeout_detaches_quit_without_dropping_it() -> Result<(), rootcause::Report> {
        let new_session =
            br#"{"value":{"sessionId":"fixture-session","capabilities":{}}}"#.to_vec();
        let delete_response = Bytes::from_static(br#"{"value":null}"#);
        let delete_delay = Duration::from_millis(200);
        let server = FixtureServer::start(HashMap::from([
            ("/session".to_owned(), ResponseSpec::body(new_session)),
            (
                "/session/fixture-session/timeouts".to_owned(),
                ResponseSpec::body(br#"{"value":null}"#.to_vec()),
            ),
            (
                "/session/fixture-session".to_owned(),
                ResponseSpec::Delay(delete_delay, delete_response.clone()),
            ),
            (
                "/session/fixture-session/".to_owned(),
                ResponseSpec::Delay(delete_delay, delete_response),
            ),
        ]))
        .await?;
        let driver =
            thirtyfour::WebDriver::builder(server.url(""), thirtyfour::ChromeCapabilities::new())
                .connect()
                .await?;
        let cleanup_timeout = Duration::from_millis(20);
        let started = Instant::now();

        let error = SessionResources {
            session: Session { driver },
            headless_shell: None,
            cleanup_timeout,
        }
        .cleanup()
        .await
        .expect_err("cleanup should stop waiting at its independent deadline");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::SessionCleanupTimeout { .. }
        ))
        .is_true();
        assert_that!(started.elapsed() < Duration::from_millis(150))
            .with_detail_message("cleanup timeout was blocked by synchronous WebDriver drop")
            .is_true();
        tokio::time::sleep(delete_delay + Duration::from_millis(50)).await;
        assert_that!(
            server.hits("/session/fixture-session") + server.hits("/session/fixture-session/")
        )
        .with_detail_message("the detached quit request must continue exactly once")
        .is_equal_to(1);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_webdriver_connection_closes_new_session()
    -> Result<(), rootcause::Report> {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("webdriver-connect-cancellation")?;
        let new_session =
            br#"{"value":{"sessionId":"fixture-session","capabilities":{}}}"#.to_vec();
        let server = FixtureServer::start(HashMap::from([
            (
                "/status".to_owned(),
                ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
            ),
            (
                "/session".to_owned(),
                ResponseSpec::Delay(Duration::from_millis(200), Bytes::from(new_session)),
            ),
            (
                "/session/fixture-session".to_owned(),
                ResponseSpec::body(br#"{"value":null}"#.to_vec()),
            ),
            (
                "/session/fixture-session/".to_owned(),
                ResponseSpec::body(br#"{"value":null}"#.to_vec()),
            ),
            (
                "/session/fixture-session/timeouts".to_owned(),
                ResponseSpec::body(br#"{"value":null}"#.to_vec()),
            ),
        ]))
        .await?;
        let manager = ChromeForTestingManager::new_with_config(
            ChromeForTestingManagerConfig::builder()
                .cache_dir(directory.path().join("cache"))
                .build(),
        )?;
        let executable = directory.path().join("fake-chrome.sh");
        tokio::fs::write(
            &executable,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "echo \"ChromeDriver was started successfully on port {}.\"\n",
                    "trap 'exit 0' TERM INT\n",
                    "while :; do :; done\n",
                ),
                server.port()
            ),
        )
        .await?;
        let mut permissions = tokio::fs::metadata(&executable).await?.permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(&executable, permissions).await?;

        let cache_lease = CacheDir::create_at(manager.cache_dir().to_owned())?
            .acquire_shared(CancellationToken::new())
            .await?;
        let loaded = LoadedBrowserPackage::new(
            ChromeBinary::Chrome,
            directory.path().join("browser"),
            executable,
            cache_lease,
        );
        let process = manager
            .launch_driver(
                &loaded,
                ChromeDriverConfig::builder()
                    .port(Port::new(server.port()))
                    .build(),
                CancellationToken::new(),
            )
            .await?;
        let selected = test_selected_version(&manager);
        let chrome = ChromeForTesting::from_test_parts(manager, selected, loaded, process);
        let cancellation = CancellationToken::new();
        let cancel_during_connection = async {
            server.wait_for_hits("/session", 1).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            chrome
                .session()
                .with_cancellation(cancellation.clone())
                .run(async |_session| -> Result<(), WebDriverError> {
                    panic!("callback must not run after connection cancellation");
                },),
            cancel_during_connection,
        );
        let error = result.expect_err("connection cancellation must be reported");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        assert_that!(
            server.hits("/session/fixture-session") + server.hits("/session/fixture-session/")
        )
        .is_equal_to(1);
        chrome.shutdown().await?;
        Ok(())
    }

    #[cfg(unix)]
    fn test_selected_version(manager: &ChromeForTestingManager) -> SelectedVersion {
        SelectedVersion {
            channel: None,
            version: "135.0.7019.0".parse().expect("valid version literal"),
            platform: manager.platform(),
            requested_artifacts: ChromeBinary::Chrome.into(),
            chrome: None,
            chrome_headless_shell: None,
            chromedriver: None,
        }
    }
}
