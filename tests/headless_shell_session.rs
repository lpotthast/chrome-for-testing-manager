//! Smoke test for scoped sessions using the Chrome Headless Shell binary.

use assertr::prelude::*;
use chrome_for_testing_manager::{ChromeBinary, Chromedriver, ChromedriverRunConfig, Session};
use rootcause::Report;
use std::time::Duration;
use thirtyfour::prelude::*;

#[tokio::test(flavor = "multi_thread")]
async fn headless_shell_session() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let config = ChromedriverRunConfig::builder()
        .chrome_binary(ChromeBinary::ChromeHeadlessShell)
        .build();

    Chromedriver::run(config)
        .await?
        .session()
        .run(test_local_page)
        .await?;

    Ok(())
}

async fn test_local_page(session: &Session) -> WebDriverResult<()> {
    session
        .goto("data:text/html,<title>Headless Shell</title><h1 id='ready'>ready</h1>")
        .await?;

    let _heading = session
        .query(By::Id("ready"))
        .wait(Duration::from_secs(2), Duration::from_millis(100))
        .exists()
        .await?;

    assert_that!(session.title().await?).is_equal_to("Headless Shell");

    Ok(())
}
