use crate::mgr::{ChromeForTestingManager, LoadedChromePackage, VersionRequest};
use crate::port::{Port, PortRequest};
use crate::prelude::{Session, SessionHandle};
use crate::session::SessionError;
use chrome_for_testing::api::channel::Channel;
use std::collections::HashMap;
use std::process::ExitStatus;
use std::time::Duration;
use tokio_process_tools::{ProcessHandle, TerminationError};

/// A wrapper struct for a spawned chromedriver process.
/// Keep this alive until your test is complete.
#[derive(Debug)]
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
    pub(crate) chromedriver_process: Option<ProcessHandle>,

    /// The port the chromedriver process listens on.
    #[allow(unused)]
    pub(crate) chromedriver_port: Port,

    /// List of browser sessions created.
    /// Session ownership must never leave this struct to enforce that `chromedriver` will
    /// outlive all sessions.
    pub(crate) sessions: HashMap<SessionHandle, Session>,
}

impl Drop for Chromedriver {
    fn drop(&mut self) {
        if self.chromedriver_process.is_some() {
            let backtrace = std::backtrace::Backtrace::capture();
            tracing::error!(?backtrace, "Leaking non-terminated chromedriver process. Call `chromedriver.terminate()` to terminate it gracefully!");
        }
    }
}

impl Chromedriver {
    pub async fn run(version: VersionRequest, port: PortRequest) -> anyhow::Result<Chromedriver> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(version).await?;
        let loaded = mgr.download(selected).await?;
        let (chromedriver_process, chromedriver_port) =
            mgr.launch_chromedriver(&loaded, port).await?;
        Ok(Chromedriver {
            chromedriver_process: Some(chromedriver_process),
            chromedriver_port,
            loaded,
            mgr,
            sessions: Default::default(),
        })
    }

    pub async fn run_latest_stable() -> anyhow::Result<Chromedriver> {
        Self::run(VersionRequest::LatestIn(Channel::Stable), PortRequest::Any).await
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

    #[cfg(feature = "thirtyfour")]
    async fn new_session_with_caps(
        &mut self,
        setup: impl Fn(
            &mut thirtyfour::ChromeCapabilities,
        ) -> Result<(), thirtyfour::prelude::WebDriverError>,
    ) -> anyhow::Result<(SessionHandle, &Session)> {
        use anyhow::Context;

        let mut caps = self.mgr.prepare_caps(&self.loaded).await?;
        setup(&mut caps).context("Failed to setup chrome capabilities.")?;
        let driver = thirtyfour::WebDriver::new(
            format!("http://localhost:{}", self.chromedriver_port),
            caps,
        )
        .await?;

        let handle = SessionHandle {
            session_id: uuid::Uuid::now_v7(),
        };
        self.sessions.insert(handle, Session { driver });

        Ok((handle, self.sessions.get(&handle).expect("present")))
    }

    #[cfg(feature = "thirtyfour")]
    pub async fn with_session(
        &mut self,
        f: impl AsyncFnOnce(&Session) -> Result<(), SessionError>,
    ) -> anyhow::Result<()> {
        self.with_custom_session(|_caps| Ok(()), f).await
    }

    #[cfg(feature = "thirtyfour")]
    pub async fn with_custom_session(
        &mut self,
        setup: impl Fn(
            &mut thirtyfour::ChromeCapabilities,
        ) -> Result<(), thirtyfour::prelude::WebDriverError>,
        f: impl AsyncFnOnce(&Session) -> Result<(), SessionError>,
    ) -> anyhow::Result<()> {
        let (handle, _) = self.new_session_with_caps(setup).await?;

        let session = self.sessions.remove(&handle).expect("present");

        let result = f(&session).await;

        session.quit().await?;
        result.map_err(Into::into)
    }

    pub fn expect_session(&self, handle: &SessionHandle) -> &Session {
        self.get_session(handle).expect("present")
    }

    pub fn get_session(&self, handle: &SessionHandle) -> Option<&Session> {
        self.sessions.get(handle)
    }

    pub async fn quit(&mut self, handle: SessionHandle) -> Result<(), SessionError> {
        let session = self.sessions.remove(&handle);
        if session.is_none() {
            return Ok(());
        }
        let session = session.unwrap();
        session.quit().await
    }
}
