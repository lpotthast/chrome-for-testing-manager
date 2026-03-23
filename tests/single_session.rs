use chrome_for_testing_manager::*;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn single_session() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    // NOTE: Using beta channel as stable channel chromedriver was bugged on Linux...
    Chromedriver::run_latest_beta()
        .await?
        .with_session(common::wikipedia::test_wikipedia)
        .await
}
