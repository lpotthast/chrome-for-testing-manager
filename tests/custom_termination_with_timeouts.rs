use chrome_for_testing_manager::prelude::*;
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn custom_termination_with_timeouts() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    // NOTE: Using beta channel as stable channel chromedriver was bugged on Linux...
    let chromedriver = Chromedriver::run_latest_beta().await?;

    chromedriver
        .with_session(common::wikipedia::test_wikipedia)
        .await?;

    chromedriver
        .terminate_with_timeouts(Duration::from_secs(1), Duration::from_secs(1))
        .await?;

    Ok(())
}
