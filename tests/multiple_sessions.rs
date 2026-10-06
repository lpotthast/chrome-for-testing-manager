//! Verifies that multiple `WebDriver` sessions can run concurrently against a single shared
//! [`ChromeForTesting`].

use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;
use std::sync::Arc;
use tokio::task::JoinSet;

mod common;

/// Number of concurrent sessions. Windows CI runners struggle with many Chrome instances starting
/// at once, so run fewer there.
const SESSION_COUNT: usize = if cfg!(windows) { 2 } else { 5 };

#[tokio::test(flavor = "multi_thread")]
async fn multiple_sessions() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chrome = Arc::new(ChromeForTesting::launch(common::chrome_config()).await?);

    let mut tests = JoinSet::new();
    for _ in 0..SESSION_COUNT {
        let chrome = Arc::clone(&chrome);
        tests.spawn(async move {
            chrome
                .session()
                .run(common::browser_flow::exercise_browser_flow)
                .await
        });
    }

    for result in tests.join_all().await {
        result?;
    }

    let _exit_status = Arc::try_unwrap(chrome)
        .expect("no more clones of ChromeForTesting to be alive")
        .shutdown()
        .await?;

    Ok(())
}
