//! Smoke test for the single-session happy path via [`Chromedriver::with_session`].

use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig};
use rootcause::Report;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn single_session() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    Chromedriver::run(ChromedriverRunConfig::default())
        .await?
        .with_session(common::wikipedia::test_wikipedia)
        .await?;

    Ok(())
}
