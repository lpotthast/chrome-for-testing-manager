//! Exercises [`ChromeForTesting::shutdown`] with a custom [`GracefulShutdown`] lifecycle policy.

use chrome_for_testing_manager::{
    ChromeForTesting, ChromeForTestingConfig, GracefulShutdown, LifecyclePolicy,
};
use rootcause::Report;
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn custom_graceful_shutdown() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chrome = ChromeForTesting::launch(
        ChromeForTestingConfig::builder()
            .cache_dir(common::cache_dir())
            .lifecycle(
                LifecyclePolicy::builder()
                    .graceful_shutdown(
                        GracefulShutdown::builder()
                            .unix_sigint(Duration::from_secs(1))
                            .windows_ctrl_break(Duration::from_secs(1))
                            .build(),
                    )
                    .build(),
            )
            .build(),
    )
    .await?;

    chrome
        .session()
        .run(common::browser_flow::exercise_browser_flow)
        .await?;

    chrome.shutdown().await?;

    Ok(())
}
