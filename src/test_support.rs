//! Crate-internal test fixtures for isolated directories, local HTTP behavior, and artifact ZIPs.

use ::chrome_for_testing::Platform;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDirectory(PathBuf);

impl TestDirectory {
    pub(crate) fn new(name: &str) -> std::io::Result<Self> {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()?
            .join("target")
            .join("unit-test-cache")
            .join(format!("{name}-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            // A second panic while a failed test unwinds would abort and hide the real failure.
            if std::thread::panicking() {
                eprintln!(
                    "failed to remove test directory {}: {error}",
                    self.0.display()
                );
            } else {
                panic!(
                    "failed to remove test directory {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum ResponseSpec {
    Body(Bytes),
    /// Send response headers plus one body byte, then stall the body until server shutdown.
    Stall,
    /// Delay the whole response, headers included.
    /// Constructed only by feature-gated session tests.
    #[cfg_attr(not(feature = "thirtyfour"), expect(dead_code))]
    Delay(Duration, Bytes),
}

impl ResponseSpec {
    pub(crate) fn body(body: impl Into<Bytes>) -> Self {
        Self::Body(body.into())
    }
}

struct FixtureState {
    routes: HashMap<String, ResponseSpec>,
    counters: HashMap<String, AtomicUsize>,
    /// Notified after every counted hit.
    hit: Notify,
    cancellation: CancellationToken,
}

/// A local HTTP server serving fixed responses per path, counting hits per known route.
pub(crate) struct FixtureServer {
    address: std::net::SocketAddr,
    state: Arc<FixtureState>,
    task: JoinHandle<()>,
}

impl FixtureServer {
    pub(crate) async fn start(routes: HashMap<String, ResponseSpec>) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let cancellation = CancellationToken::new();
        let state = Arc::new(FixtureState {
            counters: routes
                .keys()
                .map(|path| (path.clone(), AtomicUsize::new(0)))
                .collect(),
            routes,
            hit: Notify::new(),
            cancellation: cancellation.clone(),
        });
        let app = Router::new()
            .fallback(handle_request)
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(cancellation.cancelled_owned())
                .await;
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub(crate) fn base_url(&self) -> reqwest::Url {
        format!("http://{}", self.address)
            .parse()
            .expect("fixture server address is a valid URL")
    }

    #[cfg(unix)]
    pub(crate) fn port(&self) -> u16 {
        self.address.port()
    }

    pub(crate) fn hits(&self, path: &str) -> usize {
        self.state
            .counters
            .get(path)
            .map_or(0, |counter| counter.load(Ordering::Acquire))
    }

    pub(crate) async fn wait_for_hits(&self, path: &str, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // Register interest before checking, so a hit in between is not missed.
                let hit = self.state.hit.notified();
                if self.hits(path) >= expected {
                    return;
                }
                hit.await;
            }
        })
        .await
        .expect("fixture route was requested before timeout");
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.state.cancellation.cancel();
        self.task.abort();
    }
}

async fn handle_request(State(state): State<Arc<FixtureState>>, request: Request) -> Response {
    let path = request.uri().path();
    let Some(spec) = state.routes.get(path).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(counter) = state.counters.get(path) {
        counter.fetch_add(1, Ordering::AcqRel);
        state.hit.notify_waiters();
    }

    match spec {
        ResponseSpec::Body(body) => body.into_response(),
        ResponseSpec::Delay(delay, body) => {
            tokio::time::sleep(delay).await;
            body.into_response()
        }
        ResponseSpec::Stall => {
            let stream =
                futures::stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"x")) })
                    .chain(futures::stream::pending());
            Body::from_stream(stream).into_response()
        }
    }
}

pub(crate) fn artifact_zip(executable: &Path, contents: &[u8]) -> zip::result::ZipResult<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let path = executable.to_string_lossy().replace('\\', "/");
    writer.start_file(path, SimpleFileOptions::default().unix_permissions(0o755))?;
    writer.write_all(contents)?;
    Ok(writer.finish()?.into_inner())
}

pub(crate) fn large_artifact_zip(executable: &Path) -> zip::result::ZipResult<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let package_root = executable
        .components()
        .next()
        .expect("platform executable path has a package root")
        .as_os_str()
        .to_string_lossy();
    let contents = [3_u8; 4 * 1024];
    for index in 0..2_000 {
        writer.start_file(
            format!("{package_root}/padding-{index:04}.bin"),
            SimpleFileOptions::default(),
        )?;
        writer.write_all(&contents)?;
    }
    let executable = executable.to_string_lossy().replace('\\', "/");
    writer.start_file(
        executable,
        SimpleFileOptions::default().unix_permissions(0o755),
    )?;
    writer.write_all(b"browser")?;
    Ok(writer.finish()?.into_inner())
}

pub(crate) fn platform_artifact_zips(
    platform: Platform,
) -> zip::result::ZipResult<(Vec<u8>, Vec<u8>)> {
    Ok((
        artifact_zip(platform.chrome_executable_path(), b"browser")?,
        artifact_zip(platform.chromedriver_executable_path(), b"driver")?,
    ))
}

pub(crate) async fn contains_transaction_residue(path: &Path) -> std::io::Result<bool> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || contains_transaction_residue_blocking(&path))
        .await
        .map_err(std::io::Error::other)?
}

