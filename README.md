# chrome-for-testing-manager

Drive a real Chrome browser from your Rust tests without ever installing Chrome yourself.

`chrome-for-testing-manager` is a thin orchestration layer over Google's
[Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/) release index. It picks the right
`chrome` + `chromedriver` pair for your platform, downloads them into a local cache the first time you ask, spawns
`chromedriver` on a port of your choosing (or one the OS picks), and hands you a managed
[`thirtyfour`](https://docs.rs/thirtyfour) `WebDriver` session. When your test finishes, panics, or is canceled, the
spawned process is terminated and the session closed for you.

It exists so that browser tests in CI and on developer machines don't depend on whatever Chrome happens to be installed,
and so that bumping the Chrome version under test is one simple change.

## Why use it

- **No global Chrome dependency.** Tests don't care what Chrome happens to be installed on the host. Every developer
  and CI runner runs against the browser this library brings along. No more "works on my machine."
- **Deterministic upgrades.** Pin to a specific `Version`, follow a `Channel` (Stable / Beta / Dev / Canary), or always
  grab the latest. Switching is a one-line change.
- **Port and lifecycle managed for you.** Bind to a fixed port for debugging or let the OS pick one for parallel test
  isolation. The `chromedriver` process is terminated on drop, even on panics, which are very common in tests.
- **Ergonomic `thirtyfour` integration.** Run a test inside `with_session(|session| ...)` and the WebDriver session is
  created, scoped, and torn down automatically. The library needs a session-driver to be useful - `thirtyfour` is the
  default; disable the feature only if you wire in another one.
- **Observable.** Attach a `DriverOutputListener` to stream `chromedriver` stdout/stderr lines into your own logging
  or fixtures.

## Installation

```toml
[dependencies]
chrome-for-testing-manager = "0.10"
rootcause = "0.12"
thirtyfour = "0.37"

# Additional dependencies for the example below.
assertr = "0.6"
tokio = { version = "1", features = ["full"] }
```

## Example

```rust
use assertr::prelude::*;
use chrome_for_testing_manager::Chromedriver;
use rootcause::Report;
use std::time::Duration;
use thirtyfour::prelude::*;

// This library requires being used in a multithreaded runtime.
// If you want to run a test, use: `#[tokio::test(flavor = "multi_thread")]`.
#[tokio::main]
async fn main() -> Result<(), Report> {
    Chromedriver::run_default()
        .await?
        .with_session(async |session| {
            session.goto("https://wikipedia.org").await?;

            let search_form = session.find(By::Id("search-form")).await?;
            let search_input = search_form.find(By::Id("searchInput")).await?;
            search_input.send_keys("selenium").await?;

            let submit_btn = search_form.find(By::Css("button[type='submit']")).await?;
            submit_btn.click().await?;

            // Look for header to implicitly wait for the page to load.
            let _heading = session
                .query(By::Id("firstHeading"))
                .wait(Duration::from_secs(2), Duration::from_millis(100))
                .exists()
                .await?;
            assert_that!(session.title().await?).is_equal_to("Selenium – Wikipedia");

            Ok(())
        }).await
}
```

## Configuration

Anything beyond defaults goes through `ChromedriverRunConfig::builder()`. The `version` setter accepts a `Channel`,
a specific `Version`, or a `VersionRequest`; `port` accepts a `u16`, a `Port`, or a `PortRequest`.

```rust,no_run
use chrome_for_testing_manager::{
    Channel, Chromedriver, ChromedriverRunConfig, DriverOutputListener, TerminationTimeouts,
};
use std::time::Duration;

async fn run() -> Result<(), rootcause::Report<chrome_for_testing_manager::ChromeForTestingManagerError>> {
    let config = ChromedriverRunConfig::builder()
        .version(Channel::Beta)
        .port(3000u16)
        .output_listener(DriverOutputListener::new(|line| println!("{line:?}")))
        .termination_timeouts(
            TerminationTimeouts::builder()
                .interrupt(Duration::from_secs(5))
                .terminate(Duration::from_secs(5))
                .build(),
        )
        .build();
    Chromedriver::run(config).await?;
    Ok(())
}
```

## Managed sessions opt-out

The `with_session` function providing a `thirtyfour` session, called in the example, is only available because
`chrome-for-testing-manager` enables its `thirtyfour` feature by default.

If you only want its chrome/chromedriver version resolution, download, and launch orchestration, declare the dependency
as

```toml
chrome-for-testing-manager = { version = "0.10", default-features = false }
```

instead.

## Going lower-level

For most users `Chromedriver` is the right entry point. If you need finer control, pre-warming the cache without
spawning chromedriver, running multiple chromedriver instances off a single download, pinning a custom cache directory
in CI, or driving sessions through a non-`thirtyfour` WebDriver client, reach for `ChromeForTestingManager` directly.
It exposes `resolve_version`, `download`, `launch_chromedriver`, and `prepare_caps` as separate steps.

## MSRV

- Starting from version `0.8.0`, the minimum supported rust version is `1.89.0`
- Starting from version `0.7.0`, the minimum supported rust version is `1.85.1`
- Starting from version `0.5.0`, the minimum supported rust version is `1.85.0`
- Starting from version `0.1.0`, the minimum supported rust version is `1.81.0`
