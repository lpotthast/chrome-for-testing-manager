# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres
to [Semantic Versioning](https://semver.org/).

## [0.13.0] - 2026-10-05

This release replaces the high-level `Chromedriver` API with `ChromeForTesting`, adds opt-in cancellation with
drop-safe cleanup, and makes cache installation an atomic, cross-process transaction. Most public signatures changed;
see "Changed" and "Removed" for migration notes.

### Added

- `ChromeForTesting::launch(ChromeForTestingConfig)` as the new entry point, with `driver_port()`,
  `browser_executable()`, `selected_version()`, `subscribe_output()`, `recent_output()`, `session()`, and an observable
  `shutdown()`.
- `ChromeForTestingConfig` (version, browser binary, cache directory, cancellation, network and lifecycle policies, and
  a nested `ChromeDriverConfig`) and `ChromeForTestingManagerConfig` for the lower-level manager. `cache_dir` setters
  and `ChromeForTestingManager::new_with_cache_dir` accept anything convertible into a `PathBuf`.
- `NetworkPolicy` (connect, manifest, artifact-download, and `WebDriver` request deadlines) and `LifecyclePolicy`
  (graceful shutdown, `ChromeDriver` and Headless Shell startup, and session-cleanup deadlines).
- Opt-in cooperative cancellation through a re-exported `CancellationToken`: optional on `ChromeForTestingConfig` and
  `SessionBuilder::with_cancellation`, and a required argument of the lower-level `ChromeForTestingManager` methods (pass
  `CancellationToken::new()` to never cancel). Cancellation is reported as `ChromeForTestingError::Cancelled` after
  cleanup has finished. Dropped futures are rolled back as well; see the crate-level "Cancellation and drop safety"
  docs.
- `ChromeDriverProcess`, returned by `ChromeForTestingManager::launch_driver`, exposing `port()`, `subscribe_output()`,
  `recent_output()`, and a consuming `terminate()`.
- `subscribe_output()` returns a bounded, non-blocking `DriverOutputSubscription`. A subscriber that falls behind gets a
  recoverable `DriverOutputSubscriptionError::Lagged`; `Closed` is reported once the driver's output ends, including
  when the driver exits on its own.
- `recent_output()` returns the last 256 driver output lines since spawn. Startup errors of `ChromeDriver` and Chrome
  Headless Shell carry their recent output as a report attachment.
- `DriverOutputLine::new` and a `Display` implementation for `DriverOutputLine`.
- `BrowserArtifactRequest` for artifact-aware version resolution, `ChromeForTestingManager::download_for` to install a
  single browser package, `ChromeForTestingManager::prune_cache` with `CachePruneResult`, and
  `ChromeForTestingManager::platform()`.
- `Session::driver()` and `AsRef<WebDriver>` for `Session`.
- Re-exported `Platform` and the new `HttpClientPurpose`.
- Safety limits for downloaded artifacts: archives larger than 2 GiB (`DownloadTooLarge`), with more than 65,536
  entries (`ZipTooManyEntries`), or decompressing to more than 2 GiB (`ZipTooLarge`) are rejected.
- Typed errors for previously panicking or untyped situations, including `MissingRuntime` (no Tokio runtime),
  `CacheInUse`, `ChromeDriverPortMismatch`, `ChromeDriverNotReady`, `ExitedDuringStartup`,
  and `UnrecognizedStartupOutput`.

### Changed

- **Breaking:** Updated `chrome-for-testing` to 0.5.0. Its `Channel`, `Version`, and `Platform` types are re-exported,
  so their changes (including Linux ARM64 support) are part of this crate's API.
- **Breaking:** Renamed `ChromeForTestingManagerError` to `ChromeForTestingError` and restructured its variants:
  - Variants describing the same failure for different artifacts are merged and carry a `ChromeForTestingArtifact`
    (e.g. `SpawnProcess`, `WaitForStartup`, `TerminateProcess`, `NoArtifactDownload`, and the ZIP errors).
  - Every struct variant is `#[non_exhaustive]`; match with `..`.
  - `UnsupportedPlatform` carries the detected `os` and `arch`, and `NoMatchingVersion` the target `platform`.
  - Underlying causes (e.g. from `chrome-for-testing`, `reqwest`, or I/O) are kept as typed report children.
- **Breaking:** `ChromeForTestingManager::launch_chromedriver(loaded, port, inspectors, shutdown)` is replaced by
  `launch_driver(&LoadedBrowserPackage, ChromeDriverConfig, CancellationToken)`, which returns a
  `ChromeDriverProcess` instead of a process, port, and inspector tuple.
- **Breaking:** `resolve_version` takes a `BrowserArtifactRequest` and a `CancellationToken`. `Latest` and channel
  requests only select releases providing `ChromeDriver` and every requested browser package.
- **Breaking:** `download` takes the `SelectedVersion` and a `CancellationToken` and installs the artifact set recorded
  during resolution, instead of taking a separate `&[ChromeBinary]` slice.
- **Breaking:** `LoadedBrowserPackage` is a struct (`chrome_binary()`, `browser_executable()`,
  `chromedriver_executable()`) instead of an enum over `LoadedChromePackage` / `LoadedChromeHeadlessShellPackage`. It
  holds a shared cache lease, so the cache cannot be cleared or pruned while it is in use.
- **Breaking:** `Port` is backed by `NonZeroU16`. `Port::new(0)` now panics; use `Port::try_new` for unchecked values,
  or pass `0u16` where `Into<PortRequest>` is accepted, which now means `PortRequest::Any`.
- Deprecated `SelectedVersion::has_chromedriver_download`: it is always `true` now.
- **Breaking:** `SessionBuilder` no longer has type-state parameters. `with_caps` / `with_config` accept closures
  borrowing from the caller (`Send + 'a`), and repeated calls compose in order instead of replacing earlier ones.
- **Breaking:** Driver output is observed through `subscribe_output()` / `recent_output()`. `DriverOutputLine` is
  `#[non_exhaustive]` and lost its `sequence` field; construct it with `DriverOutputLine::new`.
- Artifact installation is an atomic cross-process transaction: a shared cache lease plus a per-artifact lock, unique
  staging directories, a completion marker recording the executable size, and an atomic rename into place. Cache hits
  are validated through the marker and the executable size, without taking the lock. Packages installed by earlier
  versions are reinstalled once. Interrupted installations and removals never leave a partial package behind.
- ZIP extraction runs on a hardened, cancellable extractor instead of `zip`'s built-in one. Files are written before
  any symlink exists, every symlink is validated by real resolution to stay inside the published package, and
  dangling or over-long symlinks are rejected. Archive permissions lose setuid, setgid, and sticky bits, and owners keep
  write access.
- `ChromeDriver` startup requires the spawned process's own startup line and a ready `/status`, all within the
  configured startup deadline. Fixed ports are checked against the port the driver reports.
- Chrome Headless Shell sessions reject capability options that `ChromeDriver` cannot apply when attaching to a running
  shell, and the shell's whole startup, including its initial page, is bounded by the Headless Shell startup deadline.
- Failed graceful termination escalates to a kill. If even that fails, the error is returned instead of panicking
  when the handle is dropped.
- Managed `WebDriver` sessions connect to `127.0.0.1` through a no-proxy HTTP client, so `HTTP_PROXY` no longer breaks
  them. Its request deadline is `NetworkPolicy::webdriver_request_timeout` (default 120 s);
  `WebDriverBuilder::request_timeout` inside `with_config` has no effect on it. Replace the client through
  `WebDriverBuilder::client` for other HTTP settings.
- Session cleanup quits the session within `LifecyclePolicy::session_cleanup_timeout` and honors
  `SessionBuilder::with_config`. A quit that fails or does not answer is abandoned without `thirtyfour`'s blocking drop
  retry, and a Chrome Headless Shell is terminated regardless.
- A Chrome Headless Shell is terminated gracefully whenever its session run ends, including on failure, cancellation,
  a panic in a `with_config` closure, or a dropped session future.
- `clear_cache` only removes cached version directories and the crate's own leftovers, keeping unrelated files in a
  custom cache directory. `clear_cache` and `prune_cache` also remove the lock files of removed versions. Removals
  rename a directory to a trash entry first, so an interrupted removal never leaves a partial package behind.
- A transient I/O error while validating an installed package no longer causes the package, possibly in use, to be
  replaced, and leftover staging directories that cannot be removed no longer block installation. A file or directory
  standing where a package or its completion marker belongs is repaired by reinstalling the package.
- `download_for` installs only the requested browser and `ChromeDriver`, even if the selected version resolved both
  browsers.
- `futures` is only a dependency with the `thirtyfour` feature.
- Updated `tokio-process-tools` to 0.11.2.

### Removed

- **Breaking:** `Chromedriver`, `ChromedriverRunConfig`, and `Chromedriver::run` / `run_default` / `terminate`. Use
  `ChromeForTesting::launch`, `ChromeForTestingConfig`, and `ChromeForTesting::shutdown`.
- **Breaking:** `LoadedChromePackage` and `LoadedChromeHeadlessShellPackage` (see `LoadedBrowserPackage`).
- **Breaking:** `DriverOutputInspectors` and the `DriverOutputListener` callback API.
- **Breaking:** `From<u16> for Port` and `AsRef<u16> for Port`; use `Port::new` / `Port::try_new` and `Port::as_u16`.
- **Breaking:** Error variants replaced by the merged variants above: `NoChromeDownload`, `NoChromedriverDownload`,
  `NoChromeHeadlessShellDownload`, `SpawnBrowser`, `SpawnChromedriver`, `WaitForBrowserStartup`,
  `WaitForChromedriverStartup`, `TerminateBrowser`, `TerminateChromedriver`, `CreateDownloadFile`,
  `FlushDownloadFile`, `OpenDownloadedZip`, `RemoveDownloadedZip`, `CreatePlatformDir`, `RemoveCacheDir`,
  `RecreateCacheDir`, `DownloadStalled`, `EmptyChromeBinaryDownloadRequest`,
  `InvalidHeadlessShellRemoteDebuggingPortArg`, and `UnsupportedHeadlessShellRemoteDebuggingArg`.

## [0.12.0] - 2026-06-16

### Added

- `ChromeBinary` selection for `ChromedriverRunConfig`, allowing callers to use regular Chrome for Testing or
  `ChromeBinary::ChromeHeadlessShell`.
- `ChromeForTestingManager::download(&selected, &[ChromeBinary::...])` for explicitly requesting one or more binaries.
  The manager downloads the requested binaries and matching `ChromeDriver` concurrently.
- Managed `thirtyfour` sessions for Chrome Headless Shell. The manager starts Chrome Headless Shell separately, creates
  an initial page, attaches ChromeDriver, and cleans up the browser process with the configured `GracefulShutdown`.
- `LoadedBrowserPackage`, `LoadedChromePackage`, and `LoadedChromeHeadlessShellPackage` for explicitly representing
  whether a loaded browser package is regular Chrome or Chrome Headless Shell.
- `LoadedBrowserPackage::chrome_binary()`, `LoadedBrowserPackage::browser_executable()`, and
  `SelectedVersion::has_chrome_headless_shell_download()` accessors.
- `ChromedriverRunConfig` read-only accessors for inspecting builder-produced configs after construction.

### Changed

- **Breaking:** `ChromedriverRunConfig` fields are no longer public. Construct configs through
  `ChromedriverRunConfig::builder()` or `Default` instead of struct literals or direct field mutation.
- **Breaking:** `Port` is now opaque. Use `Port::new(value)` to construct one and `port.as_u16()` to read the raw value
  instead of `Port(value)` or `.0`.
- **Breaking:** `VersionRequest` is now `#[non_exhaustive]`. Downstream exhaustive matches need a wildcard arm.
- **Breaking:** `DefaultCaps` and `DefaultConfig` are no longer re-exported from the crate root. These are internal
  session-builder type-state markers and do not need to be named by callers using the fluent session API.
- **Breaking:** `ChromeForTestingManager::download(...)` now takes a borrowed `SelectedVersion` and a non-empty
  `&[ChromeBinary]` request and returns `Vec<LoadedBrowserPackage>`. Passing an empty slice returns an error.
- **Breaking:** `ChromeForTestingManager::launch_chromedriver(...)` and `prepare_caps(...)` now take
  `LoadedBrowserPackage`, while `LoadedChromePackage` specifically means the regular Chrome package.
- **Breaking:** `ChromeForTestingManagerError::PrepareChromeCapabilities` now stores a `browser_executable` path instead
  of a `chrome_executable` path.
- Invalid Chrome Headless Shell `--remote-debugging-port` arguments are rejected during session setup with a targeted
  error instead of failing later as browser startup timeouts.
- Upgrade `rootcause` dependency to 0.13.0.

## [0.11.0] - 2026-05-11

### Changed

- **Breaking:** Replaced `Chromedriver::with_session` and `Chromedriver::with_custom_session` with a fluent
  `Chromedriver::session()` builder. The returned `SessionBuilder` exposes optional `.with_caps(...)` and
  `.with_config(...)` setup steps and a terminal `.run(...)` that opens the session, hands it to a user closure, and
  tears it down with the same scoped, panic-safe cleanup as before. Each setup step can be called at most once and is
  enforced by the type system. `.with_config(...)` configures the `thirtyfour::WebDriverBuilder` before the session is
  opened.

## [0.10.0] - 2026-05-04

### Added

- `Chromedriver::run_default()` for the common `Chromedriver::run(ChromedriverRunConfig::default())` case.
- `Chromedriver::port()` accessor returning the actual port `chromedriver` is listening on (relevant when
  `PortRequest::Any` was used to let the OS pick a free port).
- `ChromedriverRunConfig` now has a `cache_dir: Option<PathBuf>` field for overriding the cache directory used for
  downloaded `chrome` / `chromedriver` artifacts. Defaults to the platform's user-owned cache directory.
- `ChromedriverRunConfig` now has a `graceful_shutdown: GracefulShutdown` field for configuring the on-drop and
  on-terminate graceful termination budgets, defaulting to a single 3s timeout on all systems.
- Re-export `tokio_process_tools::GracefulShutdown`, `GracefulShutdownBuilder`, `UnixGracefulPhase`,
  `UnixGracefulShutdown`, `UnixGracefulSignal`, and `WindowsGracefulShutdown` from the crate root for constructing the
  `graceful_shutdown` value.
- `ChromeForTestingManager::new_with_cache_dir(PathBuf)` constructor for pinning a custom cache directory.
- `From<u16>` and `From<Port>` impls for `PortRequest`. The builder's `port` field now uses `setter(into)`, so
  `.port(8080u16)` works without `PortRequest::Specific(Port(...))` wrapping.
- `From<Channel>` and `From<Version>` impls for `VersionRequest`, plus named constructors
  `VersionRequest::stable()`, `::beta()`, `::dev()`, and `::canary()`. The builder's `version` field now uses
  `setter(into)`, so `.version(Channel::Stable)` and `.version(some_version)` work without
  `VersionRequest::LatestIn(...)` / `VersionRequest::Fixed(...)` wrapping.
- Public `Result<T>` type alias for `std::result::Result<T, rootcause::Report<ChromeForTestingManagerError>>`.
- Read accessors on `LoadedChromePackage`: `chrome_executable()` and `chromedriver_executable()`.
- Read accessors on `SelectedVersion`: `channel()`, `version()`, `has_chrome_download()`, `has_chromedriver_download()`.

### Changed

- **Breaking:** Upgrade `thirtyfour` dependency to 0.37.0. Downstream code that uses `thirtyfour` types (e.g. via
  `with_custom_session`, the `Session` deref target, or `thirtyfour::prelude`) may need to follow upstreams 0.36 -> 0.37
  migration (see thirtyfour's `MIGRATION.md`).
- **Breaking:** Upgrade `tokio-process-tools` to 0.11.0. The graceful-termination APIs now take a single per-platform
  `GracefulShutdown` argument instead of two separate `Duration`s.
- Upgrade `assertr` dev dependency to 0.6.0.
- Upgrade `ctor` dev dependency to 1.0.0.
- `ChromeForTestingManager::resolve_version`, `download`, `launch_chromedriver`, and `prepare_caps` are now `pub`
  (previously `pub(crate)`). `Chromedriver` remains the recommended entry point though. Reach for the lower-level
  manager when you need to pre-warm the cache, run multiple chromedriver instances off a single download, pin a custom
  cache directory, or drive sessions through a non-`thirtyfour` WebDriver client.
- `ChromeForTestingManager::launch_chromedriver` now takes a `GracefulShutdown` argument. It is applied to the internal
  cleanup that fires when chromedriver fails to report successful startup, so the budget configured on
  `ChromedriverRunConfig::graceful_shutdown` is honored on the startup-failure path as well.
- `DriverOutputInspectors` is now `pub` (previously `pub(crate)`); required when calling `launch_chromedriver`
  directly.
- `LoadedChromePackage` and `SelectedVersion` are now re-exported from the crate root.
- `with_custom_session` setup closure bound relaxed from `Fn` to `FnOnce`, so callers can move owned state into it.
- `Chromedriver::terminate` now honors the configured `graceful_shutdown` budget.
- The internal browser-driver output stream now uses `tokio-process-tools`' `reliable_with_backpressure` policy. A slow
  `DriverOutputListener` will backpressure `chromedriver`'s stdout/stderr. Keep listener callbacks non-blocking.
- README introduction rewritten: new "Why use it" overview, a configuration snippet covering version pinning, fixed
  ports, output listeners and graceful shutdown, and a "Going lower-level" section describing
  `ChromeForTestingManager`. The example now uses `Chromedriver::run_default()`.

### Removed

- **Breaking:** `Chromedriver::terminate_with_timeouts(interrupt, terminate)` has been removed. Configure the shutdown
  via `ChromedriverRunConfig::graceful_shutdown` and call `Chromedriver::terminate()` instead.

## [0.9.1] - 2026-04-14

### Changed

- Preserve the bare `output_listener(DriverOutputListener)` builder setter and add
  `output_listener_opt(Option<DriverOutputListener>)`.
- Move `ChromedriverRunConfig` from the output module to the ChromeDriver module.

## [0.9.0] - 2026-04-14

### Added

- Add typed-builder-based `ChromedriverRunConfig` for configuring ChromeDriver execution with default latest-stable
  `version`, default OS-assigned `port`, and optional `output_listener`.
- Add `DriverOutputListener`, `DriverOutputLine`, and `DriverOutputSource` for observing ChromeDriver stdout and stderr
  lines during a run.

### Changed

- **Breaking:** `Chromedriver::run` now takes a single `ChromedriverRunConfig` instead of separate `VersionRequest` and
  `PortRequest` arguments.
- The `thirtyfour` feature is now a default feature.

### Removed

- **Breaking:** Remove `Chromedriver::run_latest_stable`, `Chromedriver::run_latest_beta`,
  `Chromedriver::run_latest_dev`, and `Chromedriver::run_latest_canary`.

## [0.8.0] - 2026-04-13

### Added

- Add public `ChromeForTestingManagerError` and `ChromeForTestingArtifact` error context types.
- Add `AGENTS.md` guidance.

### Changed

- **Breaking:** Upgrade `chrome-for-testing` dependency to 0.4.0.
- Bump MSRV to 1.89.0.
- **Breaking:** Switch public fallible APIs from `anyhow` to typed `rootcause` reports.
- **Breaking:** `VersionRequest` is no longer `Copy` because upstream `Channel` is no longer `Copy`.
- **Breaking:** `with_session` and `with_custom_session` now accept arbitrary user error types that can be converted
  into a `rootcause` report.
- **Breaking:** `Chromedriver::terminate` and `Chromedriver::terminate_with_timeouts` now return typed `rootcause`
  reports.
- Upstream `chrome-for-testing` errors are now preserved as typed `rootcause` reports under manager error contexts.
- Use upstream `Platform` executable path helpers for cached Chrome and ChromeDriver paths.
- `with_session` and `with_custom_session` now return the user closure's output value.
- User session callback errors are attached to `ChromeForTestingManagerError::RunSessionCallback` reports, while user
  callback panics now resume after best-effort session cleanup.
- Upgrade `tokio-process-tools` to 0.8.1.
- Upgrade `assertr` dev dependency to 0.5.0.
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

[0.13.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.12.0...v0.13.0

[0.12.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.11.0...v0.12.0

[0.11.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.10.0...v0.11.0

[0.10.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.9.1...0.10.0

[0.9.1]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.9.0...0.9.1

[0.9.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.8.0...0.9.0

[0.8.0]: https://github.com/lpotthast/chrome-for-testing-manager/compare/v0.7.1...0.8.0

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
