# chrome-for-testing-manager

Programmatic management of **chrome-for-testing** installations.

- Automatically resolves the requested version. `Chromedriver::run_latest_stable` is a shortcut for
  `Chromedriver::run(VersionRequest::LatestIn(Channel::Stable), PortRequest::Any)`.
- Automatically downloads chrome-for-testing `chrome` and `chromedriver` binaries into a local cache directory.
- Possibility to spawn the chromedriver process using a random port.
- Built-int session management.

Frees you from the need to
- manually download a chromedriver package matching your locally installed chrome,
- starting it manually,
- hardcoding the chosen chromedriver port into your tests and
- doing this all-over when trying to test with a new version of chrome.

## Installation

```toml
[dependencies]
thirtyfour = "0.35"
chrome-for-testing-manager = { version = "0.4", features = ["thirtyfour"] }

# Additional dependencies for the example below.
assertr = "0.1"
anyhow = "1"
tokio = { version = "1", features = ["full"] }
```

## Example

```rust
use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use thirtyfour::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut chromedriver = Chromedriver::run_latest_stable().await?;

    chromedriver.with_session(async |session| {
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
    }).await?;

    chromedriver.terminate().await?;
  
    Ok(())
}
```

## MSRV

- Starting from version `0.5.0`, the minimum supported rust version is `1.85.0`
- Starting from version `0.1.0`, the minimum supported rust version is `1.81.0`
