//! Guarded `ChromeDriver` process lifecycle and readiness detection.
//!
//! Processes terminate on drop, retain their package cache lease, and use explicit cleanup paths
//! for cancellation or startup failure.

use super::output::{DriverOutputInspectors, DriverOutputSubscription};
use crate::cache::CacheLease;
use crate::chromedriver::ChromeDriverConfig;
use crate::policy::LifecyclePolicy;
use crate::process_support::{self, ManagedProcessHandle};
use crate::{
    CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Port, PortRequest, Result,
};
use rootcause::{bail, prelude::ResultExt};
use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;
use tokio_process_tools::{GracefulShutdown, RunningState, WaitForLineResult};

/// Delay between local readiness probes against the `ChromeDriver` status endpoint.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// An owned, guarded `ChromeDriver` process.
///
/// This type intentionally hides the generic process implementation. It retains the configured
/// shutdown policy and output inspectors, terminates automatically when dropped, and exposes only
/// the bound [`Port`] plus explicit consuming termination.
///
/// The drop guard requires an active multithreaded Tokio runtime: dropping this value on a thread
/// without one (for example after the runtime has shut down) panics instead of leaking the child
/// process. Prefer [`Self::terminate`] for observable, error-reporting shutdown.
pub struct ChromeDriverProcess {
    process: ManagedProcessHandle,
    cache_lease: CacheLease,
    port: Port,
    shutdown: GracefulShutdown,
    output_inspectors: DriverOutputInspectors,
}

impl Debug for ChromeDriverProcess {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromeDriverProcess")
            .field("process", &self.process)
            .field("cache_lease", &self.cache_lease)
            .field("port", &self.port)
            .field("shutdown", &self.shutdown)
            .field("output_inspectors", &self.output_inspectors)
            .finish()
    }
}

impl ChromeDriverProcess {
    /// Return the TCP port on which `ChromeDriver` is listening.
    #[must_use]
    pub const fn port(&self) -> Port {
        self.port
    }

    /// Gracefully terminate `ChromeDriver` with the shutdown policy supplied at launch.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated within that policy.
    pub async fn terminate(self) -> Result<ExitStatus> {
        let Self {
            mut process,
            cache_lease,
            port: _,
            shutdown,
            output_inspectors,
        } = self;
        let _cache_lease = cache_lease;
        let _output_inspectors = output_inspectors;
        process
            .terminate(shutdown)
            .await
            .context(ChromeForTestingError::TerminateProcess {
                artifact: ChromeForTestingArtifact::ChromeDriver,
            })
    }

    /// Subscribe to future `ChromeDriver` output without backpressuring the child process.
    #[must_use]
    pub fn subscribe_output(&self) -> DriverOutputSubscription {
        self.output_inspectors.subscribe()
    }
}

pub(crate) struct ChromeDriverLaunchRequest {
    pub(crate) executable: PathBuf,
    pub(crate) cache_lease: CacheLease,
    pub(crate) config: ChromeDriverConfig,
    pub(crate) cancellation: CancellationToken,
}

impl ChromeDriverProcess {
    pub(crate) async fn launch(
        request: ChromeDriverLaunchRequest,
        status_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
    ) -> Result<Self> {
        let ChromeDriverLaunchRequest {
            executable,
            cache_lease,
            config,
            cancellation,
        } = request;
        let port = config.port();
        let shutdown = lifecycle.graceful_shutdown().clone();
        crate::ensure_multithreaded_runtime()?;
        crate::check_cancelled(&cancellation)?;

        tracing::info!(path = %executable.display(), "launching chromedriver");
        let process = process_support::spawn_guarded(
            "chromedriver",
            Self::command(&executable, port),
            ChromeForTestingArtifact::ChromeDriver,
            &executable,
            shutdown.clone(),
        )?;
        let output_inspectors = DriverOutputInspectors::start(&process);
        let startup_deadline = Instant::now() + lifecycle.driver_startup_timeout();
        let (process, actual_port) = process_support::drive_startup(
            process,
            ChromeForTestingArtifact::ChromeDriver,
            &executable,
            shutdown.clone(),
            &cancellation,
            async |process| {
                Self::wait_until_ready(
                    process,
                    &executable,
                    port,
                    status_client,
                    lifecycle,
                    startup_deadline,
                )
                .await
            },
        )
        .await?;

        Ok(Self {
            process,
            cache_lease,
            port: actual_port,
            shutdown,
            output_inspectors,
        })
    }

