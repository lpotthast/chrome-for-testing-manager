//! Guarded `ChromeDriver` process lifecycle and readiness detection.
//!
//! Processes terminate on drop, retain their package cache lease, and use explicit cleanup paths
//! for cancellation or startup failure.

use super::output::{DriverOutputLine, DriverOutputSubscription};
use crate::cache::CacheLease;
use crate::chromedriver::ChromeDriverConfig;
use crate::policy::LifecyclePolicy;
use crate::process_support::{ManagedProcess, StartupLine, StartupStream};
use crate::{
    CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Port, PortRequest, Result,
};
use rootcause::bail;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

/// Delay between local readiness probes against the `ChromeDriver` status endpoint.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// An owned, guarded `ChromeDriver` process.
///
/// This type intentionally hides the generic process implementation. It exposes the bound
/// [`Port`], output observation through [`Self::subscribe_output`] and [`Self::recent_output`],
/// and explicit consuming termination through [`Self::terminate`]. It retains the shutdown policy
/// supplied at launch and terminates automatically when dropped.
///
/// The drop guard requires an active multithreaded Tokio runtime: dropping this value on a thread
/// without one (for example after the runtime has shut down) panics instead of leaking the child
/// process. Prefer [`Self::terminate`] for observable, error-reporting shutdown.
#[derive(Debug)]
pub struct ChromeDriverProcess {
    process: ManagedProcess,
    port: Port,
    /// Declared last so that it is released only after the process was dropped.
    cache_lease: CacheLease,
}

impl ChromeDriverProcess {
    /// Return the TCP port on which `ChromeDriver` is listening.
    #[must_use]
    pub const fn port(&self) -> Port {
        self.port
    }

    /// Gracefully terminate `ChromeDriver` with the shutdown policy supplied at launch.
    ///
    /// Output subscriptions receive the lines printed during shutdown and observe
    /// [`crate::DriverOutputSubscriptionError::Closed`] afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated within that policy. The process is
    /// then killed; if even that fails, the error says so and the process may still be running.
    pub async fn terminate(self) -> Result<ExitStatus> {
        let Self {
            process,
            port: _,
            cache_lease,
        } = self;
        let result = process.terminate().await;
        drop(cache_lease);
        result
    }

    /// Subscribe to future `ChromeDriver` output without backpressuring the child process.
    ///
    /// Use [`Self::recent_output`] for lines printed before subscribing.
    #[must_use]
    pub fn subscribe_output(&self) -> DriverOutputSubscription {
        self.process.subscribe_output()
    }

    /// Return the most recent `ChromeDriver` output lines (up to 256, from spawn on), oldest
    /// first.
    #[must_use]
    pub fn recent_output(&self) -> Vec<DriverOutputLine> {
        self.process.recent_output()
    }

