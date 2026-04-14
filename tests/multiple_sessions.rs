use assertr::prelude::*;
use chrome_for_testing_manager::*;
use rootcause::Report;
use std::sync::Arc;
use tokio::task::JoinSet;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn multiple_sessions() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Arc::new(Chromedriver::run(ChromedriverRunConfig::default()).await?);

    let mut tests = JoinSet::new();
    for _ in 0..5 {
        let chromedriver = Arc::clone(&chromedriver);
        tests.spawn(async move {
            chromedriver
                .with_session(common::wikipedia::test_wikipedia)
                .await
        });
    }

    let results = tests.join_all().await;
    for result in results {
        assert_that!(result).is_ok();
    }

    let _exit_status = Arc::try_unwrap(chromedriver)
        .expect("no more clones of chromedriver to be alive")
        .terminate()
        .await?;

    Ok(())
}
