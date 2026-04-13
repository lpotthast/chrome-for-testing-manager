use assertr::prelude::*;
use chrome_for_testing_manager::*;
use rootcause::Report;

#[tokio::test]
async fn unusable_on_non_multithreaded_runtime() -> Result<(), Report> {
    tracing_subscriber::fmt().try_init().ok();

    assert_that!(Chromedriver::run_latest_beta().await)
        .is_err()
        .derive(|it| it.to_string())
        .contains("chromedriver requires a multi-threaded Tokio runtime")
        .contains("detected CurrentThread");

    Ok(())
}
