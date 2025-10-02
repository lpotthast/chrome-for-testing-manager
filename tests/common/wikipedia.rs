use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use std::time::Duration;
use thirtyfour::prelude::*;

pub async fn test_wikipedia(session: &Session) -> Result<(), SessionError> {
    session.goto("wikipedia.org").await?;

    let url = session.current_url().await?;
    assert_that(url).has_display_value("https://www.wikipedia.org/");

    let search_form = session.find(By::Id("search-form")).await?;
    let search_input = search_form.find(By::Id("searchInput")).await?;
    search_input.send_keys("selenium").await?;

    let submit_btn = search_form.find(By::Css("button[type='submit']")).await?;
    submit_btn.click().await?;

    let _heading = session
        .query(By::Id("firstHeading"))
        .wait(Duration::from_secs(2), Duration::from_millis(100))
        .exists()
        .await?;

    assert_that(session.title().await?).is_equal_to("Selenium - Wikipedia");

    Ok(())
}
