# chrome-for-testing-manager

Programmatic management of **chrome-for-testing** installations.

## Example (`thirtyfour` feature enabled)

```rust
use assertr::prelude::*;
use chrome_for_testing_manager::prelude::*;
use thirtyfour::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let chromedriver = ChromeForTestingManager::latest_stable().await?;
    let driver = chromedriver.new_webdriver().await?;
    
    driver.goto("https://www.google.com").await?;

    let url = driver.current_url().await?;
    assert_that(url).has_display_value("https://www.google.com/");

    driver.quit().await?;
    
    Ok(())
}
```