    fn command(executable: &Path, port: PortRequest) -> Command {
        let mut command = Command::new(executable);
        let requested_port = match port {
            PortRequest::Any => 0,
            PortRequest::Specific(port) => port.as_u16(),
        };
        command.arg(format!("--port={requested_port}"));
        let log_level = chrome_for_testing::chromedriver::LogLevel::Info;
        command.arg(format!("--log-level={log_level}"));
        apply_creation_flags(&mut command);
        command
    }

    async fn wait_until_ready(
        process: &mut ManagedProcessHandle,
        executable: &Path,
        requested_port: PortRequest,
        status_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
        deadline: Instant,
    ) -> Result<Port> {
        let reported_port = Self::discover_port(
            process,
            executable,
            lifecycle.driver_startup_timeout(),
            deadline,
        )
        .await?;
        let port = match requested_port {
            PortRequest::Specific(requested) if requested != reported_port => {
                bail!(ChromeForTestingError::ChromeDriverPortMismatch {
                    path: executable.to_owned(),
                    requested,
                    reported: reported_port,
                });
            }
            PortRequest::Specific(requested) => requested,
            PortRequest::Any => reported_port,
        };
        Self::probe_status(
            process,
            executable,
            port,
            status_client,
            lifecycle,
            deadline,
        )
        .await?;
        Ok(port)
    }

    async fn discover_port(
        process: &mut ManagedProcessHandle,
        executable: &Path,
        startup_timeout: Duration,
        deadline: Instant,
    ) -> Result<Port> {
        let started_on_port = Arc::new(AtomicU16::new(0));
        let callback_port = Arc::clone(&started_on_port);
        let startup_result = process
            .stdout()
            .wait_for_line(
                deadline.saturating_duration_since(Instant::now()),
                move |line| {
                    if !line.contains("started successfully on port") {
                        return false;
                    }
                    let Some(port) = line
                        .trim()
                        .trim_matches('"')
                        .trim_end_matches('.')
                        .split(' ')
                        .next_back()
                        .and_then(|value| value.parse::<u16>().ok())
                        .and_then(Port::try_new)
                    else {
                        tracing::error!(%line, "failed to parse port from chromedriver output");
                        return false;
                    };
                    callback_port.store(port.as_u16(), Ordering::Release);
                    true
                },
                process_support::startup_line_options(),
            )
            .await
            .context(ChromeForTestingError::WaitForStartup {
                artifact: ChromeForTestingArtifact::ChromeDriver,
                path: executable.to_owned(),
                timeout: startup_timeout,
            })?;

        match startup_result {
            WaitForLineResult::Matched => {}
            WaitForLineResult::StreamClosed => {
                bail!(ChromeForTestingError::StartupOutputClosed {
                    artifact: ChromeForTestingArtifact::ChromeDriver,
                    path: executable.to_owned(),
                });
            }
            WaitForLineResult::Timeout => {
                bail!(ChromeForTestingError::WaitForStartup {
                    artifact: ChromeForTestingArtifact::ChromeDriver,
                    path: executable.to_owned(),
                    timeout: startup_timeout,
                });
            }
        }
        Ok(Port::try_new(started_on_port.load(Ordering::Acquire))
            .expect("matched ChromeDriver startup output stores a nonzero port"))
    }

    async fn probe_status(
        process: &mut ManagedProcessHandle,
        executable: &Path,
        port: Port,
        status_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
        deadline: Instant,
    ) -> Result<()> {
        let status_url = format!("http://127.0.0.1:{port}/status");
        loop {
            match process.is_running() {
                RunningState::Running => {}
                RunningState::Terminated(status) => {
                    bail!(ChromeForTestingError::ExitedDuringStartup {
                        artifact: ChromeForTestingArtifact::ChromeDriver,
                        path: executable.to_owned(),
                        status,
                    });
                }
                RunningState::Uncertain(error) => {
                    tracing::debug!(%error, "could not determine ChromeDriver startup state");
                }
            }

            if Instant::now() >= deadline {
                bail!(ChromeForTestingError::WaitForStartup {
                    artifact: ChromeForTestingArtifact::ChromeDriver,
                    path: executable.to_owned(),
                    timeout: lifecycle.driver_startup_timeout(),
                });
            }

            // The deadline must cover the whole probe: a server that sends headers and then
            // stalls the body would otherwise extend startup beyond the configured timeout.
            let probe = async {
                let response = status_client.get(&status_url).send().await.ok()?;
                if !response.status().is_success() {
                    return None;
                }
                response.text().await.ok()
            };
            if let Ok(Some(body)) = tokio::time::timeout_at(deadline, probe).await
                && Self::webdriver_status_is_ready(&body)
            {
                return Ok(());
            }

            tokio::time::sleep_until(deadline.min(Instant::now() + READINESS_POLL_INTERVAL)).await;
        }
    }

