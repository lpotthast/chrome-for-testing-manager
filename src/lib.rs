mod cache;
pub mod chromedriver;
mod download;
pub mod mgr;
pub mod port;
pub mod session;

pub mod prelude {
    pub use crate::chromedriver::Chromedriver;
    pub use crate::mgr::ChromeForTestingManager;
    pub use crate::mgr::VersionRequest;
    pub use crate::port::Port;
    pub use crate::port::PortRequest;
    pub use crate::session::Session;
    pub use chrome_for_testing::api::channel::Channel;
    pub use chrome_for_testing::api::version::Version;
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use crate::session::SessionError;
    use assertr::prelude::*;
    use serial_test::serial;
    use thirtyfour::ChromiumLikeCapabilities;

    #[ctor::ctor]
    fn init_test_tracing() {
        tracing_subscriber::fmt().with_test_writer().try_init().ok();
    }

    #[tokio::test]
    #[serial]
    #[cfg(feature = "thirtyfour")]
    async fn latest_stable() -> anyhow::Result<()> {
        // NOTE: Using beta channel as stable channel chromedriver was bugged on Linux...
        let chromedriver = Chromedriver::run_latest_beta().await?;
        chromedriver.with_session(test_google).await?;
        chromedriver.terminate().await?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    #[cfg(feature = "thirtyfour")]
    async fn latest_stable_with_caps() -> anyhow::Result<()> {
        // NOTE: Using beta channel as stable channel chromedriver was bugged on Linux...
        let chromedriver = Chromedriver::run_latest_beta().await?;
        chromedriver
            .with_custom_session(|caps| caps.unset_headless(), test_google)
            .await?;
        chromedriver.terminate().await?;
        Ok(())
    }

    async fn test_google(session: &Session) -> Result<(), SessionError> {
        session.goto("https://www.google.com").await?;
        let url = session.current_url().await?;
        assert_that(url).has_display_value("https://www.google.com/");
        Ok(())
    }
}
