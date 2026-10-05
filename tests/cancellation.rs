//! Exercises cancellation before launch and while a managed browser callback is running.

use assertr::prelude::*;
use chrome_for_testing_manager::{
    CancellationToken, ChromeForTesting, ChromeForTestingConfig, ChromeForTestingError, Port,
};
use futures::FutureExt;
use rootcause::Report;
use std::sync::{Arc, Mutex};
use thirtyfour::error::WebDriverError;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_before_launch_stops_before_setup() -> Result<(), Report> {
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = ChromeForTesting::launch(
        ChromeForTestingConfig::builder()
            .cancellation(cancellation)
            .build(),
    )
    .await
    .expect_err("a pre-cancelled launch must fail");
    assert_that!(matches!(
        error.current_context(),
        ChromeForTestingError::Cancelled
    ))
    .is_true();
    Ok(())
}

/// One shared launch covers both interrupted-callback scenarios: cooperative cancellation and a
/// panicking callback. Both must leave no `WebDriver` session behind.
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_callbacks_still_close_webdriver_sessions() -> Result<(), Report> {
    let chrome = ChromeForTesting::launch(common::chrome_config()).await?;
    let port = chrome.driver_port();

    let cancellation = CancellationToken::new();
    let callback_cancellation = cancellation.clone();
    let cancelled_session_id = Arc::new(Mutex::new(None::<String>));
    let callback_session_id = Arc::clone(&cancelled_session_id);
    let error = chrome
        .session()
        .with_cancellation(cancellation)
        .run(async move |session| {
            *callback_session_id
                .lock()
                .expect("session id mutex is not poisoned") =
                Some(session.session_id().to_string());
            callback_cancellation.cancel();
            std::future::pending::<Result<(), WebDriverError>>().await
        })
        .await
        .expect_err("a cancelled callback must fail");
    assert_that!(matches!(
        error.current_context(),
        ChromeForTestingError::Cancelled
    ))
    .is_true();
    assert_session_closed(port, &cancelled_session_id, "cancelled").await?;

    let panicked_session_id = Arc::new(Mutex::new(None::<String>));
    let callback_session_id = Arc::clone(&panicked_session_id);
    let panicked = std::panic::AssertUnwindSafe(chrome.session().run(
        async move |session| -> Result<(), WebDriverError> {
            *callback_session_id
                .lock()
                .expect("session id mutex is not poisoned") =
                Some(session.session_id().to_string());
            panic!("intentional callback panic");
        },
    ))
    .catch_unwind()
    .await;
    assert_that!(panicked.is_err()).is_true();
    assert_session_closed(port, &panicked_session_id, "panicked").await?;

    chrome.shutdown().await?;
    Ok(())
}

async fn assert_session_closed(
    port: Port,
    session_id: &Mutex<Option<String>>,
    scenario: &str,
) -> Result<(), Report> {
    let sessions = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/sessions"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let session_id = session_id
        .lock()
        .expect("session id mutex is not poisoned")
        .clone()
        .expect("callback observed a session id");
    assert_that!(sessions)
        .with_detail_message(format!(
            "{scenario} WebDriver session {session_id} remained active"
        ))
        .does_not_contain(&session_id);
    Ok(())
}