fn contains_transaction_residue_blocking(path: &Path) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".staging-") || name.ends_with(".zip") {
            return Ok(true);
        }
        if entry.file_type()?.is_dir() && contains_transaction_residue_blocking(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Describes the behavior of a fake `chromedriver` executable, step by step.
///
/// The binary runs its steps in order and exits successfully after the last one. It exits
/// successfully on `SIGTERM` / `SIGINT` at any point, optionally printing a line first.
///
/// ```ignore
/// let executable = FakeChromedriverBinaryBuilder::new()
///     .print_line("warming up")
///     .announce_port(port)
///     .idle_until_terminated()
///     .write(directory.path().join("fake-chromedriver"))
///     .await?;
/// ```
#[cfg(unix)]
#[derive(Default)]
pub(crate) struct FakeChromedriverBinaryBuilder {
    /// Commands run, in order, when terminated, before exiting.
    on_termination: Vec<String>,
    steps: Vec<String>,
}

#[cfg(unix)]
impl FakeChromedriverBinaryBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Print `line` when terminated, before exiting.
    pub(crate) fn on_termination_print(mut self, line: &str) -> Self {
        self.on_termination
            .push(format!("printf '%s\\n' {}", shell_quote(line)));
        self
    }

    /// Create an empty file at `path` when terminated, before exiting, to signal that the binary
    /// was terminated gracefully rather than killed.
    pub(crate) fn on_termination_create_file(mut self, path: &Path) -> Self {
        self.on_termination
            .push(format!("touch {}", shell_quote_path(path)));
        self
    }

    /// Delay exiting by `delay` when terminated.
    pub(crate) fn on_termination_delay(mut self, delay: std::time::Duration) -> Self {
        self.on_termination
            .push(format!("sleep {}", delay.as_secs_f64()));
        self
    }

    /// Print `line` to stdout.
    pub(crate) fn print_line(self, line: &str) -> Self {
        self.step(format!("printf '%s\\n' {}", shell_quote(line)))
    }

    /// Print `line` to stdout, terminated by `\r\n` instead of `\n`.
    pub(crate) fn print_crlf_line(self, line: &str) -> Self {
        self.step(format!("printf '%s\\r\\n' {}", shell_quote(line)))
    }

    /// Print the line with which `ChromeDriver` announces the port it listens on.
    pub(crate) fn announce_port(self, port: impl std::fmt::Display) -> Self {
        self.print_line(&format!(
            "ChromeDriver was started successfully on port {port}."
        ))
    }

    /// Write the binary's process ID to `path`.
    pub(crate) fn record_pid(self, path: &Path) -> Self {
        self.step(format!("echo $$ > {}", shell_quote_path(path)))
    }

    /// Create an empty file at `path`, e.g. to signal that the binary started.
    pub(crate) fn create_file(self, path: &Path) -> Self {
        self.step(format!("touch {}", shell_quote_path(path)))
    }

    /// Block until a file exists at `path`.
    pub(crate) fn wait_for_file(self, path: &Path) -> Self {
        self.step(format!(
            "while [ ! -e {} ]; do sleep 0.05; done",
            shell_quote_path(path)
        ))
    }

    /// Start a child process that inherits the binary's output and outlives it for a while, the
    /// way Chrome's helper processes keep a crashed browser's output pipes open.
    pub(crate) fn spawn_child_holding_output(self) -> Self {
        self.step("sleep 5 &".to_owned())
    }

    /// Block until terminated.
    pub(crate) fn idle_until_terminated(self) -> Self {
        // Sleeping in the background keeps the binary responsive to signals. Termination signals
        // the whole process group, and dash reports the killed `sleep` as "Terminated" on stderr,
        // so `wait` is silenced to keep that out of captured output.
        self.step("while :; do sleep 1 & wait $! 2>/dev/null; done".to_owned())
    }

    /// Count the runs of the binary in the file at `counter`, and on each of the first `runs`
    /// runs, report the port as unavailable and exit with `1`, the way `ChromeDriver` does when
    /// its port is in use.
    pub(crate) fn report_port_unavailable_on_first_runs(self, counter: &Path, runs: u32) -> Self {
        let counter = shell_quote_path(counter);
        self.step(format!(
            "runs=$(cat {counter} 2>/dev/null || echo 0); echo $((runs + 1)) > {counter}; \
             if [ \"$runs\" -lt {runs} ]; then \
             printf '%s\\n' 'IPv4 port not available. Exiting...'; exit 1; fi"
        ))
    }

    /// Exit with `code`.
    pub(crate) fn exit(self, code: i32) -> Self {
        self.step(format!("exit {code}"))
    }

    /// Write the binary to `path` and make it executable.
    pub(crate) async fn write(self, path: impl Into<PathBuf>) -> std::io::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;

        let path = path.into();
        let mut trap = self.on_termination;
        trap.push("exit 0".to_owned());
        let trap = trap.join("; ");
        let mut script = format!("#!/bin/sh\ntrap {} TERM INT\n", shell_quote(&trap));
        for step in self.steps {
            script.push_str(&step);
            script.push('\n');
        }
        tokio::fs::write(&path, script).await?;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).await?;
        Ok(path)
    }

    fn step(mut self, command: String) -> Self {
        self.steps.push(command);
        self
    }
}

/// Quote `value` as a single shell word.
#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(unix)]
fn shell_quote_path(path: &Path) -> String {
    shell_quote(path.to_str().expect("test paths are valid UTF-8"))
}

/// A shared lease on a (new) cache at `cache_dir`.
pub(crate) async fn cache_lease(
    cache_dir: &Path,
) -> Result<crate::cache::CacheLease, rootcause::Report<crate::ChromeForTestingError>> {
    crate::cache::CacheDir::create_at(cache_dir.to_owned())?
        .acquire_shared(&CancellationToken::new())
        .await
}
