use crate::mgr::{ChromeForTestingManager, LoadedChromePackage, VersionRequest};
use crate::port::{Port, PortRequest};
use crate::prelude::{Session, SessionHandle};
use crate::session::SessionError;
use chrome_for_testing::api::channel::Channel;
use std::collections::HashMap;
use tokio_process_tools::TerminateOnDrop;

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
    #[expect(unused)]
    pub(crate) chromedriver_process: TerminateOnDrop,

    /// The port the chromedriver process listens on.
    #[allow(unused)]
    pub(crate) chromedriver_port: Port,

    /// List of browser sessions created.
    /// Session ownership must never leave this struct to enforce that `chromedriver` will
    /// outlive all sessions.
    pub(crate) sessions: HashMap<SessionHandle, Session>,
}

impl Chromedriver {
    pub async fn run(version: VersionRequest, port: PortRequest) -> anyhow::Result<Chromedriver> {
        let mgr = ChromeForTestingManager::new();
        let selected = mgr.resolve_version(version).await?;
        let loaded = mgr.download(selected).await?;
        let (chromedriver, chromedriver_port) = mgr.launch_chromedriver(&loaded, port).await?;
        Ok(Chromedriver {
            chromedriver_process: chromedriver,
            chromedriver_port,
            loaded,
            mgr,
            sessions: Default::default(),
        })
    }

    pub async fn run_latest_stable() -> anyhow::Result<Chromedriver> {
        Self::run(VersionRequest::LatestIn(Channel::Stable), PortRequest::Any).await
    }

    #[cfg(feature = "thirtyfour")]
    pub async fn new_session(&mut self) -> anyhow::Result<(SessionHandle, &Session)> {
        self.new_session_with_caps(|_caps| Ok(())).await
    }

    #[cfg(feature = "thirtyfour")]
    pub async fn new_session_with_caps(
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
