//! Reusable local-page interaction flow for managed `WebDriver` sessions.

use assertr::prelude::*;
use chrome_for_testing_manager::Session;
use rootcause::Report;
use std::time::Duration;
use thirtyfour::prelude::*;

pub async fn exercise_browser_flow(session: &Session) -> Result<(), Report<WebDriverError>> {
    session
        .goto(concat!(
            "data:text/html,",
            "<title>Local browser fixture</title>",
            "<form id='search-form'>",
            "<input id='searchInput'>",
            "<button type='button' onclick=\"document.title='Selenium'\">Search</button>",
            "</form>",
            "<h1 id='firstHeading'>Selenium</h1>"
        ))
        .await?;

    let search_form = session.find(By::Id("search-form")).await?;
    let search_input = search_form.find(By::Id("searchInput")).await?;
    search_input.send_keys("selenium").await?;

    let submit_btn = search_form.find(By::Css("button[type='button']")).await?;
    submit_btn.click().await?;

    let _heading = session
        .query(By::Id("firstHeading"))
        .wait(Duration::from_secs(2), Duration::from_millis(100))
        .exists()
        .await?;

    assert_that!(session.title().await?).is_equal_to("Selenium");

    Ok(())
}
