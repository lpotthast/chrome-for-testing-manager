//! Smoke test for a non-headless session created via [`Chromedriver::session`] with a caps setup.

use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig};
use rootcause::Report;
use thirtyfour::ChromiumLikeCapabilities;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn single_session_non_headless() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    Chromedriver::run(ChromedriverRunConfig::default())
        .await?
        .session()
        .with_caps(ChromiumLikeCapabilities::unset_headless)
        .run(common::wikipedia::test_wikipedia)
        .await?;

    Ok(())
}
