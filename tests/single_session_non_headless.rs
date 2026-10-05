//! Smoke test for a non-headless session created via [`ChromeForTesting::session`] with a caps setup.

use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;
use thirtyfour::ChromiumLikeCapabilities;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn single_session_non_headless() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chrome = ChromeForTesting::launch(common::chrome_config()).await?;
    chrome
        .session()
        .with_caps(ChromiumLikeCapabilities::unset_headless)
        .run(common::browser_flow::exercise_browser_flow)
        .await?;
    chrome.shutdown().await?;

    Ok(())
}
