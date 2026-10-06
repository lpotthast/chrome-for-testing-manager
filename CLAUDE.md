# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rust library (`chrome-for-testing-manager`) for programmatic management of chrome-for-testing installations. Resolves
a Chrome / ChromeDriver version against Google's Chrome for Testing release index, downloads the pair into a per-user
cache with atomic cross-process installation, spawns ChromeDriver on a configurable or OS-assigned port, and
(optionally) provides managed `thirtyfour` WebDriver sessions. Built on the `chrome-for-testing` crate for API
interaction and `tokio-process-tools` for process lifecycle.

## Build & Dev Commands

```bash
cargo build                                                # Build
cargo test --all --all-features                            # Run all tests (unit + integration)
cargo test <test_name> --all-features                      # Run a single test by name
just verify                                                # fmt-check, check, clippy (pedantic, -D warnings), test, doc
just tidy                                                  # Update deps, sort Cargo.toml, format
just readme                                                # Regenerate README body from `src/lib.rs` docs (cargo-rdme)
just install-tools                                         # One-time: nightly + cargo-hack/-minimal-versions/-msrv
just minimal-versions                                      # Verify minimum dependency version bounds
```

Unit tests in `src/` are hermetic: they use local `axum` fixture servers and fake shell-script executables, no network
or real Chrome. Integration tests in `tests/` spawn real ChromeDriver processes and hit the Chrome for Testing API;
the `thirtyfour`-dependent ones are declared with `required-features` in `Cargo.toml`, so they run under default or
`--all-features` builds and are skipped without the feature. They share one cache under `CARGO_TARGET_TMPDIR`
(`target/tmp/integration-test-cache/shared` by default) and may run concurrently; the cache's cross-process file
locks make concurrent installs safe (no `serial_test`).

## Architecture

All public types are re-exported from `lib.rs` (e.g., `chrome_for_testing_manager::ChromeForTesting`).

High-level entry point (`src/facade/`):
- `ChromeForTesting::launch(ChromeForTestingConfig)` is the primary API. It resolves a version, downloads binaries,
  spawns chromedriver, and returns a handle that terminates the process on drop. Call `.shutdown().await` to consume
  the handle and observe the `ExitStatus`; `.driver_port()` and `.browser_executable()` expose the bound port and
  cached browser binary for non-`thirtyfour` clients, `.selected_version()` exposes the resolved release, and
  `.subscribe_output()` streams driver output lines.
- `ChromeForTestingConfig` is a `typed_builder` config covering `version` (accepts `Channel` / `Version` /
  `VersionRequest` via `setter(into)`), `chrome_binary`, `cache_dir`, an optional `cancellation` token, `network` /
  `lifecycle` policies, and the nested `ChromeDriverConfig` (`port` accepts `u16` / `Port` / `PortRequest`). Config
  types are write-only builder inputs without getters.
- `ChromeForTesting::session()` (feature `thirtyfour`) returns a `SessionBuilder` with optional `.with_caps(...)`,
  `.with_config(...)`, and `.with_cancellation(...)` steps and a terminal `.run(async |s| { ... }).await` that opens
  the session, hands it to the user closure, and tears it down via `WebDriver::quit().await` with panic-safe cleanup.
  `session()` takes `&self`, so an `Arc<ChromeForTesting>` can be cloned across a `JoinSet` to run many sessions
  concurrently against one chromedriver (see `tests/multiple_sessions.rs`).

Lower-level orchestration (`ChromeForTestingManager` in `src/manager/`):
- `resolve_version(VersionRequest, BrowserArtifactRequest, CancellationToken)` hits the release index only; `Latest`
  is selected only from releases containing every requested artifact.
- `download(SelectedVersion, ...) -> Vec<LoadedBrowserPackage>` / `download_for(..., ChromeBinary, ...)` install
  atomically and cache-aware: a shared cache lease plus per-artifact exclusive file locks, unique staging dirs,
  completion markers (executable size written at install), atomic rename. Cache hits are validated lock-free through
  marker + executable size only. A successful extraction is the commit point; the transaction publishes even when
  cancellation arrives afterwards.
- `launch_driver(&LoadedBrowserPackage, ChromeDriverConfig, CancellationToken) -> ChromeDriverProcess` spawns the
  process, parses the reported port, probes `/status`, and returns a guarded handle with `subscribe_output()`.
- `prepare_caps(&LoadedBrowserPackage)` (feature `thirtyfour`) builds `ChromeCapabilities` pre-wired with the cached
  Chrome binary path and headless flag.
- The manager owns three `reqwest` clients: one external (manifest default timeout; artifact downloads override
  per-request), one no-proxy local client (driver status + DevTools, fixed 10 s deadline), and (feature `thirtyfour`)
  one no-proxy `WebDriver` client (`NetworkPolicy::webdriver_request_timeout`).

Supporting modules: `artifact_store/` (download, hardened ZIP extraction, installation transactions), `cache/` (typed
shared/exclusive file-lock guards, clear/prune), `chromedriver/` (config, guarded process, output fan-out),
`session/` (builder, Headless Shell launch), `version/` (requests + resolver), `process_support` (`ManagedProcess`:
guarded spawn, output capture, startup, and terminate-then-drain, shared by driver and Headless Shell), `policy.rs`
(`NetworkPolicy`, `LifecyclePolicy`). Dropped futures: installs cancel through a drop guard on their token and roll
back in a task that owns the artifact lock and a cache lease, processes terminate on drop, and a session run hands
cleanup to the runtime from a drop guard (tracked by the manager's `TaskTracker`, which `ChromeForTesting::shutdown`
awaits). Cache contents live beneath a layout-versioned directory (`LAYOUT_DIR` in `cache/`); bump it instead of the
completion-marker schema when the on-disk layout changes.

Cancellation is opt-in and cooperative; the single authoritative description of the drop-safety guarantee lives in
the crate-level docs section "Cancellation and drop safety" in `lib.rs` - link to it instead of restating it in
method docs.

Errors and runtime constraints:
- All fallible APIs return `rootcause::Report<ChromeForTestingError>` (alias `chrome_for_testing_manager::Result`).
  Use `rootcause::prelude::ResultExt` (`.context(...)`) to attach context; do not return bare error enums. Variants
  covering the same failure for different artifacts are unified with a `ChromeForTestingArtifact` field.
- `ChromeForTesting::launch`, `launch_driver`, and Headless Shell session runs assert `RuntimeFlavor::MultiThread`
  and error with `UnsupportedRuntime` otherwise; every other async API errors with `MissingRuntime` outside a Tokio
  runtime instead of panicking. Tests must use `#[tokio::test(flavor = "multi_thread")]`.

Feature gate: `thirtyfour` (default; also enables `futures`). Gated items: `Session`, `ChromeForTesting::session`,
`SessionBuilder`, `ChromeForTestingManager::prepare_caps`.

## Conventions

- Edition: 2024
- MSRV: 1.89.0
- License: MIT OR Apache-2.0
- Clippy pedantic warnings are enforced (`just verify` runs with `-D warnings`)
- Test assertions use the `assertr` crate; HTTP fixtures use `axum` (dev-dependency)
- Tests require a multithreaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`)
