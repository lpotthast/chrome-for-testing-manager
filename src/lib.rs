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
        let chromedriver = Chromedriver::run_latest_stable().await?;

        chromedriver
            .with_session(async |session| {
                session.goto("https://www.google.com").await?;
                let url = session.current_url().await?;
                assert_that(url).has_display_value("https://www.google.com/");
                Ok(())
            })
            .await?;

        chromedriver.terminate().await?;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    #[cfg(feature = "thirtyfour")]
    async fn latest_stable_with_caps() -> anyhow::Result<()> {
        let chromedriver = Chromedriver::run_latest_stable().await?;

        chromedriver
            .with_custom_session(
                |caps| caps.unset_headless(),
                async |session| {
                    session.goto("https://www.google.com").await?;
                    let url = session.current_url().await?;
                    assert_that(url).has_display_value("https://www.google.com/");
                    Ok(())
                },
            )
            .await?;

        chromedriver.terminate().await?;

        Ok(())
    }
}
