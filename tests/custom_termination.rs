//! Exercises the explicit [`Chromedriver::terminate`] path (rather than relying on drop).

use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig};
use rootcause::Report;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn custom_termination() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Chromedriver::run(ChromedriverRunConfig::default()).await?;

    chromedriver
        .with_session(common::wikipedia::test_wikipedia)
        .await?;

    chromedriver.terminate().await?;

    Ok(())
}
