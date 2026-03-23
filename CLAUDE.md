# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rust library (`chrome-for-testing-manager`) for programmatic management of chrome-for-testing installations. Downloads Chrome and ChromeDriver binaries, manages a local cache, spawns ChromeDriver on configurable or random ports, and provides session management via the `thirtyfour` WebDriver library. Built on the `chrome-for-testing` crate for API interaction.

## Build & Dev Commands

```bash
cargo build                                        # Build
cargo test --all --all-features                    # Run all tests (unit + integration)
cargo test <test_name> --all-features              # Run a single test by name
cargo clippy --all --all-features -- -W clippy::pedantic  # Lint (pedantic)
cargo doc --no-deps --all-features                 # Build docs
just tidy                                          # Full pipeline: update deps, sort, fmt, check, clippy, test, doc
just install-tools                                 # One-time: install nightly + cargo-hack, cargo-minimal-versions, cargo-msrv
just minimal-versions                              # Verify minimum dependency version bounds
```

Integration tests (`tests/`) spawn real ChromeDriver processes and hit the Chrome for Testing API. They require `--all-features` to enable the `thirtyfour` feature gate.

## Architecture

All public types are re-exported from `lib.rs` — users import from the crate root (e.g., `chrome_for_testing_manager::Chromedriver`).

Key types:
- `Chromedriver`: Main entry point. Resolves a version, downloads binaries, spawns chromedriver. Automatically terminates the process on drop via `TerminateOnDrop`.
- `ChromeForTestingManager`: Lower-level manager handling version resolution (`resolve_version`), binary downloading (`download`), and process launching (`launch_chromedriver`).
- `Session`: Wraps `thirtyfour::WebDriver` with `Deref`. Provides automatic cleanup and panic-safe session handling via `with_session` / `with_custom_session` closures.
- `VersionRequest`: Enum for version selection — `Latest`, `LatestIn(Channel)`, or `Fixed(Version)`.
- `Port` / `PortRequest`: Port configuration — `Any` for OS-assigned or `Specific(Port)`.

The `thirtyfour` feature gate controls session management (`Session`, `SessionError`, `with_session`, `with_custom_session`, `prepare_caps`).

## Conventions

- Edition: 2024
- MSRV: 1.85.1
- License: MIT OR Apache-2.0
- Clippy pedantic warnings are enforced
- Test assertions use the `assertr` crate
- Tests use `serial_test` for isolation and `ctor` for one-time tracing initialization
- Tests require a multithreaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`)