    fn webdriver_status_is_ready(body: &str) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };
        value
            .get("value")
            .and_then(|value| value.get("ready"))
            .or_else(|| value.get("ready"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

#[cfg(target_os = "windows")]
fn apply_creation_flags(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn apply_creation_flags(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::{ChromeDriverLaunchRequest, ChromeDriverProcess};
    use crate::cache::{CacheDir, CacheLease};
    use crate::policy::LifecyclePolicy;
    use crate::test_support::{FixtureServer, ResponseSpec, TestDirectory};
    use crate::{
        CancellationToken, ChromeDriverConfig, ChromeForTestingError, GracefulShutdown, PortRequest,
    };
    use assertr::prelude::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::process::Command;

    #[test]
    fn random_port_is_encoded_explicitly() {
        let command =
            ChromeDriverProcess::command(std::path::Path::new("chromedriver"), PortRequest::Any);
        let args = command.as_std().get_args().collect::<Vec<_>>();

        assert_that!(args.first().copied()).is_equal_to(Some(std::ffi::OsStr::new("--port=0")));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn startup_output_closure_is_not_reported_as_a_timeout() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("chromedriver-startup-output-closed")?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(&executable, "#!/bin/sh\nexit 0\n").await?;

        let error = ChromeDriverProcess::launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::default(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &test_lifecycle(),
        )
        .await
        .expect_err("a process that closes stdout before startup must fail");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::StartupOutputClosed { .. }
        ))
        .is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_termination_keeps_output_subscription_alive() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("chromedriver-termination-output")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\necho 'ChromeDriver was started successfully on port {}.'\ntrap 'echo shutdown-complete; exit 0' TERM INT\nwhile :; do :; done\n",
                status_server.port()
            ),
        )
        .await?;

        let process = ChromeDriverProcess::launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::default(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &test_lifecycle(),
        )
        .await?;
        let mut subscription = process.subscribe_output();
        process.terminate().await?;

        let mut lines = Vec::new();
        while let Ok(line) = subscription.recv().await {
            lines.push(line.line);
        }
        assert_that!(lines.iter().any(|line| line == "shutdown-complete"))
            .with_detail_message(
                "the subscription must receive output emitted during graceful termination",
            )
            .is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_port_uses_status_readiness_and_process_retains_cache_lease()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-status-readiness")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\necho 'ChromeDriver was started successfully on port {}.'\ntrap 'exit 0' TERM INT\nwhile :; do :; done\n",
                status_server.port()
            ),
        )
        .await?;
        let cache = CacheDir::create_at(directory.path().join("cache"))?;
        let cache_lease = cache.acquire_shared(CancellationToken::new()).await?;
        let lifecycle = test_lifecycle();
        let status_client = test_status_client()?;

        let process = ChromeDriverProcess::launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease,
                config: ChromeDriverConfig::builder()
                    .port(crate::Port::new(status_server.port()))
                    .build(),
                cancellation: CancellationToken::new(),
            },
            &status_client,
            &lifecycle,
        )
        .await?;
        assert_that!(process.port().as_u16()).is_equal_to(status_server.port());
        assert_that!(matches!(
            cache
                .clear()
                .await
                .expect_err("running process must retain the package cache lease")
                .current_context(),
            ChromeForTestingError::CacheInUse { .. }
        ))
        .is_true();

        process.terminate().await?;
        cache.clear().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn stalled_status_body_does_not_extend_the_startup_deadline()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-stalled-status-body")?;
        let status_server =
            FixtureServer::start(HashMap::from([("/status".to_owned(), ResponseSpec::Stall)]))
                .await?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\necho 'ChromeDriver was started successfully on port {}.'\ntrap 'exit 0' TERM INT\nwhile :; do :; done\n",
                status_server.port()
            ),
        )
        .await?;
        let startup_timeout = Duration::from_millis(300);
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(startup_timeout)
            .build();
        let started = std::time::Instant::now();

        let error = ChromeDriverProcess::launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::builder()
                    .port(crate::Port::new(status_server.port()))
                    .build(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &lifecycle,
        )
        .await
        .expect_err("a status endpoint that stalls its body must not count as ready");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::WaitForStartup { .. }
        ))
        .is_true();
        assert_that!(started.elapsed() < startup_timeout + Duration::from_secs(5))
            .with_detail_message("the stalled body read must be bounded by the startup deadline")
            .is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_port_does_not_accept_an_unrelated_ready_server() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("chromedriver-fixed-port-attribution")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do :; done\n",
        )
        .await?;
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(Duration::from_millis(100))
            .build();

        let error = ChromeDriverProcess::launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::builder()
                    .port(crate::Port::new(status_server.port()))
                    .build(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &lifecycle,
        )
        .await
        .expect_err("the spawned process must report startup itself");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::WaitForStartup { .. }
        ))
        .is_true();
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn fixed_port_rejects_a_different_port_reported_by_the_child()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-fixed-port-mismatch")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let requested = crate::Port::new(status_server.port());
        let reported = crate::Port::new(if status_server.port() == u16::MAX {
            u16::MAX - 1
        } else {
            status_server.port() + 1
        });
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\necho 'ChromeDriver was started successfully on port {reported}.'\ntrap 'exit 0' TERM INT\nwhile :; do :; done\n"
            ),
        )
        .await?;

        let error = ChromeDriverProcess::launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::builder().port(requested).build(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &test_lifecycle(),
        )
        .await
        .expect_err("the child must bind the requested fixed port");

        let ChromeForTestingError::ChromeDriverPortMismatch {
            requested: actual_requested,
            reported: actual_reported,
            ..
        } = error.current_context()
        else {
            panic!("expected typed port mismatch, got {error:?}");
        };
        assert_that!(*actual_requested).is_equal_to(requested);
        assert_that!(*actual_reported).is_equal_to(reported);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_startup_terminates_guarded_process()
    -> Result<(), rootcause::Report> {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("chromedriver-startup-cancellation")?;
        let executable = directory.path().join("fake-chromedriver.sh");
        tokio::fs::write(
            &executable,
            concat!(
                "#!/bin/sh\n",
                "script_dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n",
                "echo $$ > \"$script_dir/pid\"\n",
                "touch \"$script_dir/started\"\n",
                "trap 'exit 0' TERM INT\n",
                "while :; do :; done\n",
            ),
        )
        .await?;
        let mut permissions = tokio::fs::metadata(&executable).await?.permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(&executable, permissions).await?;

        let cancellation = CancellationToken::new();
        let cancel_after_start = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while tokio::fs::metadata(directory.path().join("started"))
                    .await
                    .is_err()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("fake process started");
            cancellation.cancel();
        };
        let status_client = test_status_client()?;
        let lifecycle = test_lifecycle();
        let (result, ()) = tokio::join!(
            ChromeDriverProcess::launch(
                ChromeDriverLaunchRequest {
                    executable,
                    cache_lease: test_cache_lease(&directory).await?,
                    config: ChromeDriverConfig::default(),
                    cancellation: cancellation.clone(),
                },
                &status_client,
                &lifecycle,
            ),
            cancel_after_start,
        );
        let error = result.expect_err("startup must be cancelled");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::Cancelled
        ))
        .is_true();
        let pid = tokio::fs::read_to_string(directory.path().join("pid")).await?;
        let output = Command::new("kill")
            .arg("-0")
            .arg(pid.trim())
            .output()
            .await?;
        assert_that!(output.status.success())
            .with_detail_message("cancelled startup process remained alive")
            .is_false();
        Ok(())
    }

    #[cfg(unix)]
    async fn write_executable(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        tokio::fs::write(path, contents).await?;
        let mut permissions = tokio::fs::metadata(path).await?.permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(path, permissions).await
    }

    fn test_shutdown() -> GracefulShutdown {
        GracefulShutdown::builder()
            .unix_sigterm(Duration::from_secs(2))
            .windows_ctrl_break(Duration::from_secs(2))
            .build()
    }

    fn test_lifecycle() -> LifecyclePolicy {
        LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .build()
    }

    async fn test_cache_lease(directory: &TestDirectory) -> Result<CacheLease, rootcause::Report> {
        Ok(CacheDir::create_at(directory.path().join("cache"))?
            .acquire_shared(CancellationToken::new())
            .await?)
    }

    fn test_status_client() -> Result<reqwest::Client, rootcause::Report> {
        Ok(reqwest::Client::builder().no_proxy().build()?)
    }
}
