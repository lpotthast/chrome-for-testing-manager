use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use thirtyfour::prelude::*;

#[tokio::test]
async fn single_session() -> anyhow::Result<()> {
    tracing_subscriber::fmt().try_init().ok();

    let chromedriver = Chromedriver::run_latest_stable().await?;

    chromedriver
        .with_session(async |session| {
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
            let _heading = session.find(By::Id("firstHeading")).await.unwrap();

            assert_that(session.title().await.unwrap())
                .is_equal_to("Selenium – Wikipedia");

            Ok(())
        })
        .await?;

    chromedriver.terminate().await?;

    Ok(())
}
