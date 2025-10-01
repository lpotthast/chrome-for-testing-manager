use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;

#[tokio::test]
async fn single_session() {
    tracing_subscriber::fmt().try_init().ok();
    let rt = tokio::runtime::Handle::current();

    assert_that_panic_by(|| {
        let _ = rt.spawn(Chromedriver::run_latest_beta()).await;
    })
    .has_type::<&str>()
    .is_equal_to("asd");
}
