//! Builder for scoped, panic-safe `WebDriver` session execution.
//!
//! Capability and client configuration are applied before connection. Every acquired resource
//! (a Chrome Headless Shell, the `WebDriver` session) is owned by one cleanup guard from the moment
//! it exists, so errors, cancellation, panics, and dropped futures all flow through the same
//! ordered cleanup: quit the session, then terminate the shell.

use super::Session;
use super::headless_shell::HeadlessShellSession;
use crate::browser::{ChromeBinary, LoadedBrowserPackage};
use crate::error::{attach_child, operation_result_with_cleanup};
use crate::manager::ChromeForTestingManager;
use crate::{CancellationToken, ChromeForTestingError, Port};
use rootcause::prelude::ResultExt;
use rootcause::{IntoReportCollection, Report, markers::SendSync, report};
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use thirtyfour::prelude::WebDriverError;
use thirtyfour::{ChromeCapabilities, WebDriverBuilder};
use tokio_util::task::TaskTracker;

type CapsSetup<'a> =
    Box<dyn FnOnce(&mut ChromeCapabilities) -> Result<(), WebDriverError> + Send + 'a>;
type ConfigSetup<'a> = Box<dyn FnOnce(WebDriverBuilder) -> WebDriverBuilder + Send + 'a>;

/// A scoped, chainable builder for opening a `thirtyfour` [`Session`] against a running
/// [`crate::ChromeForTesting`].
///
/// Obtained via [`crate::ChromeForTesting::session`]. Optional setup steps:
///
/// - [`Self::with_cancellation`] opts into cooperative cancellation of the session run.
/// - [`Self::with_caps`] mutates the [`ChromeCapabilities`] before the session opens (e.g. add
///   Chrome args).
/// - [`Self::with_config`] receives the [`WebDriverBuilder`] and may configure the element poller,
///   user-agent, or HTTP client.
///
/// Call [`Self::run`] to open the session and execute the user closure inside scoped, panic-safe
/// cleanup that always quits the session.
pub struct SessionBuilder<'a> {
    manager: &'a ChromeForTestingManager,
    loaded: &'a LoadedBrowserPackage,
    driver_port: Port,
    cancellation: Option<CancellationToken>,
    caps_setups: Vec<CapsSetup<'a>>,
    config_setups: Vec<ConfigSetup<'a>>,
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
            caps_setups: Vec::new(),
            config_setups: Vec::new(),
        }
    }

    /// Provide a token for cooperative cancellation of this session run.
    ///
    /// Cancellation interrupts launching a Chrome Headless Shell and the user closure; a
    /// `WebDriver` connection in flight is completed and then closed. Cleanup itself is not
    /// cancellable. See the [crate-level cancellation section](crate#cancellation-and-drop-safety).
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Provide a closure that mutates the [`ChromeCapabilities`] used to create the session.
    ///
    /// The capabilities start out headless and pointing at the cached browser binary. Repeated
    /// calls compose: closures run in the order they were added.
    ///
    /// For Chrome Headless Shell, browser arguments are applied to the separately launched shell,
    /// which is always headless. Other `goog:chromeOptions` entries are rejected because
    /// `ChromeDriver` cannot apply them after attaching to that already-running process.
    #[must_use]
    pub fn with_caps<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ChromeCapabilities) -> Result<(), WebDriverError> + Send + 'a,
    {
        self.caps_setups.push(Box::new(f));
        self
    }

    /// Provide a closure that configures the [`WebDriverBuilder`] before the session is opened.
    ///
    /// The builder starts out with an HTTP client that bypasses proxies (the driver listens on
    /// `127.0.0.1`) and applies the `webdriver_request_timeout` of [`crate::NetworkPolicy`]. Because a
    /// client is supplied, [`WebDriverBuilder::request_timeout`] has no effect; replace the client
    /// through [`WebDriverBuilder::client`] for other HTTP settings. Repeated calls compose:
    /// closures run in the order they were added. The configuration also governs the requests
    /// that quit the session.
    #[must_use]
    pub fn with_config<F>(mut self, f: F) -> Self
    where
        F: FnOnce(WebDriverBuilder) -> WebDriverBuilder + Send + 'a,
    {
        self.config_setups.push(Box::new(f));
        self
    }

    /// Open a [`Session`], hand it to the user closure, and tear it down once the closure resolves
    /// or panics.
    ///
    /// Cleanup runs regardless of outcome. A panic in the user closure is caught, cleanup is
    /// attempted, and the original panic is always resumed.
    ///
    /// The closure's error type must be a [`std::error::Error`] or a [`rootcause::Report`]. To mix
    /// error types through `?`, return `Result<T, rootcause::Report>`; convert boxed errors
    /// (`Box<dyn Error + Send + Sync>`) with `rootcause::compat::IntoRootcause::into_rootcause`.
    ///
    /// If cancellation is requested while the `WebDriver` connection is in flight, the connection
    /// is completed, because abandoning it could leave an unreachable server-side session, and the
    /// new session is then closed before cancellation is reported. Cancellation during the user
    /// closure drops its future before cleanup begins. If this future itself is dropped, cleanup
    /// is handed to the Tokio runtime and runs in the background;
    /// [`crate::ChromeForTesting::shutdown`] waits for it before terminating `ChromeDriver`.
    ///
    /// Quitting the session waits at most the `session_cleanup_timeout` of [`crate::LifecyclePolicy`]; a
    /// session that cannot be quit in time is abandoned (`ChromeDriver` ends it when it
    /// terminates). A Chrome Headless Shell is terminated regardless.
    ///
    /// # Errors
    ///
    /// Returns [`ChromeForTestingError::Cancelled`] if cancellation is requested and cleanup
    /// succeeds. Other errors cover capability setup, session creation, the user closure, or
    /// cleanup. An operation error remains primary when cleanup also fails.
    pub async fn run<T, E, F>(self, f: F) -> Result<T, Report<ChromeForTestingError>>
    where
        F: for<'b> AsyncFnOnce(&'b Session) -> Result<T, E>,
        E: IntoReportCollection<SendSync>,
    {
        use futures::FutureExt;

        let Self {
            manager,
            loaded,
            driver_port: port,
            cancellation,
            caps_setups,
            config_setups,
        } = self;
        let cancellation = cancellation.unwrap_or_default();
        crate::check_cancelled(&cancellation)?;

        let mut caps = manager.prepare_caps(loaded)?;
        for setup in caps_setups {
            setup(&mut caps).context(ChromeForTestingError::ConfigureSessionCapabilities)?;
        }
        let mut resources = SessionCleanupGuard {
            resources: Some(SessionResources {
                session: None,
                headless_shell: None,
                cleanup_timeout: manager.lifecycle().session_cleanup_timeout(),
            }),
            background_cleanups: manager.session_cleanups().clone(),
        };
        if loaded.chrome_binary() == ChromeBinary::ChromeHeadlessShell {
            resources.get_mut().headless_shell = Some(
                HeadlessShellSession::launch(
                    loaded,
                    &mut caps,
                    manager.local_client(),
                    manager.lifecycle(),
                    &cancellation,
                )
                .await?,
            );
        }
        // A panic in a config closure unwinds through `resources`, whose drop terminates the shell.
        let builder = config_setups.into_iter().fold(
            thirtyfour::WebDriver::builder(format!("http://127.0.0.1:{port}"), caps)
                .client(manager.webdriver_client().clone()),
            |builder, setup| setup(builder),
        );
        if let Err(mut error) = connect(builder, port, &cancellation, resources.get_mut()).await {
            if let Some(shell_output) = resources.headless_shell_output() {
                error = error.attach(shell_output);
            }
            return operation_result_with_cleanup(Err(error), resources.cleanup().await);
        }

        let callback_result = {
            // `f` is called inside the future, so that a panic before its own future is returned
            // is caught as well.
            let mut callback = std::pin::pin!(
                AssertUnwindSafe(async { f(resources.session()).await }).catch_unwind()
            );
            tokio::select! {
                // Give an already-requested cancellation deterministic precedence over callback
                // completion so cleanup begins instead of returning a test result.
                biased;
                () = cancellation.cancelled() => None,
                result = &mut callback => Some(result),
            }
        };

        let cleanup_result = resources.cleanup().await;
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

/// Open the `WebDriver` session and store it in `resources`.
///
/// `POST /session` is not interrupted by cancellation: `ChromeDriver` can create the remote
/// session before the response reveals the session id needed to close it. A session connected
/// after cancellation is stored nonetheless, so that cleanup closes it.
async fn connect(
    builder: WebDriverBuilder,
    port: Port,
    cancellation: &CancellationToken,
    resources: &mut SessionResources,
) -> Result<(), Report<ChromeForTestingError>> {
    crate::check_cancelled(cancellation)?;
    let connection = builder
        .connect()
        .await
        .context(ChromeForTestingError::StartWebDriverSession { port });
    let connection = connection.map(|driver| resources.session = Some(Session { driver }));
    if !cancellation.is_cancelled() {
        return connection;
    }
    let mut cancelled = report!(ChromeForTestingError::Cancelled);
    if let Err(connection_error) = connection {
        attach_child(&mut cancelled, connection_error);
    }
    Err(cancelled)
}

/// The resources acquired for one session run.
struct SessionResources {
    session: Option<Session>,
    headless_shell: Option<HeadlessShellSession>,
    cleanup_timeout: Duration,
}

impl SessionResources {
    /// Quit the session within the cleanup timeout, then terminate the Headless Shell.
    async fn cleanup(self) -> Result<(), Report<ChromeForTestingError>> {
        let Self {
            session,
            headless_shell,
            cleanup_timeout,
        } = self;
        let quit_result = match session {
            Some(session) => session.quit_within(cleanup_timeout).await,
            None => Ok(()),
        };
        let shell_result = match headless_shell {
            Some(headless_shell) => headless_shell.terminate().await.map(drop),
            None => Ok(()),
        };
        operation_result_with_cleanup(quit_result, shell_result)
    }
}

/// Owns [`SessionResources`] until [`Self::cleanup`]. If dropped before (the run future was
/// dropped, or a setup closure panicked), it hands the cleanup to the Tokio runtime, tracked by
/// `background_cleanups` so that shutting down the environment can wait for it.
struct SessionCleanupGuard {
    resources: Option<SessionResources>,
    background_cleanups: TaskTracker,
}

impl SessionCleanupGuard {
    fn get_mut(&mut self) -> &mut SessionResources {
        self.resources
            .as_mut()
            .expect("session guard owns resources until cleanup")
    }

    /// The connected session.
    fn session(&self) -> &Session {
        self.resources
            .as_ref()
            .and_then(|resources| resources.session.as_ref())
            .expect("session guard owns a connected session")
    }

    /// The recent output of the Chrome Headless Shell, formatted as a report attachment.
    fn headless_shell_output(&self) -> Option<String> {
        self.resources
            .as_ref()
            .and_then(|resources| resources.headless_shell.as_ref())
            .and_then(HeadlessShellSession::formatted_recent_output)
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
        if resources.session.is_none() && resources.headless_shell.is_none() {
            return;
        }

        // Drop cannot await `WebDriver::quit`, so transfer the acquired resources to the active
        // runtime. Without a live runtime cleanup cannot run, and after detaching there is no
        // caller to receive a cleanup error.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.background_cleanups.spawn_on(
            async move {
                if let Err(error) = resources.cleanup().await {
                    tracing::error!(%error, "failed to clean up dropped browser session");
                }
            },
            &handle,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::facade::ChromeForTesting;
    use crate::test_support::{FixtureServer, ResponseSpec, TestDirectory};
    #[cfg(unix)]
    use crate::version::SelectedVersion;
    #[cfg(unix)]
    use crate::{ChromeBinary, ChromeDriverConfig};
    use crate::{
        ChromeForTestingManager, ChromeForTestingManagerConfig, LoadedBrowserPackage, Port,
    };
    use assertr::prelude::*;
    use axum::body::Bytes;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[tokio::test(flavor = "multi_thread")]
    async fn unanswered_quit_is_abandoned_at_the_cleanup_deadline() -> Result<(), rootcause::Report>
    {
        let new_session =
            br#"{"value":{"sessionId":"fixture-session","capabilities":{}}}"#.to_vec();
        let delete_response = Bytes::from_static(br#"{"value":null}"#);
        let delete_delay = Duration::from_secs(10);
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
        let started = Instant::now();

        let error = SessionResources {
            session: Some(Session { driver }),
            headless_shell: None,
            cleanup_timeout: Duration::from_millis(100),
        }
        .cleanup()
        .await
        .expect_err("an unanswered quit must fail cleanup at its deadline");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::QuitSession
        ))
        .is_true();
        assert_that!(started.elapsed() < delete_delay / 2)
            .with_detail_message("cleanup must not wait for the unanswered quit")
            .is_true();
        // An abandoned handle is leaked, so `thirtyfour` does not retry the quit synchronously
        // when it is dropped.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_that!(
            server.hits("/session/fixture-session") + server.hits("/session/fixture-session/")
        )
        .with_detail_message("the quit request must be sent exactly once")
        .is_equal_to(1);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_webdriver_connection_closes_new_session()
    -> Result<(), rootcause::Report> {
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
        let chrome = fixture_environment(&directory, &server).await?;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_setup_steps_compose_in_order() -> Result<(), rootcause::Report> {
        use thirtyfour::{BrowserCapabilitiesHelper, ChromiumLikeCapabilities};

        let directory = TestDirectory::new("session-builder-composition")?;
        let manager = ChromeForTestingManager::new_with_config(
            ChromeForTestingManagerConfig::builder()
                .cache_dir(directory.path().join("cache"))
                .build(),
        )?;
        let cache_lease = crate::test_support::cache_lease(manager.cache_dir()).await?;
        let loaded = LoadedBrowserPackage::new(
            ChromeBinary::Chrome,
            directory.path().join("browser"),
            directory.path().join("driver"),
            cache_lease,
        );
        // Borrowed (non-`'static`) state, as a caller's configuration typically is.
        let args = ["--first".to_owned(), "--second".to_owned()];
        let user_agents = ["first-agent", "second-agent"];

        let builder = SessionBuilder::new(&manager, &loaded, Port::new(1))
            .with_caps(|caps| caps.add_arg(&args[0]))
            .with_caps(|caps| caps.add_arg(&args[1]))
            .with_config(|builder| builder.user_agent(user_agents[0]))
            .with_config(|builder| builder.user_agent(user_agents[1]));

        let mut caps = ChromeCapabilities::new();
        for setup in builder.caps_setups {
            setup(&mut caps)?;
        }
        assert_that!(caps.args()).is_equal_to(args.to_vec());
        let configured = builder.config_setups.into_iter().fold(
            thirtyfour::WebDriver::builder("http://127.0.0.1:1", ChromeCapabilities::new()),
            |builder, setup| setup(builder),
        );
        assert_that!(format!("{configured:?}")).contains("second-agent");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn synchronous_callback_panic_is_caught_and_cleanup_awaited()
    -> Result<(), rootcause::Report> {
        use futures::FutureExt;

        let directory = TestDirectory::new("session-synchronous-panic")?;
        let server = FixtureServer::start(session_routes(Duration::from_millis(200))).await?;
        let chrome = fixture_environment(&directory, &server).await?;

        #[expect(
            unreachable_code,
            reason = "the closure panics before returning its future"
        )]
        let panicked = AssertUnwindSafe(chrome.session().run(|_session: &Session| {
            panic!("intentional synchronous panic");
            async { Ok::<(), WebDriverError>(()) }
        }))
        .catch_unwind()
        .await;

        let payload = panicked.expect_err("the callback panic must be resumed");
        assert_that!(payload.downcast_ref::<&str>().copied())
            .is_equal_to(Some("intentional synchronous panic"));
        assert_that!(quit_hits(&server))
            .with_detail_message("cleanup must be awaited, not handed to the runtime")
            .is_equal_to(1);
        chrome.shutdown().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_waits_for_cleanup_of_dropped_session_runs() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("session-dropped-run-cleanup")?;
        let quit_delay = Duration::from_millis(500);
        let server = FixtureServer::start(session_routes(quit_delay)).await?;
        let chrome = fixture_environment(&directory, &server).await?;

        let run = chrome
            .session()
            .run(async |_session| -> Result<(), WebDriverError> { std::future::pending().await });
        let dropped = tokio::time::timeout(Duration::from_millis(200), run).await;
        assert_that!(dropped.is_err()).is_true();

        let started = Instant::now();
        chrome.shutdown().await?;
        assert_that!(quit_hits(&server)).is_equal_to(1);
        assert_that!(started.elapsed() >= quit_delay / 2)
            .with_detail_message("shutdown must wait for the background quit to finish")
            .is_true();
        Ok(())
    }

    /// `WebDriver` routes of one fixture session whose quit is answered after `quit_delay`.
    #[cfg(unix)]
    fn session_routes(quit_delay: Duration) -> HashMap<String, ResponseSpec> {
        let quit_response = Bytes::from_static(br#"{"value":null}"#);
        HashMap::from([
            (
                "/status".to_owned(),
                ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
            ),
            (
                "/session".to_owned(),
                ResponseSpec::body(
                    br#"{"value":{"sessionId":"fixture-session","capabilities":{}}}"#.to_vec(),
                ),
            ),
            (
                "/session/fixture-session".to_owned(),
                ResponseSpec::Delay(quit_delay, quit_response.clone()),
            ),
            (
                "/session/fixture-session/".to_owned(),
                ResponseSpec::Delay(quit_delay, quit_response),
            ),
            (
                "/session/fixture-session/timeouts".to_owned(),
                ResponseSpec::body(br#"{"value":null}"#.to_vec()),
            ),
        ])
    }

    #[cfg(unix)]
    fn quit_hits(server: &FixtureServer) -> usize {
        server.hits("/session/fixture-session") + server.hits("/session/fixture-session/")
    }

    /// An environment whose fake `ChromeDriver` reports the port of `server`, which answers its
    /// status and `WebDriver` requests.
    #[cfg(unix)]
    async fn fixture_environment(
        directory: &TestDirectory,
        server: &FixtureServer,
    ) -> Result<ChromeForTesting, rootcause::Report> {
        let manager = ChromeForTestingManager::new_with_config(
            ChromeForTestingManagerConfig::builder()
                .cache_dir(directory.path().join("cache"))
                .build(),
        )?;
        let executable = directory.path().join("fake-chrome.sh");
        crate::test_support::write_executable(
            &executable,
            &format!(
                concat!(
                    "#!/bin/sh\n",
                    "trap 'exit 0' TERM INT\n",
                    "echo \"ChromeDriver was started successfully on port {}.\"\n",
                    "while :; do sleep 1 & wait $!; done\n",
                ),
                server.port()
            ),
        )
        .await?;

        let cache_lease = crate::test_support::cache_lease(manager.cache_dir()).await?;
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
        Ok(ChromeForTesting::from_test_parts(
            manager, selected, loaded, process,
        ))
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
