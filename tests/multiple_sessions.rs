use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use thirtyfour::prelude::*;
use tokio::task::JoinSet;

#[tokio::test(flavor = "multi_thread")]
async fn multiple_sessions() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    // NOTE: Using beta channel as stable channel chromedriver was bugged on Linux...
    let chromedriver = Arc::new(Chromedriver::run_latest_beta().await?);

    let mut tests = JoinSet::new();
    for _ in 0..5 {
        let chromedriver = Arc::clone(&chromedriver);
        tests.spawn(async move {
            chromedriver
                .with_custom_session(
                    |caps| caps.unset_headless(),
                    async |session| {
                        // Navigate to https://wikipedia.org.
                        session.goto("https://wikipedia.org").await?;
                        let search_form = session.find(By::Id("search-form")).await?;

                        // Find element from element.
                        let search_input = search_form.find(By::Id("searchInput")).await?;

                        // Type in the search terms.
                        search_input.send_keys("selenium").await?;

                        // Click the search button.
                        let submit_btn = search_form.find(By::Css("button[type='submit']")).await?;
                        submit_btn.click().await?;

                        // Look for heading to implicitly wait for the page to load.
                        let _heading = session
                            .query(By::Id("firstHeading"))
                            .wait(Duration::from_secs(2), Duration::from_micros(100))
                            .exists()
                            .await?;

                        assert_that(session.title().await?).is_equal_to("Selenium - Wikipedia");

                        Ok(())
                    },
                )
                .await
        });
    }

    let results = tests.join_all().await;
    for result in results {
        assert_that(result).is_ok();
    }

    let _exit_status = Arc::try_unwrap(chromedriver)
        .expect("no more clones of chromedriver to be alive")
        .terminate()
        .await?;

    Ok(())
}
