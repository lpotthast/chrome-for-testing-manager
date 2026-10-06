//! Verifies that a managed environment works on a current-thread Tokio runtime.

use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;

mod common;

#[tokio::test]
async fn session_and_shutdown_work_on_a_current_thread_runtime() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chrome = ChromeForTesting::launch(common::chrome_config()).await?;
    chrome
        .session()
        .run(common::browser_flow::exercise_browser_flow)
        .await?;
    chrome.shutdown().await?;

    Ok(())
}

#[tokio::test]
async fn dropping_on_a_current_thread_runtime_does_not_block_or_panic() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let chrome = ChromeForTesting::launch(common::chrome_config()).await?;
    let port = chrome.driver_port();
    drop(chrome);

    // The driver is terminated gracefully in the background, which this runtime drives as soon as
    // the test yields.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::net::TcpStream::connect(("127.0.0.1", port.as_u16()))
        .await
        .is_ok()
    {
        if tokio::time::Instant::now() >= deadline {
            rootcause::bail!("the dropped driver is still listening on port {port}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(())
}
