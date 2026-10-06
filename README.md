# chrome-for-testing-manager

[![Crates.io](https://img.shields.io/crates/v/chrome-for-testing-manager.svg)](https://crates.io/crates/chrome-for-testing-manager)
[![Docs.rs](https://docs.rs/chrome-for-testing-manager/badge.svg)](https://docs.rs/chrome-for-testing-manager)
[![CI](https://github.com/lpotthast/chrome-for-testing-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/lpotthast/chrome-for-testing-manager/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.89.0-blue.svg)](https://github.com/lpotthast/chrome-for-testing-manager/blob/main/Cargo.toml)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/chrome-for-testing-manager.svg)](#license)

<!-- cargo-rdme start -->

Drive a real Chrome browser from your Rust tests without ever installing Chrome yourself.

`chrome-for-testing-manager` is a thin orchestration layer over Google's
[Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/) release index. It picks the right
`chrome` + `chromedriver` pair for your platform, downloads them into a local cache the first time you ask, spawns
`chromedriver` on a port of your choosing (or one the OS picks), and hands you a managed
[`thirtyfour`](https://docs.rs/thirtyfour) `WebDriver` session. When your test finishes, panics, or is cancelled,
the session is closed and the spawned processes are terminated for you.

It exists so that browser tests in CI and on developer machines don't depend on whatever Chrome happens to be installed,
and so that bumping the Chrome version under test is one simple change.

## Why use it

- **No global Chrome dependency.** Tests don't care what Chrome happens to be installed on the host. Every developer
  and CI runner runs against the browser this library brings along. No more "works on my machine."
- **Deterministic upgrades.** Pin to a specific `Version`, follow a `Channel` (Stable / Beta / Dev / Canary), or always
  grab the latest. Switching is a one-line change.
- **Port and lifecycle managed for you.** Bind to a fixed port for debugging or let the OS pick one for parallel test
  isolation. A dropped handle terminates its process gracefully in the background of the Tokio runtime. Call
  `shutdown().await` when termination and its result must be observed.
- **Ergonomic `thirtyfour` integration.** Run a browser test inside `session().run(|s| ...)` where the `WebDriver`
  session is created, scoped, and torn down automatically. Optional `.with_caps(...)`, `.with_config(...)`, and
  `.with_cancellation(...)` builder steps let you tweak Chrome capabilities, the `WebDriver` client, or cancellation
  without leaving the chain. The `thirtyfour` feature is enabled by default. Disabling it removes the session APIs
  and keeps version resolution, caching, downloads, and process management.
- **Observable.** Call `subscribe_output()` for a bounded, non-blocking subscription streaming `chromedriver`
  stdout/stderr lines into your own logging or fixtures, `recent_output()` for the last lines printed since spawn, or
  `subscribe_output_with_history()` for both without missing or duplicating a line. Startup failures carry that recent
  output in their error report.

## Installation

```toml
[dependencies]
chrome-for-testing-manager = "0.13"
rootcause = "0.13"
thirtyfour = "0.37"

# Additional dependencies for the example below.
assertr = "0.7"
tokio = { version = "1", features = ["full"] }
```

## Example

```rust
use assertr::prelude::*;
use chrome_for_testing_manager::ChromeForTesting;
use rootcause::Report;
use std::time::Duration;
use thirtyfour::prelude::*;

// Any Tokio runtime works, e.g. `#[tokio::test]` in tests.
#[tokio::main]
async fn main() -> Result<(), Report> {
    let chrome = ChromeForTesting::launch(Default::default()).await?;
    let session_result = chrome
        .session()
        .run(async |session| {
            session.goto(concat!(
                "data:text/html,",
                "<title>Local fixture</title>",
                "<form id='search-form'><input id='searchInput'>",
                "<button type='button' onclick=\"document.title='Selenium'\">Search</button></form>",
                "<h1 id='firstHeading'>Selenium</h1>"
            )).await?;

            let search_form = session.find(By::Id("search-form")).await?;
            let search_input = search_form.find(By::Id("searchInput")).await?;
            search_input.send_keys("selenium").await?;

            let submit_btn = search_form.find(By::Css("button[type='button']")).await?;
            submit_btn.click().await?;

            // Look for header to implicitly wait for the page to load.
            let _heading = session
                .query(By::Id("firstHeading"))
                .wait(Duration::from_secs(2), Duration::from_millis(100))
                .exists()
                .await?;
            assert_that!(session.title().await?).is_equal_to("Selenium");

            Ok::<(), WebDriverError>(())
        }).await;
    let shutdown_result = chrome.shutdown().await;

    session_result?;
    shutdown_result?;
    Ok(())
}
```

Calling `shutdown()` instead of just dropping `chrome` waits for `chromedriver` to exit and reports any failure.
Cancellation is opt-in. See [Cancellation and drop safety](#cancellation-and-drop-safety) below.

## Configuration

Anything beyond defaults goes through `ChromeForTestingConfig::builder()`. The `version` setter accepts a `Channel`,
a specific `Version`, or a `VersionRequest`, and defaults to the latest Stable release. HTTP deadlines belong to
`NetworkPolicy`, and process and session timing (startup deadlines, graceful shutdown, session cleanup) to
`LifecyclePolicy`. Driver settings are grouped in `ChromeDriverConfig`, whose `port` setter accepts a `u16`, a
`Port`, or a `PortRequest`. The port defaults to an OS-assigned one, which `0u16` requests explicitly as well.

```rust
use chrome_for_testing_manager::{
    Channel, ChromeDriverConfig, ChromeForTesting, ChromeForTestingConfig,
    DriverOutputSubscriptionError, GracefulShutdown, LifecyclePolicy, NetworkPolicy,
};
use std::time::Duration;

async fn run() -> Result<(), rootcause::Report<chrome_for_testing_manager::ChromeForTestingError>> {
    let config = ChromeForTestingConfig::builder()
        .version(Channel::Beta)
        .cache_dir("target/chrome-cache")
        .network(
            NetworkPolicy::builder()
                .artifact_download_timeout(Duration::from_secs(10 * 60))
                .build(),
        )
        .lifecycle(
            LifecyclePolicy::builder()
                .graceful_shutdown(
                    GracefulShutdown::builder()
                        .unix_sigterm(Duration::from_secs(5))
                        .windows_ctrl_break(Duration::from_secs(5))
                        .build(),
                )
                .build(),
        )
        .driver(ChromeDriverConfig::builder().port(3000u16).build())
        .build();
    let chrome = ChromeForTesting::launch(config).await?;

    let mut driver_output = chrome.subscribe_output();
    tokio::spawn(async move {
        loop {
            match driver_output.recv().await {
                Ok(line) => println!("{line}"),
                // This subscriber fell behind and missed some lines. It can keep receiving.
                Err(DriverOutputSubscriptionError::Lagged { skipped }) => {
                    eprintln!("missed {skipped} chromedriver output lines");
                }
                // The driver's output has ended.
                Err(_) => break,
            }
        }
    });

    chrome.shutdown().await?;
    Ok(())
}
```

`GracefulShutdown` (re-exported from `tokio-process-tools`) mirrors the platform's actual graceful-shutdown model:

- on Unix, it carries one or more phases (`SIGTERM` and/or `SIGINT`).
- on Windows, it carries a single `CTRL_BREAK_EVENT` budget.

## Managed sessions opt-out

The `session()` builder used in the example requires the `thirtyfour` feature, which is enabled by default. If you
only need version resolution, downloads, and process management, for example to drive the browser with another
`WebDriver` client, disable the default features:

```toml
chrome-for-testing-manager = { version = "0.13", default-features = false }
```

## Going lower-level

For most users `ChromeForTesting` is the right entry point. Other `WebDriver` clients can connect through
`driver_port()`. With regular Chrome, they register `browser_executable()` as the browser binary in their session
capabilities. With Chrome Headless Shell, they launch `browser_executable()` with `--remote-debugging-port` themselves
and attach through `goog:chromeOptions.debuggerAddress`, as the managed `thirtyfour` sessions do.

For finer control, such as pre-warming the cache without spawning `chromedriver` or running several `chromedriver`
instances off a single download, use `ChromeForTestingManager` directly. It exposes the steps separately:

- `resolve_version` picks a release providing `ChromeDriver` and every browser package in the given
  `BrowserArtifactRequest`.
- `download` installs exactly the resolved set and returns a `LoadedBrowserPackage` per browser, while
  `download_for` installs a single browser package. A `LoadedBrowserPackage` holds the browser and `ChromeDriver`
  paths and keeps the cache from being cleared while it exists.
- `launch_driver` starts `ChromeDriver` with a `ChromeDriverConfig` and returns a `ChromeDriverProcess`, with
  `port()`, the same output observation methods as `ChromeForTesting`, and a consuming `terminate()`.
- `prepare_caps` (feature `thirtyfour`) builds headless capabilities pointing at the cached browser.
- `wait_for_background_tasks` waits for the cleanups of dropped processes, session runs, and installations.

Installations are coordinated across processes through a shared cache lock and a per-artifact exclusive lock. Each
artifact is downloaded and validated in a unique staging directory, marked with the executable size, and atomically
renamed into place. Loaded packages and running processes hold a shared cache lease, so `clear_cache()` and
`prune_cache(...)` return `CacheInUse` instead of deleting artifacts in use. Both remove only version directories
and their lock files, keeping unrelated files in a custom cache directory.

The cache contents live in a layout-versioned directory beneath the cache root, so releases with incompatible on-disk
layouts can share one cache root without replacing each other's packages. Releases before 0.13 stored versions
directly in the cache root. Neither `clear_cache()` nor `prune_cache(...)` touches those, so delete them manually once
no older release uses them.

## Cancellation and drop safety

Cancellation is opt-in. Pass a `CancellationToken` through `ChromeForTestingConfig::builder`,
the session builder's `with_cancellation` step (feature `thirtyfour`), or the lower-level
`ChromeForTestingManager` methods to cancel work cooperatively. Cancelled work is rolled back
before `ChromeForTestingError::Cancelled` is returned: an in-flight installation stops and
removes its staging directory, a starting process is terminated, and a `WebDriver` handshake in
flight is completed and the new session closed.

Dropping a future instead of cancelling it is handled as well, but less observably:

- An installation is cancelled and rolls back in the background: its extraction stops at the next
  chunk, and its staging directory is removed before its cache locks are released.
- A process that is starting up, and every managed process when its handle is dropped, is
  terminated gracefully in the background, with its configured shutdown policy. Its cache lease
  is held until it has exited or was killed.
- A `WebDriver` session run hands its cleanup (quitting the session, terminating a Chrome Headless
  Shell) to the Tokio runtime. A session whose handshake was cut off cannot be closed.
  `ChromeDriver` ends it when it terminates.

`ChromeForTesting::shutdown` (or `ChromeForTestingManager::wait_for_background_tasks`) waits
for these background cleanups and reports their failures, which are logged as well. None of them
survives the Tokio runtime shutting down or the process being killed. A process whose graceful
termination can no longer run, because no runtime is left to drive it, is killed as a last
resort. Prefer explicit cancellation followed by awaiting the operation, and call
`ChromeForTesting::shutdown` for observable graceful shutdown.

<!-- cargo-rdme end -->

## MSRV

- Starting from version `0.8.0`, the minimum supported rust version is `1.89.0`
- Starting from version `0.7.0`, the minimum supported rust version is `1.85.1`
- Starting from version `0.5.0`, the minimum supported rust version is `1.85.0`
- Starting from version `0.1.0`, the minimum supported rust version is `1.81.0`

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
