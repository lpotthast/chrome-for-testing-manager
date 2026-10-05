# Repository Guidelines

## Project Structure & Module Organization

This Rust 2024 library manages Chrome for Testing installations, cached artifacts, driver processes, and optional
WebDriver sessions. Source lives in `src/`; `src/lib.rs` defines the public API through re-exports. The `src/facade/`
tree owns the high-level `ChromeForTesting` facade and its user-facing configuration. The `src/chromedriver/` tree
owns technical driver configuration, launch, the guarded process, and output subscriptions. The `src/artifact_store/`
and `src/cache/` trees own atomic artifact publication and cache locking respectively. The `src/version/` tree owns
version types and release-manifest resolution, while `src/session/` owns scoped sessions, their builder, and Headless
Shell. The lean `src/manager/` tree composes those domain services behind the lower-level manager facade.
Cross-cutting policies, errors, ports, and the shared guarded-process type (`src/process_support.rs`) remain focused
root modules.

Integration tests live in `tests/`, with reusable browser flows under `tests/common/`. Public documentation belongs in
`README.md`; release notes belong in `CHANGELOG.md`.

## Architecture & Feature Boundaries

Re-export public types from `src/lib.rs` for crate-root imports. Keep session management, capability preparation, and
other `thirtyfour`-dependent APIs behind the existing feature. Preserve cancellation-safe cleanup and transactional
cache behavior when changing downloads or process lifecycles.

## Build, Test, and Development Commands

- `cargo build`: build the library with default features.
- `cargo test --all --all-features`: run the complete test suite.
- `cargo test <test_name> --all-features`: run one named test.
- `cargo fmt --all`: format Rust sources.
- `cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic`: run strict linting.
- `cargo doc --no-deps --all-features`: build API documentation.
- `just verify`: run the full non-mutating validation pipeline.
- `just tidy`: update dependencies, sort manifests, and format files; expect maintained files to change.

## Coding Style & Naming Conventions

Follow `rustfmt` and standard Rust naming: `snake_case` for modules, functions, and tests; `CamelCase` for types such as
`ChromeForTesting` and `VersionRequest`. The crate targets Rust 1.89.0. Treat missing-documentation and Clippy pedantic
warnings as actionable; use narrowly scoped allowances only when justified. Start every module with `//!`
documentation describing its responsibility, boundaries, and important lifecycle or safety invariants.

## Testing Guidelines

Browser integration tests may access the network and spawn real ChromeDriver processes. Use multi-threaded Tokio tests,
descriptive names such as `single_session`, and `assertr` where it matches surrounding code. Put shared setup in
`tests/common/` and initialize tracing opportunistically for diagnostics. Report any skipped network or process tests.

## Commit & Pull Request Guidelines

Use short, imperative commit subjects, following history such as `Fix lints` and `Simplify entry`. Pull requests should
summarize behavior changes, link relevant issues, list verification commands, and explain skipped checks. Include
screenshots only when a user-visible interface changes.

## Security & Configuration Tips

Do not commit downloaded browser binaries, local caches, credentials, or generated build output. Tests should resolve
Chrome for Testing through the crate rather than depend on a locally installed browser.
