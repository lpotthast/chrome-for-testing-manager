use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use thirtyfour::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut chromedriver = Chromedriver::run_latest_stable().await?;
    let (handle, session) = chromedriver.new_session().await?;

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

    // Always explicitly close the session/browser.
    chromedriver.quit(handle).await?;

    Ok(())
}
