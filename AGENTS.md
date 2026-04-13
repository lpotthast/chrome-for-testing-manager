# Repository Guidelines

## Project Structure & Module Organization

This is a Rust 2024 library crate for programmatic management of Chrome for Testing and ChromeDriver installations. It
downloads `chrome` and `chromedriver` binaries into a local cache, spawns ChromeDriver on configurable or OS-assigned
ports, and provides optional session management through `thirtyfour`. It is built on the `chrome-for-testing` crate for
Chrome for Testing API interaction.

Source lives in `src/`; `src/lib.rs` defines the public surface by re-exporting types from internal modules. Key modules
include `chromedriver.rs` for the high-level entry point, `mgr.rs` for version resolution/download/launch orchestration,
`session.rs` for `thirtyfour` session helpers, and `cache.rs`, `download.rs`, and `port.rs` for supporting behavior.

Integration tests live in `tests/`, with shared helpers under `tests/common/`. There is no asset tree. Public
documentation belongs in `README.md`; release notes belong in `CHANGELOG.md`.

## Architecture Notes

All public types should be re-exported from `src/lib.rs` so users can import them from the crate root, for example
`chrome_for_testing_manager::Chromedriver`. `Chromedriver` is the main entry point for resolving a version, downloading
binaries, and spawning ChromeDriver. `ChromeForTestingManager` contains the lower-level resolution, download, and launch
operations. `Session` wraps `thirtyfour::WebDriver` when the `thirtyfour` feature is enabled and supports cleanup-oriented
session helpers. `VersionRequest` selects latest, channel-specific, or fixed versions; `PortRequest` selects OS-assigned
or specific ports.

Keep `thirtyfour`-dependent APIs, including session management and capability preparation, behind the existing
`thirtyfour` feature gate.

## Build, Test, and Development Commands

- `cargo build`: build the crate.
- `cargo test --all --all-features`: run tests with the optional `thirtyfour` feature enabled.
- `cargo test <test_name> --all-features`: run one named test.
- `cargo fmt`: format Rust code.
- `cargo clippy --all --all-features -- -W clippy::pedantic`: lint with pedantic warnings.
- `cargo doc --no-deps --all-features`: build crate docs.
- `just tidy`: run the full maintenance pipeline: update, sort, format, check, clippy, tests, and docs.
- `just install-tools`: install helper tools used by the `Justfile`.
- `just minimal-versions`: verify that direct minimum dependency version bounds are sufficient.

## Coding Style & Naming Conventions

Use standard `rustfmt` formatting and Rust naming conventions: `snake_case` for modules, functions, and tests;
`CamelCase` for public types such as `Chromedriver`, `VersionRequest`, and `PortRequest`. Keep user-facing APIs
re-exported through `src/lib.rs`. Prefer focused modules that match existing responsibilities.

The crate targets Rust `1.89.0` and edition `2024`, and is licensed as `MIT OR Apache-2.0`. Treat Clippy pedantic
warnings as actionable unless there is a clear reason to allow one locally.

## Testing Guidelines

Integration tests spawn real ChromeDriver processes and may hit the Chrome for Testing API, so they require
network/process support and `--all-features`. Use `#[tokio::test(flavor = "multi_thread")]`; the library expects a
multi-threaded Tokio runtime. Shared browser-flow logic should go in `tests/common/`. Follow the existing descriptive
test naming pattern, for example `single_session` or `custom_termination_with_timeouts`.

Use `assertr` for assertions where it matches the surrounding tests. Tests use `serial_test` for process/cache isolation
and `ctor` for one-time tracing initialization.

## Commit & Pull Request Guidelines

Recent commit history uses short, imperative summaries such as `Fix dead code warning`, `Update readme`, and
`Prepare v0.7.0`; follow that style. For pull requests, include a concise description, relevant issue links, and the
commands you ran. Note any skipped browser/integration tests and why.

## Security & Configuration Tips

Do not commit downloaded Chrome/ChromeDriver binaries, local caches, or generated build output. Keep feature-gated
`thirtyfour` behavior behind the existing Cargo feature boundary, and avoid adding tests that depend on a locally
installed Chrome when Chrome for Testing should be resolved by the crate.
