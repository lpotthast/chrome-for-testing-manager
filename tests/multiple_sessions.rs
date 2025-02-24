use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use std::sync::Arc;
use thirtyfour::prelude::*;

#[tokio::test]
async fn multiple_sessions() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Arc::new(Chromedriver::run_latest_stable().await?);

    let mut tests = Vec::new();
    for _ in 0..10 {
        let chromedriver = Arc::clone(&chromedriver);
        let test = tokio::spawn(async move {
            chromedriver
                .with_custom_session(
                    |caps| caps.unset_headless(),
                    async |session| {
                        // Navigate to https://wikipedia.org.
                        session.goto("https://wikipedia.org").await?;
                        let elem_form = session.find(By::Id("search-form")).await?;

                        // Find element from element.
                        let elem_text = elem_form.find(By::Id("searchInput")).await?;

                        // Type in the search terms.
                        elem_text.send_keys("selenium").await?;

                        // Click the search button.
                        let elem_button = elem_form.find(By::Css("button[type='submit']")).await?;
                        elem_button.click().await?;

                        // Look for header to implicitly wait for the page to load.
                        session.find(By::ClassName("firstHeading")).await?;
                        assert_that(session.title().await?).is_equal_to("Selenium - Wikipedia");

                        Ok(())
                    },
                )
                .await
                .unwrap();
        });
        tests.push(test);
    }

    futures::future::join_all(tests).await;

    let _exit_status = Arc::try_unwrap(chromedriver)
        .expect("no more clones of chromedriver to be alive")
        .terminate()
        .await?;

    Ok(())
}
