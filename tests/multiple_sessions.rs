use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use std::sync::Arc;
use thirtyfour::prelude::*;
use tokio::task::JoinSet;

#[tokio::test]
async fn multiple_sessions() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Arc::new(Chromedriver::run_latest_stable().await?);

    let mut tests = JoinSet::new();
    for _ in 0..5 {
        let chromedriver = Arc::clone(&chromedriver);
        tests.spawn(async move {
            chromedriver
                .with_custom_session(
                    |caps| caps.unset_headless(),
                    async |session| {
                        // Navigate to https://wikipedia.org.
                        session.goto("https://wikipedia.org").await.unwrap();
                        let search_form = session.find(By::Id("search-form")).await.unwrap();

                        // Find element from element.
                        let search_input = search_form.find(By::Id("searchInput")).await.unwrap();

                        // Type in the search terms.
                        search_input.send_keys("selenium").await.unwrap();

                        // Click the search button.
                        let submit_btn = search_form
                            .find(By::Css("button[type='submit']"))
                            .await
                            .unwrap();
                        submit_btn.click().await.unwrap();

                        // Look for heading to implicitly wait for the page to load.
                        let _heading = session.find(By::Id("firstHeading")).await.unwrap();

                        assert_that(session.title().await.unwrap())
                            .is_equal_to("Selenium – Wikipedia");

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
