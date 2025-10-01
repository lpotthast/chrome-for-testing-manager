use crate::mgr::{ChromeForTestingManager, LoadedChromePackage, VersionRequest};
use crate::port::{Port, PortRequest};
#[cfg(feature = "thirtyfour")]
use crate::session::{Session, SessionError};
use anyhow::anyhow;
use async_dropper::{AsyncDrop, AsyncDropper};
use async_trait::async_trait;
use chrome_for_testing::api::channel::Channel;
use futures::FutureExt;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::runtime::RuntimeFlavor;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::{OutputStream, ProcessHandle, TerminationError};

struct AutoTerminateProcess<T: OutputStream> {
    process: Option<ProcessHandle<T>>,
}

impl<T: OutputStream> Default for AutoTerminateProcess<T> {
    fn default() -> Self {
        Self { process: None }
    }
}

impl<T: OutputStream> Deref for AutoTerminateProcess<T> {
    type Target = Option<ProcessHandle<T>>;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

/// A wrapper struct for a spawned chromedriver process.
/// Keep this alive until your test is complete.
pub struct Chromedriver {
    /// The manager instance used to resolve a version, download it and starting the chromedriver.
    #[allow(unused)]
    pub(crate) mgr: ChromeForTestingManager,

    /// Chrome and chromedriver binaries used for testing.
    #[allow(unused)]
    pub(crate) loaded: LoadedChromePackage,

    /// The running chromedriver process. Terminated when dropped.
    ///
    /// Always stores a process handle. The value is only taken out on termination,
    /// notifying our `Drop` impl that the process was gracefully terminated when seeing `None`.
    pub(crate) chromedriver_process: AsyncDropper<AutoTerminateProcess<BroadcastOutputStream>>,

    /// The port the chromedriver process listens on.
    #[allow(unused)]
    pub(crate) chromedriver_port: Port,
}

impl Debug for Chromedriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chromedriver")
            .field("mgr", &self.mgr)
            .field("loaded", &self.loaded)
            .field("chromedriver_process", &self.chromedriver_process.process)
            .field("chromedriver_port", &self.chromedriver_port)
            .finish()
    }
}

//impl Drop for Chromedriver {
//    fn drop(&mut self) {
//        if self.chromedriver_process.is_some() {
//            let backtrace = std::backtrace::Backtrace::capture();
//            tracing::error!(
//                ?backtrace,
//                "Leaking non-terminated chromedriver process. Call `chromedriver.terminate()` to terminate it gracefully!"
//            );
//        }
//    }
//}

/// Implementation of [AsyncDrop] that specifies the actual behavior
#[async_trait]
impl<T: OutputStream + Send + 'static> AsyncDrop for AutoTerminateProcess<T> {
    // simulated work during async_drop
    async fn async_drop(&mut self) {
        let interrupt_timeout = Duration::from_secs(3);
        let terminate_timeout = Duration::from_secs(3);

        if let Some(mut process) = self.process.take() {
            tracing::info!("Terminating chromedriver");
            let _ = process
                .terminate(interrupt_timeout, terminate_timeout)
                .await;
            tracing::info!("chromedriver terminated successfully");
        }
    }

    //fn drop_timeout(&self) -> Duration {
    //    Duration::from_secs(7) // extended from default 3 seconds, as an example
    //}

    // NOTE: the method below is automatically derived for you, but you can override it
    // make sure that the object is equal to T::default() by the end, otherwise it will panic!
    // fn reset(&mut self) {
    //     self.0 = String::default();
    // }

    // NOTE: below was not implemented since we want the default of DropFailAction::Continue
    // fn drop_fail_action(&self) -> DropFailAction;
}

impl Chromedriver {
    pub async fn run(version: VersionRequest, port: PortRequest) -> anyhow::Result<Chromedriver> {
        // Assert that async-drop will work.
        // This is the only way of constructing a `Chromedriver` instance,
        // so it's safe to do this here.
        match tokio::runtime::Handle::current().runtime_flavor() {
            RuntimeFlavor::MultiThread => { /* we want this */ }
            unsupported_flavor => {
                return Err(anyhow!(
                    r#"
                    The Chromedriver type requires a multithreaded tokio runtime,
                    as we rely on async-drop functionality not available on a single-threaded runtime.

                    Detected runtime flavor: {unsupported_flavor:?}.
                    
                    If you are writing a test, annotate it with `#[tokio::test(flavor = "multi_thread")]`.
                    "#
                ));
            }
        }

        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(version).await?;
        let loaded = mgr.download(selected).await?;
        let (chromedriver_process, chromedriver_port) =
            mgr.launch_chromedriver(&loaded, port).await?;
        Ok(Chromedriver {
            chromedriver_process: AsyncDropper::new(AutoTerminateProcess {
                process: Some(chromedriver_process),
            }),
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
            .process
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
        f: impl AsyncFnOnce(&Session) -> Result<(), SessionError> + UnwindSafe,
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
        F: for<'a> AsyncFnOnce(&'a Session) -> Result<(), SessionError> + UnwindSafe,
    {
        use crate::session::Session;
        use anyhow::Context;

        let mut caps = self.mgr.prepare_caps(&self.loaded).await?;
        setup(&mut caps).context("Failed to set up chrome capabilities.")?;
        let driver = thirtyfour::WebDriver::new(
            format!("http://localhost:{}", self.chromedriver_port),
            caps,
        )
        .await?;

        let session = Session { driver };

        // Execute the user function
        let maybe_panicked = AssertUnwindSafe(f(&session)).catch_unwind().await;

        // No matter what happened, clean up the session!
        session.quit().await?;

        // Handle panics.
        let result = maybe_panicked.map_err(|err| {
            let err = anyhow::anyhow!("{err:?}");
            SessionError::Panic {
                reason: err.to_string(),
            }
        })?;

        // Map the `SessionError` into an `anyhow::Error`.
        result.map_err(Into::into)
    }
}
