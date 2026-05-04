//! Exercises [`Chromedriver::terminate`] with custom [`TerminationTimeouts`] configured on the
//! run config.

use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig, TerminationTimeouts};
use rootcause::Report;
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn custom_termination_with_timeouts() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Chromedriver::run(
        ChromedriverRunConfig::builder()
            .termination_timeouts(
                TerminationTimeouts::builder()
                    .interrupt(Duration::from_secs(1))
                    .terminate(Duration::from_secs(1))
                    .build(),
            )
            .build(),
    )
    .await?;

    chromedriver
        .with_session(common::wikipedia::test_wikipedia)
        .await?;

    chromedriver.terminate().await?;

    Ok(())
}
