use chrome_for_testing_manager::prelude::*;
use thirtyfour::prelude::*;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn single_session_non_headless() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    // NOTE: Using beta channel as stable channel chromedriver was bugged on Linux...
    Chromedriver::run_latest_beta()
        .await?
        .with_custom_session(
            |caps| caps.unset_headless(),
            common::wikipedia::test_wikipedia,
        )
        .await
}
