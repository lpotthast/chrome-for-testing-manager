mod cache;
mod chromedriver;
mod download;
mod mgr;
mod port;
mod session;

pub mod prelude {
    pub use crate::chromedriver::Chromedriver;
    pub use crate::mgr::ChromeForTestingManager;
    pub use crate::mgr::VersionRequest;
    pub use crate::session::Session;
    pub use crate::session::SessionHandle;
    pub use chrome_for_testing::api::channel::Channel;
    pub use chrome_for_testing::api::version::Version;
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use assertr::prelude::*;
    use serial_test::serial;
    use thirtyfour::ChromiumLikeCapabilities;

    #[ctor::ctor]
    fn init_test_tracing() {
        tracing_subscriber::fmt().with_test_writer().try_init().ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    #[cfg(feature = "thirtyfour")]
    async fn latest_stable() -> anyhow::Result<()> {
        let mut chromedriver = Chromedriver::run_latest_stable().await?;
        let (handle, session) = chromedriver.new_session().await?;

        session.goto("https://www.google.com").await?;

        let url = session.current_url().await?;
        assert_that(url).has_display_value("https://www.google.com/");

        chromedriver.quit(handle).await?;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    #[cfg(feature = "thirtyfour")]
    async fn latest_stable_with_caps() -> anyhow::Result<()> {
        let mut chromedriver = Chromedriver::run_latest_stable().await?;
        let (handle, session) = chromedriver
            .new_session_with_caps(|caps| caps.unset_headless())
            .await?;

        session.goto("https://www.google.com").await?;

        let url = session.current_url().await?;
        assert_that(url).has_display_value("https://www.google.com/");

        chromedriver.quit(handle).await?;

        Ok(())
    }
}
