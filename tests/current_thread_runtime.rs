//! Verifies that [`ChromeForTesting::launch`] rejects current-thread Tokio runtimes with a typed error.

use assertr::prelude::*;
use chrome_for_testing_manager::{ChromeForTesting, ChromeForTestingConfig, ChromeForTestingError};
use rootcause::Report;

#[tokio::test]
async fn unusable_on_non_multithreaded_runtime() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    let error = ChromeForTesting::launch(ChromeForTestingConfig::default())
        .await
        .expect_err("a current-thread runtime must be rejected");
    assert_that!(matches!(
        error.current_context(),
        ChromeForTestingError::UnsupportedRuntime {
            runtime_flavor: tokio::runtime::RuntimeFlavor::CurrentThread,
            ..
        }
    ))
    .is_true();

    Ok(())
}
