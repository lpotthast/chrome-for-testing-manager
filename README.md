# chrome-for-testing-manager

Programmatic management of **chrome-for-testing** installations.

## Example (`thirtyfour` feature enabled)

```rust
use crate::{ChromeForTestingManager, Port, PortRequest, VersionRequest};
use chrome_for_testing::api::channel::Channel;
use thirtyfour::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let mgr = ChromeForTestingManager::new();
    let selected = mgr.resolve_version(VersionRequest::LatestIn(Channel::Stable)).await?;
    let loaded = mgr.download(selected).await?;
    let (_chromedriver, port) = mgr.launch_chromedriver(&loaded, PortRequest::Any).await?;

    let caps = mgr.prepare_caps(&loaded).await?;
    let driver = thirtyfour::WebDriver::new(format!("http://localhost:{port}"), caps).await?;
    driver.goto("https://www.google.com").await?;

    let url = driver.current_url().await?;
    assert_that(url).has_display_value("https://www.google.com/");

    Ok(())
}
```
