# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres
to [Semantic Versioning](https://semver.org/).

## [0.13.0] - 2026-10-06

This release replaces the high-level `Chromedriver` API with `ChromeForTesting`, adds opt-in cancellation with
drop-safe cleanup, makes cache installation an atomic, cross-process transaction, and supports every Tokio runtime
flavor. Most public signatures changed. See "Changed" and "Removed" for migration notes.

### Added

- `ChromeForTesting::launch(ChromeForTestingConfig)` as the new entry point, with `driver_port()`,
  `browser_executable()`, `selected_version()`, `subscribe_output()`, `recent_output()`, `session()`, and an observable
  `shutdown()`. `shutdown()` first waits for the background cleanups of dropped operations and reports their failures,
  then terminates `ChromeDriver`. A termination failure remains primary.
- `ChromeForTestingConfig` (`version`, `chrome_binary`, `cache_dir`, `cancellation`, `network`, `lifecycle`, and a
  nested `ChromeDriverConfig` under `driver`), and `ChromeForTestingManagerConfig` with
  `ChromeForTestingManager::new_with_config` for the lower-level manager. Config types are write-only builders without
  getters.
- `NetworkPolicy` (connect, manifest, artifact-download, and `WebDriver` request deadlines) and `LifecyclePolicy`
  (graceful shutdown, `ChromeDriver` and Headless Shell startup, and session-cleanup deadlines).
- Opt-in cooperative cancellation through a re-exported `CancellationToken` (from `tokio-util` 0.7). It is optional on
  `ChromeForTestingConfig` and `SessionBuilder::with_cancellation`, and a required argument of
  `ChromeForTestingManager::resolve_version`, `download`, `download_for`, and `launch_driver` (pass
  `CancellationToken::new()` to never cancel). Cancellation is reported as `ChromeForTestingError::Cancelled` after
  cleanup has finished. A token cancelled before a download fails it without touching the cache. Dropped futures are
  cleaned up as well: installations roll back, processes are terminated, and sessions are quit. See the crate-level
  "Cancellation and drop safety" docs.
- `ChromeForTestingManager::wait_for_background_tasks()` waits for the cleanups that dropped operations hand to the
  runtime (terminating the processes of dropped handles, quitting the sessions of dropped session runs, rolling back
  dropped installations) and reports their failures as `ChromeForTestingError::BackgroundCleanup`, each failure
  attached.
- `ChromeDriverProcess`, returned by `ChromeForTestingManager::launch_driver`, exposing `port()`, `subscribe_output()`,
  `recent_output()`, and a consuming `terminate()`.
- `subscribe_output()` returns a bounded, non-blocking `DriverOutputSubscription`. A subscriber that falls behind gets a
  recoverable `DriverOutputSubscriptionError::Lagged`. `Closed` is reported once the driver's output ends, including
  when the driver exits on its own.
- `recent_output()` returns up to the last 256 driver output lines since spawn. Startup errors of `ChromeDriver` and
  Chrome Headless Shell (except cancellation) carry their recent output as a report attachment, as do failed
  `WebDriver` session starts against a Chrome Headless Shell.
- `subscribe_output_with_history()` on `ChromeForTesting` and `ChromeDriverProcess` returns the recent output together
  with a subscription, without missing or duplicating a line in between.
- A `log_level` option on `ChromeDriverConfig::builder()`, taking a `ChromeDriverLogLevel` (default `Info`, the level
  previously hardcoded), to configure `ChromeDriver`'s own log verbosity.
- `Port::try_new`. `Port` implements `Hash`, `PartialOrd`, `Ord`, `From<NonZeroU16>`, and `TryFrom<u16>`, `u16`
  implements `From<Port>`, and `PortRequest` implements `Hash`.
- `DriverOutputLine::new` and a `Display` implementation for `DriverOutputLine`.
- `BrowserArtifactRequest` (with `From<ChromeBinary>`) for artifact-aware version resolution,
  `ChromeForTestingManager::download_for` to install one browser package and `ChromeDriver` (even if the selection
  resolved both browsers), `ChromeForTestingManager::prune_cache` with `CachePruneResult`,
  `ChromeForTestingManager::platform()`, and `ChromeForTestingManager::cache_dir()`.
- `ChromeForTestingManager` and `SelectedVersion` implement `Clone`. Clones of a manager share its background tasks.
  `SelectedVersion` exposes `platform()` and `requested_artifacts()`.
- `Display` for `VersionRequest` (e.g. `latest Stable`), `ChromeBinary`, and `BrowserArtifactRequest` (e.g.
  `chrome and chrome-headless-shell`).
- `Session::driver()` and `AsRef<WebDriver>` for `Session`.
- Re-exported `Platform`, and `HttpClientPurpose`, which identifies the failed client in the new `BuildHttpClient`
  error.
- Safety limits for downloaded artifacts: archives larger than 2 GiB (`DownloadTooLarge`) or with more than 65,536
  entries (`ZipTooManyEntries`) are rejected.
- Error variants for failures that previously panicked or that the new features introduce:
  - `MissingRuntime`, returned instead of panicking when `ChromeForTesting::launch`, `SessionBuilder::run`, or a
    `ChromeForTestingManager` operation is called outside a Tokio runtime, and `BuildHttpClient`, previously a panic in
    `reqwest`.
  - Cache: `CacheInUse`, `OpenLockFile`, `AcquireCacheLock`, `ReadCacheDir`, and `RemoveCacheEntry`.
  - Installation: `ValidateInstalledPackage`, `CreateStagingDir`, `RemoveStaleArtifact`,
    `InvalidPackageExecutablePath`, `MissingExtractedExecutable`, `WriteCompletionMarker`, and
    `InstallCompletedPackage`.
  - Version selection: `BrowserArtifactNotResolved` (from `download_for`) and `SelectedVersionPlatformMismatch`.
  - Process startup: `ChromeDriverPortMismatch`, `ChromeDriverNotReady`, `ExitedDuringStartup`,
    `UnrecognizedStartupOutput`, `StartupOutputClosed`, and `ReadStartupOutput`.
  - Sessions: `UnsupportedHeadlessShellCapability` and `QuitSessionTimeout`.

### Changed

- **Breaking:** Updated `chrome-for-testing` to 0.5.0. Its `Channel`, `Version`, and `Platform` types are re-exported,
  so their changes (including Linux ARM64 support) are part of this crate's API.
- **Breaking:** Renamed `ChromeForTestingManagerError` to `ChromeForTestingError` and restructured its variants:
  - Variants describing the same failure for different artifacts are merged into variants carrying a
    `ChromeForTestingArtifact`: `SpawnProcess`, `WaitForStartup`, `TerminateProcess`, and `NoArtifactDownload`. The
    ZIP errors `InvalidZip`, `ZipTooLarge`, and `ExtractZip` carry the artifact as well.
  - Broader variants absorb narrower ones: `WriteDownloadFile` (which gained the file `path`) covers creating and
    flushing the download file, `InvalidZip` covers opening the archive, `CreateCacheDir` covers platform directories,
    `RemoveCacheEntry` replaces `RemoveCacheDir` and `RecreateCacheDir`, and `InvalidHeadlessShellRemoteDebuggingArg`
    replaces both remote-debugging argument variants.
  - Every variant except `Cancelled` and `MissingRuntime` is `#[non_exhaustive]`. Match struct variants with `..`, and
    the unit variants `DetermineCacheDir`, `ConfigureSessionCapabilities`, `RunSessionCallback`, and `QuitSession` as
    `Variant { .. }`.
  - Changed fields: `UnsupportedPlatform` carries the detected `os` and `arch`, `NoMatchingVersion` the target
    `platform` and the `requested_artifacts`, `TerminateProcess` the executable `path` instead of a port or `DevTools`
    address, and `WaitForStartup` the startup `timeout`. `ZipTooLarge`'s `size` and `max_size` are `u64` instead of
    `u128`.
  - Errors from `chrome-for-testing` (manifest requests, platform detection) are kept as typed report children instead
    of stringified attachments.
  - Messages render version requests and artifact sets readably (e.g. `latest Stable`, `chrome and
    chrome-headless-shell`) instead of in their `Debug` form, and `DetermineCacheDir` suggests configuring a cache
    directory instead of asking whether `$HOME` is set.
- **Breaking:** `ChromeForTestingManager::launch_chromedriver(loaded, port, output_listener, shutdown)` is replaced by
  `launch_driver(&LoadedBrowserPackage, ChromeDriverConfig, CancellationToken)`, which returns a `ChromeDriverProcess`
  instead of a `(process, port, inspectors)` tuple. The port moves into `ChromeDriverConfig`, the graceful shutdown into
  the manager's `LifecyclePolicy` (see `ChromeForTestingManagerConfig`), and the output listener is replaced by
  `subscribe_output()`.
- **Breaking:** `resolve_version` takes a `BrowserArtifactRequest` and a `CancellationToken`, and checks downloads for
  the target platform. `Latest` selects the newest release providing `ChromeDriver` and every requested browser
  package. A channel or pinned request whose release lacks one fails with `NoMatchingVersion` during resolution instead
  of later during download.
- **Breaking:** `download` takes `&SelectedVersion` and a `CancellationToken` and installs the artifact set recorded
  during resolution, instead of taking a separate `&[ChromeBinary]` slice. It returns one package per resolved browser,
  Chrome before Chrome Headless Shell.
- **Breaking:** `LoadedBrowserPackage` is a struct instead of an enum over `LoadedChromePackage` /
  `LoadedChromeHeadlessShellPackage`, so it can no longer be matched on. Use its `chrome_binary()`,
  `browser_executable()`, and `chromedriver_executable()` accessors. It holds a shared cache lease, so the cache cannot
  be cleared or pruned while it is in use.
- **Breaking:** `Port` is backed by `NonZeroU16`. `Port::new(0)` now panics. Use `Port::try_new` for unchecked values,
  or pass `0u16` where `Into<PortRequest>` is accepted, which now means `PortRequest::Any`. `PortRequest` is
  `#[non_exhaustive]`.
- Deprecated `SelectedVersion::has_chromedriver_download`: it is always `true` now.
- **Breaking:** `SessionBuilder` no longer has type-state parameters (it is `SessionBuilder<'a>`). `with_caps` /
  `with_config` closures must be `Send` (they may still borrow for `'a`), and repeated calls compose in order.
- **Breaking:** Driver output is observed through `subscribe_output()` / `recent_output()`. `DriverOutputLine` is
  `#[non_exhaustive]` and lost its `sequence` field. Construct it with `DriverOutputLine::new`.
- **Breaking:** Dropping a `ChromeForTesting`, a `ChromeDriverProcess`, or a session run's Chrome Headless Shell no
  longer waits for the process to exit, and never panics. As a result, every Tokio runtime flavor is supported,
  including current-thread runtimes and plain `#[tokio::test]`. Previously, dropping blocked a runtime worker until the
  process was terminated and panicked without a multi-threaded runtime, and `Chromedriver::run` rejected current-thread
  runtimes.
  - The process is terminated gracefully, with its configured `GracefulShutdown`, in a background task on the current
    runtime. To wait for termination and observe its result, call `shutdown()` / `terminate()` instead of dropping, or
    `ChromeForTestingManager::wait_for_background_tasks` for processes launched through a manager you hold.
  - The process keeps its cache lease until it has exited or been killed, so `clear_cache` / `prune_cache` report
    `CacheInUse` while a dropped process is still shutting down.
  - A forceful kill of the process group is only the last resort, used when no runtime is left to drive graceful
    termination (dropped outside a runtime, or the runtime shut down first). It is logged as a warning.
- If graceful termination fails, the process is killed once more. If that fails too, or the killed process does not
  exit within 5 s, the error is returned instead of retrying termination and panicking when the handle is dropped.
- **Breaking:** Managed `WebDriver` sessions connect to `127.0.0.1` instead of `localhost`, through a no-proxy HTTP
  client, so `HTTP_PROXY` no longer breaks them. Its request deadline is `NetworkPolicy::webdriver_request_timeout`
  (default 120 s, matching `thirtyfour`'s own default). `WebDriverBuilder::request_timeout` inside `with_config` no
  longer has an effect. Replace the client through `WebDriverBuilder::client` for other HTTP settings.
- Quitting a session during cleanup is bounded by `LifecyclePolicy::session_cleanup_timeout`. A quit that fails or
  times out is abandoned instead of triggering `thirtyfour`'s blocking quit retry on drop, and a Chrome Headless Shell
  is terminated regardless.
- A Chrome Headless Shell is also terminated gracefully when the session future is dropped while the shell is still
  starting. Previously, it was killed and the drop panicked.
- A session callback that panics before returning its future is caught like any other callback panic: the session is
  quit and the shell terminated before the panic resumes.
- `ChromeDriver` startup requires both its startup line and a ready `/status` response, all within the configurable
  `ChromeDriver` startup deadline (default 10 s). An unparsable startup line fails immediately with
  `UnrecognizedStartupOutput` instead of waiting for the deadline, and a fixed port must match the port the driver
  reports (`ChromeDriverPortMismatch`).
- **Breaking:** Chrome Headless Shell sessions reject every `goog:chromeOptions` entry other than `args`
  (`UnsupportedHeadlessShellCapability`), because `ChromeDriver` cannot apply them when attaching to a running shell.
  The cached-shell `binary` set by `prepare_caps` is removed automatically. The shell's whole startup, including its
  initial page, is bounded by the Headless Shell startup deadline. Browser arguments given without their leading `--`
  (e.g. `user-agent=...`) are normalized as `ChromeDriver` does for regular Chrome, instead of being opened as URLs.
- Artifact installation is an atomic cross-process transaction: a shared cache lease plus a per-artifact lock, unique
  staging directories, a completion marker recording the executable size, and an atomic rename into place. Cache hits
  are validated through the marker and the executable size, without taking the lock. Interrupted installations and
  removals never leave a partial package behind. An I/O error while validating an installed package is reported
  instead of replacing the package, which may be in use, and a file or directory standing where a package or its
  completion marker belongs is repaired by reinstalling the package. A dropped installation future rolls back in the
  background and keeps its locks until its file-system work has stopped. A failed rollback is reported by
  `ChromeForTesting::shutdown` / `wait_for_background_tasks`. The downloaded archive is deleted right after extraction.
- **Breaking:** Cached artifacts live in a layout-versioned directory (`v1`) beneath the cache root, so releases with
  different on-disk layouts never replace each other's packages, even while in use. Version directories that earlier
  releases stored directly in the cache root are neither reused nor removed by `clear_cache` / `prune_cache`. Delete
  them manually once no older release uses them.
- ZIP extraction runs on a hardened, cancellable extractor instead of `zip`'s built-in one. Files are written before
  any symlink exists, every symlink is validated by real resolution to stay inside the published package, and
  dangling or over-long symlinks are rejected. Archive permissions lose setuid, setgid, and sticky bits as well as group
  and other write access, and owners are always granted read and write access (and execute access on directories).
  The 2 GiB decompressed-size limit (`ZipTooLarge`) is enforced on the bytes actually extracted instead of the size the
  archive declares.
- Artifact downloads are bounded by `NetworkPolicy::artifact_download_timeout` (default 15 min) instead of being
  aborted after three consecutive 30 s stalls. Release-manifest requests and connections time out after 30 s each
  (previously unbounded).
- `clear_cache` removes only cached version directories and leftovers of interrupted removals, instead of deleting and
  recreating the whole cache directory, so unrelated files in a custom cache directory are kept. On Windows, cache
  renames and removals are retried briefly while a virus scanner or indexer holds a file open.
- Driver output lines no longer keep the `\r` of a `\r\n` line terminator.
- `futures` is only a dependency with the `thirtyfour` feature.
- The crate-level documentation contains the full README content. The README is generated from it with `cargo-rdme`.
- Updated `tokio-process-tools` to 0.11.3.

### Fixed

- `PortRequest::Any` (the default) lets the OS assign a free port, as documented. Previously no `--port` was passed,
  so `ChromeDriver` bound its default port 9515, and concurrent environments collided.
- `launch_driver` supports non-Unicode executable paths, and `prepare_caps` returns `PrepareChromeCapabilities` for
  them, instead of panicking.
- On an unsupported platform, creating a manager or launching fails with `UnsupportedPlatform` before creating the
  cache directory.

### Removed

- **Breaking:** `Chromedriver`, `ChromedriverRunConfig`, and `Chromedriver::run` / `run_default` / `terminate`. Use
  `ChromeForTesting::launch`, `ChromeForTestingConfig`, and `ChromeForTesting::shutdown`. The config's `port` moved to
  `ChromeDriverConfig` (`.driver(...)`), `graceful_shutdown` to `LifecyclePolicy` (`.lifecycle(...)`), and
  `output_listener` is replaced by `subscribe_output()`.
- **Breaking:** `LoadedChromePackage` and `LoadedChromeHeadlessShellPackage` (see `LoadedBrowserPackage`).
- **Breaking:** `DriverOutputInspectors` and the `DriverOutputListener` callback API. Use `subscribe_output()` /
  `recent_output()`.
- **Breaking:** `From<u16> for Port` and `AsRef<u16> for Port`. Use `Port::new` / `Port::try_new` and `Port::as_u16`.
- **Breaking:** The `UnsupportedRuntime` error variant: current-thread runtimes are supported now.
- **Breaking:** Error variants that were merged or absorbed (see "Changed"): `NoChromeDownload`,
  `NoChromedriverDownload`, `NoChromeHeadlessShellDownload`, `SpawnBrowser`, `SpawnChromedriver`,
  `WaitForBrowserStartup`, `WaitForChromedriverStartup`, `TerminateBrowser`, `TerminateChromedriver`,
  `CreateDownloadFile`, `FlushDownloadFile`, `OpenDownloadedZip`, `CreatePlatformDir`, `RemoveCacheDir`,
  `RecreateCacheDir`, `InvalidHeadlessShellRemoteDebuggingPortArg`, and `UnsupportedHeadlessShellRemoteDebuggingArg`.
- **Breaking:** Error variants whose failure can no longer occur: `RemoveDownloadedZip` (deleting the extracted archive
  is best effort), `EmptyChromeBinaryDownloadRequest` (a `BrowserArtifactRequest` cannot be empty), and
  `DownloadStalled` (see the download deadline above).

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
