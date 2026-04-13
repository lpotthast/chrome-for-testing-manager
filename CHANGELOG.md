# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Add public `ChromeForTestingManagerError` and `ChromeForTestingArtifact` error context types.
- Add `AGENTS.md` guidance.

### Changed

- Upgrade `tokio-process-tools` to 0.8.1.
- Upgrade `assertr` dev dependency to 0.5.0.
- **Breaking:** Upgrade `chrome-for-testing` dependency to 0.4.0.
- Bump MSRV to 1.89.0.
- **Breaking:** Switch public fallible APIs from `anyhow` to typed `rootcause` reports.
- **Breaking:** `VersionRequest` is no longer `Copy` because upstream `Channel` is no longer `Copy`.
- **Breaking:** `with_session` and `with_custom_session` now accept arbitrary user error types that can be converted into
  a `rootcause` report.
- **Breaking:** `Chromedriver::terminate` and `Chromedriver::terminate_with_timeouts` now return typed `rootcause`
  reports.
- Upstream `chrome-for-testing` errors are now preserved as typed `rootcause` reports under manager error contexts.
- Use upstream `Platform` executable path helpers for cached Chrome and ChromeDriver paths.
- `with_session` and `with_custom_session` now return the user closure's output value.
- User session callback errors are attached to `ChromeForTestingManagerError::RunSessionCallback` reports, while user
  callback panics now resume after best-effort session cleanup.
- Update README examples for `rootcause` and version 0.8.

### Removed

- **Breaking:** Remove `SessionError`.

### Fixed

- Terminate a spawned ChromeDriver process if startup detection times out or the output stream closes.

## [0.7.1] - 2026-03-23

### Fixed

- Suppress `dead_code` warning for `chrome_executable` field which is only used behind the `thirtyfour` feature gate.

## [0.7.0] - 2026-03-23

### Added

- `# Errors` and `# Panics` doc sections on all public methods.
- Download stall detection: warns on chunks taking longer than 30 s, aborts after 3 consecutive stalls.
- ZIP bomb guard: validates decompressed archive size against a 2 GB safety limit.
- HTTP response status validation on download requests.
- `#[tracing::instrument]` span on `download_zip` for structured download tracing.
- Justfile for common development tasks.
- CLAUDE.md, LLM instructions for Claude Code.
- CHANGELOG.md.

### Fixed

- Chromedriver stderr inspector was incorrectly attached to stdout.
- Chrome executable path for `MacX64` now correctly uses the `.app` bundle path (was pointing to a non-existent `chrome`
  binary).
- Port parsing from chromedriver output no longer panics on unexpected formats; logs an error instead.

### Changed

- **Breaking:** Upgrade `chrome-for-testing` dependency to 0.3.0.
- **Breaking:** Upgrade `reqwest` dependency to 0.13.
- **Breaking:** Remove `prelude` module; all public types are now re-exported from the crate root.
- **Breaking:** Modules (`chromedriver`, `mgr`, `port`, `session`) are now `pub(crate)`; import types
- **Breaking:** Renamed cache directory from `chromedriver-manager` to `chrome-for-testing-manager` to match the crate
  name. This will lead to cache misses of previously already downloaded chrome/chromedriver versions.
- **Breaking:** `ChromeForTestingManager::new()` now returns `anyhow::Result<Self>` instead of panicking on unsupported
  platforms or cache directory issues. The `Default` impl has been removed.
- Simplify `resolve_version` for `VersionRequest::Latest` to use an iterator chain instead of a manual loop.
- `Session::quit()` returns `Ok(())` instead of `unimplemented!()` when the `thirtyfour` feature is disabled.
- Bump MSRV to 1.85.1.
- Use `DownloadsByPlatform::for_platform()` trait for cleaner download lookups.
- Use `LastKnownGoodVersions::channel()` convenience accessor.
- Use `let...else` for early returns in `download()`.
- `fetch()` calls now pass `&reqwest::Client` (borrowed) instead of cloning.
- `prepare_caps()` is no longer `async` (had no await points).
- Rename `Chromedriver` fields from `chromedriver_process`/`chromedriver_port` to `process`/`port`.
- Replace `zip-extensions` dependency with `zip` v8 (deflate-only); archive is now validated as a proper ZIP before
  extraction.
- Upgrade `tokio-process-tools` to 0.7.2 (new `Process::new().spawn_broadcast()` API).
- Fix all pedantic clippy warnings.
- Cargo.toml keywords for better crate discoverability.

### Removed

- Unused `revision` field from `SelectedVersion`.

## [0.6.0] - 2025-10-02

### Added

- Automatic chromedriver termination via `tokio_process_tools::TerminateOnDrop`.
- `SessionError` to prelude.
- Explicit termination tests.

### Changed

- Moved Wikipedia test logic into shared module.

## [0.5.2] - 2025-06-01

### Fixed

- `single_session` test.
- Show content-length in MB.

### Changed

- Do not panic when chromedriver was not terminated; log `ERROR` instead.
- Updated dependencies.

## [0.5.1] - 2025-06-01

### Fixed

- Clippy lints.
- Type visibility; include `Port`/`PortRequest` types in prelude.

## [0.5.0] - 2025-02-24

### Changed

- **Breaking:** Updated to Rust edition 2024.
- **Breaking:** Bumped MSRV to 1.85.0.
- **Breaking:** Only allow closure-taking `with_session` / `with_custom_session`.
- Updated installation instructions; added missing `terminate` calls in examples.

### Removed

- Session storage (no longer required).

## [0.4.0] - 2025-02-16

### Added

- Session management functionality.
- Handle `VersionRequest::Fixed` variant.

## [0.3.0] - 2025-02-14

### Added

- Force keep-alive of running chromedriver by spawning in wrapper-type.
- `latest_stable` and `latest_stable_with_caps` convenience methods.
- Prelude module.

## [0.2.0] - 2025-02-14 [YANKED]

## [0.1.0] - 2025-01-10

### Added

- Initial release.
- Programmatic chromedriver management with local caching and random port spawning.

[Unreleased]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.7.1...HEAD

[0.7.1]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.7.0...0.7.1

[0.7.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.6.0...0.7.0

[0.6.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.5.2...v0.6.0

[0.5.2]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.5.1...v0.5.2

[0.5.1]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.5.0...v0.5.1

[0.5.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.4.0...v0.5.0

[0.4.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.3.0...v0.4.0

[0.3.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.2.0...v0.3.0

[0.2.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.1.0...v0.2.0

[0.1.0]: https://github.com/lpotthast/chrome-for-testing-manager/releases/tag/v0.1.0
