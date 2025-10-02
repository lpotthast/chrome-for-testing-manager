use crate::mgr::{ChromeForTestingManager, LoadedChromePackage, VersionRequest};
use crate::port::{Port, PortRequest};
use anyhow::anyhow;
use chrome_for_testing::api::channel::Channel;
use std::fmt::{Debug, Formatter};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::{TerminateOnDrop, TerminationError};

/// A wrapper struct for a spawned chromedriver process.
/// Keep this alive until your test is complete.
///
/// Automatically terminates the spawned chromedriver process when dropped.
///
/// You can always manually call `terminate`, but the on-drop automation makes it much safer in
/// quickly panicking contexts, such as tests.
pub struct Chromedriver {
    /// The manager instance used to resolve a version, download it and starting the chromedriver.
    mgr: ChromeForTestingManager,

    /// Chrome and chromedriver binaries used for testing.
    loaded: LoadedChromePackage,

    /// The running chromedriver process. Terminated when dropped.
    ///
    /// Always stores a process handle. The value is only taken out on termination,
    /// notifying our `Drop` impl that the process was gracefully terminated when seeing `None`.
    chromedriver_process: Option<TerminateOnDrop<BroadcastOutputStream>>,

    /// The port the chromedriver process listens on.
    chromedriver_port: Port,
}

impl Debug for Chromedriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chromedriver")
            .field("mgr", &self.mgr)
            .field("loaded", &self.loaded)
            .field("chromedriver_process", &self.chromedriver_process)
            .field("chromedriver_port", &self.chromedriver_port)
            .finish()
    }
}

impl Chromedriver {
    pub async fn run(version: VersionRequest, port: PortRequest) -> anyhow::Result<Chromedriver> {
        // Assert that async-drop will work.
        // This is the only way of constructing a `Chromedriver` instance,
        // so it's safe to do this here.
        match tokio::runtime::Handle::current().runtime_flavor() {
            RuntimeFlavor::MultiThread => { /* we want this */ }
            unsupported_flavor => {
                return Err(anyhow!(indoc::formatdoc! {r#"
                    The Chromedriver type requires a multithreaded tokio runtime,
                    as we rely on async-drop functionality not available on a single-threaded runtime.

                    Detected runtime flavor: {unsupported_flavor:?}.

                    If you are writing a test, annotate it with `#[tokio::test(flavor = "multi_thread")]`.
                "#}));
            }
        }

        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(version).await?;
        let loaded = mgr.download(selected).await?;
        let (chromedriver_process, chromedriver_port) =
            mgr.launch_chromedriver(&loaded, port).await?;
        Ok(Chromedriver {
            chromedriver_process: Some(
                chromedriver_process
                    .terminate_on_drop(Duration::from_secs(3), Duration::from_secs(3)),
            ),
            chromedriver_port,
            loaded,
            mgr,
        })
    }

    pub async fn run_latest_stable() -> anyhow::Result<Chromedriver> {
        Self::run(VersionRequest::LatestIn(Channel::Stable), PortRequest::Any).await
    }

    pub async fn run_latest_beta() -> anyhow::Result<Chromedriver> {
        Self::run(VersionRequest::LatestIn(Channel::Beta), PortRequest::Any).await
    }

    pub async fn run_latest_dev() -> anyhow::Result<Chromedriver> {
        Self::run(VersionRequest::LatestIn(Channel::Dev), PortRequest::Any).await
    }

    pub async fn run_latest_canary() -> anyhow::Result<Chromedriver> {
        Self::run(VersionRequest::LatestIn(Channel::Canary), PortRequest::Any).await
    }

    pub async fn terminate(self) -> Result<ExitStatus, TerminationError> {
        self.terminate_with_timeouts(Duration::from_secs(3), Duration::from_secs(3))
            .await
    }

    pub async fn terminate_with_timeouts(
        mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        self.chromedriver_process
            .take()
            .expect("present")
            .terminate(interrupt_timeout, terminate_timeout)
            .await
    }

    /// Execute an async closure with a WebDriver session.
    /// The session will be automatically cleaned up after the closure completes.
    #[cfg(feature = "thirtyfour")]
    pub async fn with_session(
        &self,
        f: impl AsyncFnOnce(&crate::session::Session) -> Result<(), crate::session::SessionError>,
    ) -> anyhow::Result<()> {
        self.with_custom_session(|_caps| Ok(()), f).await
    }

    /// Execute an async closure with a custom-configured WebDriver session.
    /// The session will be automatically cleaned up after the closure completes.
    #[cfg(feature = "thirtyfour")]
    pub async fn with_custom_session<F>(
        &self,
        setup: impl Fn(
            &mut thirtyfour::ChromeCapabilities,
        ) -> Result<(), thirtyfour::prelude::WebDriverError>,
        f: F,
    ) -> anyhow::Result<()>
    where
        F: for<'a> AsyncFnOnce(
            &'a crate::session::Session,
        ) -> Result<(), crate::session::SessionError>,
    {
        use crate::session::Session;
        use anyhow::Context;
        use futures::FutureExt;

        let mut caps = self.mgr.prepare_caps(&self.loaded).await?;
        setup(&mut caps).context("Failed to set up chrome capabilities.")?;
        let driver = thirtyfour::WebDriver::new(
            format!("http://localhost:{}", self.chromedriver_port),
            caps,
        )
        .await?;

        let session = Session { driver };

        // Execute the user function.
        let maybe_panicked = core::panic::AssertUnwindSafe(f(&session))
            .catch_unwind()
            .await;

        // No matter what happened, clean up the session!
        session.quit().await?;

        // Handle panics.
        let result = maybe_panicked.map_err(|err| {
            let err = anyhow::anyhow!("{err:?}");
            crate::session::SessionError::Panic {
                reason: err.to_string(),
            }
        })?;

        // Map the `SessionError` into an `anyhow::Error`.
        result.map_err(Into::into)
    }
}
