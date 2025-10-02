use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;

#[tokio::test]
async fn unusable_on_non_multithreaded_runtime() {
    tracing_subscriber::fmt().try_init().ok();

    assert_that(Chromedriver::run_latest_beta().await)
        .is_err()
        .derive(|it| it.to_string())
        .is_equal_to(indoc::formatdoc! {r#"
            The Chromedriver type requires a multithreaded tokio runtime,
            as we rely on async-drop functionality not available on a single-threaded runtime.
            
            Detected runtime flavor: CurrentThread.
            
            If you are writing a test, annotate it with `#[tokio::test(flavor = "multi_thread")]`.
        "#});
}
