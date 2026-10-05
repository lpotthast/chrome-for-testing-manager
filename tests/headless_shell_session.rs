//! Smoke test for scoped sessions using the Chrome Headless Shell binary.

use assertr::prelude::*;
use chrome_for_testing_manager::{ChromeBinary, ChromeForTesting, ChromeForTestingConfig, Session};
use rootcause::Report;
use std::time::Duration;
use thirtyfour::ChromiumLikeCapabilities;
use thirtyfour::prelude::*;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn headless_shell_session() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let config = ChromeForTestingConfig::builder()
        .cache_dir(common::cache_dir())
        .chrome_binary(ChromeBinary::ChromeHeadlessShell)
        .build();

    let chrome = ChromeForTesting::launch(config).await?;
    chrome.session().run(test_local_page).await?;
    chrome.shutdown().await?;

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

/// Several Headless Shell sessions run concurrently against one driver, each with browser
/// arguments borrowed from the caller and applied to its own separately launched shell.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_headless_shell_sessions_apply_borrowed_args() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let config = ChromeForTestingConfig::builder()
        .cache_dir(common::cache_dir())
        .chrome_binary(ChromeBinary::ChromeHeadlessShell)
        .build();
    let chrome = ChromeForTesting::launch(config).await?;
    let user_agents: Vec<String> = (0..3)
        .map(|index| format!("headless-shell-{index}"))
        .collect();

    let sessions = user_agents.iter().map(|user_agent| {
        chrome
            .session()
            .with_caps(move |caps| caps.add_arg(&format!("--user-agent={user_agent}")))
            .run(async |session: &Session| -> WebDriverResult<String> {
                session.goto("data:text/html,<title>agent</title>").await?;
                session
                    .execute("return navigator.userAgent;", Vec::new())
                    .await?
                    .convert::<String>()
            })
    });
    let reported = futures::future::try_join_all(sessions).await?;

    assert_that!(reported).is_equal_to(user_agents);
    chrome.shutdown().await?;
    Ok(())
}
