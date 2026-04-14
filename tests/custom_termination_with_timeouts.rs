use chrome_for_testing_manager::*;
use rootcause::Report;
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn custom_termination_with_timeouts() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Chromedriver::run(ChromedriverRunConfig::default()).await?;

    chromedriver
        .with_session(common::wikipedia::test_wikipedia)
        .await?;

    chromedriver
        .terminate_with_timeouts(Duration::from_secs(1), Duration::from_secs(1))
        .await?;

    Ok(())
}
