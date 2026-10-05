//! Smoke test for the single-session happy path via [`ChromeForTesting::session`].

use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn single_session() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chrome = ChromeForTesting::launch(common::chrome_config()).await?;
    chrome
        .session()
        .run(common::browser_flow::exercise_browser_flow)
        .await?;
    chrome.shutdown().await?;

    Ok(())
}
