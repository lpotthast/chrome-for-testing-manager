//! Verifies that [`Chromedriver::run`] rejects current-thread Tokio runtimes with a useful error.

use assertr::prelude::*;
use chrome_for_testing_manager::{Chromedriver, ChromedriverRunConfig};
use rootcause::Report;

#[tokio::test]
async fn unusable_on_non_multithreaded_runtime() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    assert_that!(Chromedriver::run(ChromedriverRunConfig::default()).await)
        .is_err()
        .derive(ToString::to_string)
        .contains("chromedriver requires a multi-threaded Tokio runtime")
        .contains("detected CurrentThread");

    Ok(())
}
