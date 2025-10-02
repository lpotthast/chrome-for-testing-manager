# chrome-for-testing-manager

Programmatic management of **chrome-for-testing** installations.

- Automatically resolves the requested version. `Chromedriver::run_latest_stable` is a shortcut for
  `Chromedriver::run(VersionRequest::LatestIn(Channel::Stable), PortRequest::Any)`.
- Automatically downloads chrome-for-testing `chrome` and `chromedriver` binaries into a local cache directory.
- Possibility to spawn the chromedriver process using a random port.
- Built-int session management.

Frees you from the need to

- manually download a chromedriver package matching your locally installed chrome,
- starting and stopping it manually,
- hardcoding the chosen chromedriver port into your tests and
- doing this all-over when trying to test with a new version of chrome.

## Installation

```toml
[dependencies]
thirtyfour = "0.35"
chrome-for-testing-manager = { version = "0.6", features = ["thirtyfour"] }

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

// This library requires being used in a multithreaded runtime.
// If you want to run a test, use: `#[tokio::test(flavor = "multi_thread")]`.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Chromedriver::run_latest_stable()
        .await?
        .with_session(async |session| {
            session.goto("https://wikipedia.org").await?;
          
            let search_form = session.find(By::Id("search-form")).await?;
            let search_input = search_form.find(By::Id("searchInput")).await?;
            search_input.send_keys("selenium").await?;

            let submit_btn = elem_form.find(By::Css("button[type='submit']")).await?;
            submit_btn.click().await?;

            // Look for header to implicitly wait for the page to load.
            let _heading = session
                .query(By::Id("firstHeading"))
                .wait(Duration::from_secs(2), Duration::from_millis(100))
                .exists()
                .await?;
            assert_that(session.title().await?).is_equal_to("Selenium - Wikipedia");

            Ok(())
        }).await
}
```

## MSRV

- Starting from version `0.5.0`, the minimum supported rust version is `1.85.0`
- Starting from version `0.1.0`, the minimum supported rust version is `1.81.0`
