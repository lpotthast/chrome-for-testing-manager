//! Exercises [`Chromedriver::terminate`] with a custom [`GracefulShutdown`] configured on the
//! run config.

use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig, GracefulShutdown};
use rootcause::Report;
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn custom_graceful_shutdown() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Chromedriver::run(
        ChromedriverRunConfig::builder()
            .graceful_shutdown(
                GracefulShutdown::builder()
                    .unix_sigint(Duration::from_secs(1))
                    .windows_ctrl_break(Duration::from_secs(1))
                    .build(),
            )
            .build(),
    )
    .await?;

    chromedriver
        .session()
        .run(common::wikipedia::test_wikipedia)
        .await?;

    chromedriver.terminate().await?;

    Ok(())
}
