# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rust library (`chrome-for-testing-manager`) for programmatic management of chrome-for-testing installations. Resolves
a Chrome / ChromeDriver version against Google's Chrome for Testing release index, downloads the pair into a per-user
cache, spawns ChromeDriver on a configurable or OS-assigned port, and (optionally) provides managed `thirtyfour`
WebDriver sessions. Built on the `chrome-for-testing` crate for API interaction and `tokio-process-tools` for process
lifecycle.

## Build & Dev Commands

```bash
cargo build                                                # Build
cargo test --all --all-features                            # Run all tests (unit + integration)
cargo test <test_name> --all-features                      # Run a single test by name
cargo clippy --all --all-features -- -W clippy::pedantic   # Lint (pedantic)
cargo doc --no-deps --all-features                         # Build docs
just tidy                                                  # Full pipeline: update, sort, fmt, check, clippy, test, doc
just install-tools                                         # One-time: nightly + cargo-hack/-minimal-versions/-msrv
just minimal-versions                                      # Verify minimum dependency version bounds
```

Integration tests in `tests/` spawn real ChromeDriver processes and hit the Chrome for Testing API. They require
`--all-features` to enable the `thirtyfour` feature gate, and run serially via `serial_test` to avoid cache contention.

## Architecture

All public types are re-exported from `lib.rs` (e.g., `chrome_for_testing_manager::Chromedriver`).

High-level entry point:
- `Chromedriver::run(ChromedriverRunConfig)` (and `run_default()`) is the primary API. It resolves a version,
  downloads binaries, spawns chromedriver, and returns a handle that auto-terminates on drop via
  `tokio_process_tools::TerminateOnDrop`.
- `ChromedriverRunConfig` is a `typed_builder` config covering `version`, `port`, optional `output_listener`,
  optional `cache_dir` override, and `termination_timeouts`. The `version` and `port` setters use `setter(into)`,
  so they accept `Channel` / `Version` / `VersionRequest` and `u16` / `Port` / `PortRequest` respectively.
- `Chromedriver::with_session` / `with_custom_session` (feature `thirtyfour`) run an async closure against a
  scoped `Session`, with panic-safe cleanup that always calls `WebDriver::quit`.

Lower-level orchestration (`ChromeForTestingManager`):
- `resolve_version(VersionRequest) -> SelectedVersion`: hits the chrome-for-testing release index only.
- `download(SelectedVersion) -> LoadedChromePackage`: cache-aware; no-op if both binaries already exist on disk.
- `launch_chromedriver(&LoadedChromePackage, PortRequest, Option<DriverOutputListener>)`: spawns the process,
  waits up to 10s for the "started successfully on port" stdout line, parses the bound port (relevant for
  `PortRequest::Any`), and returns the raw `ProcessHandle`, the bound `Port`, and `DriverOutputInspectors`.
- `prepare_caps(&LoadedChromePackage)` (feature `thirtyfour`): builds `ChromeCapabilities` pre-wired with the
  cached Chrome binary path and the headless flag set.

Reach for `ChromeForTestingManager` directly to pre-warm a cache, run multiple chromedriver instances off one
download, drive non-`thirtyfour` WebDriver clients, or pin a custom `cache_dir` (also possible via the run config).

Output observation: pass a `DriverOutputListener` to receive `DriverOutputLine` callbacks tagged by
`DriverOutputSource` (Stdout / Stderr) with a monotonic `sequence` for combined-tail rendering. Inspectors are
owned by `Chromedriver` for the high-level path; the lower-level `launch_chromedriver` returns them so the caller
must keep them alive.

Errors and runtime constraints:
- All fallible APIs return `rootcause::Report<ChromeForTestingManagerError>`. Use `rootcause::prelude::ResultExt`
  (`.context(...)`) to attach context; do not return bare error enums.
- `Chromedriver::run` asserts `RuntimeFlavor::MultiThread` and errors with `UnsupportedRuntime` otherwise. Tests
  must use `#[tokio::test(flavor = "multi_thread")]`.

Feature gate: `thirtyfour` (default). Gated items: `Session`, `Chromedriver::with_session`,
`Chromedriver::with_custom_session`, `ChromeForTestingManager::prepare_caps`.

## Conventions

- Edition: 2024
- MSRV: 1.89.0
- License: MIT OR Apache-2.0
- Clippy pedantic warnings are enforced
- Test assertions use the `assertr` crate
- Tests use `serial_test` for isolation and `ctor` for one-time tracing initialization
- Tests require a multithreaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`)