    pub(crate) async fn launch(
        executable: &Path,
        cache_lease: CacheLease,
        config: ChromeDriverConfig,
        cancellation: &CancellationToken,
        status_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
    ) -> Result<Self> {
        let requested_port = config.port();
        crate::ensure_multithreaded_runtime()?;
        crate::check_cancelled(cancellation)?;

        tracing::info!(path = %executable.display(), "launching chromedriver");
        let process = ManagedProcess::spawn(
            "chromedriver",
            ChromeForTestingArtifact::ChromeDriver,
            executable,
            Self::command(executable, requested_port),
            lifecycle.graceful_shutdown().clone(),
        )?;
        let startup_timeout = lifecycle.driver_startup_timeout();
        let deadline = Instant::now() + startup_timeout;
        let (process, port) = process
            .start(cancellation, async |process| {
                let reported_port = process
                    .wait_for_startup_line(
                        StartupStream::Stdout,
                        deadline.saturating_duration_since(Instant::now()),
                        Self::classify_startup_line,
                    )
                    .await?;
                let port = match requested_port {
                    PortRequest::Specific(requested) if requested != reported_port => {
                        bail!(ChromeForTestingError::ChromeDriverPortMismatch {
                            path: process.executable().to_owned(),
                            requested,
                            reported: reported_port,
                        });
                    }
                    PortRequest::Specific(requested) => requested,
                    PortRequest::Any => reported_port,
                };
                Self::probe_status(process, port, status_client, startup_timeout, deadline).await?;
                Ok(port)
            })
            .await?;

        Ok(Self {
            process,
            port,
            cache_lease,
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
        command
    }

    /// Recognize `ChromeDriver was started successfully on port <port>.` and parse the port.
    fn classify_startup_line(line: &str) -> StartupLine<Port> {
        if !line.contains("started successfully on port") {
            return StartupLine::Ignore;
        }
        line.trim()
            .trim_matches('"')
            .trim_end_matches('.')
            .split(' ')
            .next_back()
            .and_then(|value| value.parse::<u16>().ok())
            .and_then(Port::try_new)
            .map_or(StartupLine::Unrecognized, StartupLine::Ready)
    }

    async fn probe_status(
        process: &mut ManagedProcess,
        port: Port,
        status_client: &reqwest::Client,
        startup_timeout: Duration,
        deadline: Instant,
    ) -> Result<()> {
        let status_url = format!("http://127.0.0.1:{port}/status");
        loop {
            process.ensure_running()?;

            if Instant::now() >= deadline {
                bail!(ChromeForTestingError::ChromeDriverNotReady {
                    path: process.executable().to_owned(),
                    port,
                    timeout: startup_timeout,
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

#[cfg(test)]
mod tests {
    use super::ChromeDriverProcess;
    use crate::cache::{CacheDir, CacheLease};
    use crate::policy::LifecyclePolicy;
    #[cfg(unix)]
    use crate::test_support::write_executable;
    use crate::test_support::{FixtureServer, ResponseSpec, TestDirectory, cache_lease};
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
    async fn early_exit_is_reported_with_its_status() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-startup-output-closed")?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(&executable, "#!/bin/sh\nexit 0\n").await?;

        let error = launch(
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
        .expect_err("a process that exits before startup must fail");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::ExitedDuringStartup { status, .. } if status.success()
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
                "#!/bin/sh\ntrap 'echo shutdown-complete; exit 0' TERM INT\necho 'ChromeDriver was started successfully on port {}.'\nwhile :; do sleep 1 & wait $!; done\n",
                status_server.port()
            ),
        )
        .await?;

        let process = launch(
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
                "#!/bin/sh\ntrap 'exit 0' TERM INT\necho 'ChromeDriver was started successfully on port {}.'\nwhile :; do sleep 1 & wait $!; done\n",
                status_server.port()
            ),
        )
        .await?;
        let cache = CacheDir::create_at(directory.path().join("cache"))?;
        let cache_lease = cache.acquire_shared(&CancellationToken::new()).await?;
        let lifecycle = test_lifecycle();
        let status_client = test_status_client()?;

        let process = launch(
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
                "#!/bin/sh\ntrap 'exit 0' TERM INT\necho 'ChromeDriver was started successfully on port {}.'\nwhile :; do sleep 1 & wait $!; done\n",
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

        let error = launch(
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
            ChromeForTestingError::ChromeDriverNotReady { .. }
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
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1 & wait $!; done\n",
        )
        .await?;
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(Duration::from_millis(100))
            .build();

        let error = launch(
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
                "#!/bin/sh\ntrap 'exit 0' TERM INT\necho 'ChromeDriver was started successfully on port {reported}.'\nwhile :; do sleep 1 & wait $!; done\n"
            ),
        )
        .await?;

        let error = launch(
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
        let directory = TestDirectory::new("chromedriver-startup-cancellation")?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            concat!(
                "#!/bin/sh\n",
                "trap 'exit 0' TERM INT\n",
                "script_dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n",
                "echo $$ > \"$script_dir/pid\"\n",
                "touch \"$script_dir/started\"\n",
                "while :; do sleep 1 & wait $!; done\n",
            ),
        )
        .await?;

        let cancellation = CancellationToken::new();
        let cancel_after_start = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while tokio::fs::metadata(directory.path().join("started"))
                    .await
                    .is_err()
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fake process started");
            cancellation.cancel();
        };
        let status_client = test_status_client()?;
        let lifecycle = test_lifecycle();
        let (result, ()) = tokio::join!(
            launch(
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
    #[tokio::test(flavor = "multi_thread")]
    async fn subscriptions_close_when_the_driver_exits_on_its_own() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("chromedriver-self-exit-output")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let executable = directory.path().join("fake-chromedriver.sh");
        let release = directory.path().join("release");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\necho 'ChromeDriver was started successfully on port {}.'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\necho 'exiting on my own'\n",
                status_server.port(),
                release.display()
            ),
        )
        .await?;

        let process = launch(
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
        tokio::fs::write(&release, "").await?;

        let lines = tokio::time::timeout(Duration::from_secs(5), async {
            let mut lines = Vec::new();
            while let Ok(line) = subscription.recv().await {
                lines.push(line.line);
            }
            lines
        })
        .await
        .expect("the subscription must close once the driver's output ends");
        assert_that!(lines).contains_exactly(["exiting on my own".to_owned()]);
        assert_that!(
            process
                .recent_output()
                .iter()
                .map(|line| line.line.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly([
            format!(
                "ChromeDriver was started successfully on port {}.",
                status_server.port()
            )
            .as_str(),
            "exiting on my own",
        ]);
        process.terminate().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn unrecognized_startup_line_fails_fast_with_recent_output()
    -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-unrecognized-startup")?;
        let executable = directory.path().join("fake-chromedriver.sh");
        write_executable(
            &executable,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\necho 'warming up'\necho 'ChromeDriver was started successfully on port soon.'\nwhile :; do sleep 1 & wait $!; done\n",
        )
        .await?;
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(Duration::from_secs(30))
            .build();
        let started = std::time::Instant::now();

        let error = launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::default(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &lifecycle,
        )
        .await
        .expect_err("an unparsable port must fail startup");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::UnrecognizedStartupOutput { line, .. } if line.contains("port soon")
        ))
        .is_true();
        assert_that!(started.elapsed() < Duration::from_secs(10))
            .with_detail_message("startup must not wait for its deadline")
            .is_true();
        assert_that!(format!("{error:?}")).contains("[stdout] warming up");
        Ok(())
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
        Ok(cache_lease(&directory.path().join("cache")).await?)
    }

    fn test_status_client() -> Result<reqwest::Client, rootcause::Report> {
        Ok(reqwest::Client::builder().no_proxy().build()?)
    }

    struct ChromeDriverLaunchRequest {
        executable: std::path::PathBuf,
        cache_lease: CacheLease,
        config: ChromeDriverConfig,
        cancellation: CancellationToken,
    }

    async fn launch(
        request: ChromeDriverLaunchRequest,
        status_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
    ) -> crate::Result<ChromeDriverProcess> {
        ChromeDriverProcess::launch(
            &request.executable,
            request.cache_lease,
            request.config,
            &request.cancellation,
            status_client,
            lifecycle,
        )
        .await
    }
}
