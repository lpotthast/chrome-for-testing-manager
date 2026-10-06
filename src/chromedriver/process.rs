//! Guarded `ChromeDriver` process lifecycle and readiness detection.
//!
//! Processes terminate on drop, retain their package cache lease, and use explicit cleanup paths
//! for cancellation or startup failure.

use super::output::{DriverOutputLine, DriverOutputSubscription};
use crate::cache::CacheLease;
use crate::chromedriver::ChromeDriverConfig;
use crate::error::attach_child;
use crate::policy::LifecyclePolicy;
use crate::process_support::{ManagedProcess, StartupLine, StartupStream, deadline_after};
use crate::{
    CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, Port, PortRequest, Result,
};
use rootcause::{Report, bail, report};
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

/// Delay between local readiness probes against the `ChromeDriver` status endpoint.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How much of a not-ready status body to keep for the error report.
const MAX_REPORTED_STATUS_BODY: usize = 1024;

/// An owned, guarded `ChromeDriver` process.
///
/// This type intentionally hides the generic process implementation. It exposes the bound
/// [`Port`], output observation through [`Self::subscribe_output`] and [`Self::recent_output`],
/// and explicit consuming termination through [`Self::terminate`]. It retains the shutdown policy
/// supplied at launch and terminates automatically when dropped.
///
/// The drop guard requires an active multithreaded Tokio runtime: dropping this value on a thread
/// without one (for example after the runtime has shut down) panics instead of leaking the child
/// process. Dropping it also panics, after sending a kill signal, when the process cannot be
/// terminated. Prefer [`Self::terminate`] for observable, error-reporting shutdown.
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
    ///
    /// To combine it with [`Self::subscribe_output`], use
    /// [`Self::subscribe_output_with_history`], which neither loses nor duplicates lines printed in
    /// between.
    #[must_use]
    pub fn recent_output(&self) -> Vec<DriverOutputLine> {
        self.process.recent_output()
    }

    /// Return the most recent `ChromeDriver` output lines (up to 256, oldest first) together with
    /// a subscription to every line printed afterwards.
    ///
    /// Every line is either part of the returned history or delivered to the subscription, never
    /// both.
    #[must_use]
    pub fn subscribe_output_with_history(
        &self,
    ) -> (Vec<DriverOutputLine>, DriverOutputSubscription) {
        self.process.subscribe_output_with_history()
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
            Self::command(executable, &config),
            lifecycle.graceful_shutdown().clone(),
        )?;
        let startup_timeout = lifecycle.driver_startup_timeout();
        let deadline = deadline_after(startup_timeout);
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

    fn command(executable: &Path, config: &ChromeDriverConfig) -> Command {
        let mut command = Command::new(executable);
        let requested_port = match config.port() {
            PortRequest::Any => 0,
            PortRequest::Specific(port) => port.as_u16(),
        };
        command.arg(format!("--port={requested_port}"));
        command.arg(format!("--log-level={}", config.log_level()));
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
        // Why the most recent probe did not confirm readiness, reported if startup times out.
        let mut last_probe_failure: Option<Report> = None;
        loop {
            process.ensure_running()?;

            if Instant::now() >= deadline {
                let mut error = report!(ChromeForTestingError::ChromeDriverNotReady {
                    path: process.executable().to_owned(),
                    port,
                    timeout: startup_timeout,
                });
                if let Some(probe_failure) = last_probe_failure {
                    attach_child(&mut error, probe_failure);
                }
                return Err(error);
            }

            // The deadline must cover the whole probe: a server that sends headers and then
            // stalls the body would otherwise extend startup beyond the configured timeout.
            let probe = async {
                let response = status_client
                    .get(&status_url)
                    .send()
                    .await
                    .map_err(|error| Report::new_sendsync(error).into_dynamic())?;
                let status = response.status();
                if !status.is_success() {
                    return Err(report!("{status_url} answered with status {status}"));
                }
                response
                    .text()
                    .await
                    .map_err(|error| Report::new_sendsync(error).into_dynamic())
            };
            match tokio::time::timeout_at(deadline, probe).await {
                Ok(Ok(body)) if Self::webdriver_status_is_ready(&body) => return Ok(()),
                Ok(Ok(body)) => {
                    let body = truncate_on_char_boundary(&body, MAX_REPORTED_STATUS_BODY);
                    last_probe_failure =
                        Some(report!("{status_url} did not report readiness: {body}"));
                }
                Ok(Err(probe_failure)) => last_probe_failure = Some(probe_failure),
                // The deadline passed; the next iteration reports it.
                Err(_) => {}
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

/// The longest prefix of `value` of at most `max_len` bytes, not splitting a character.
fn truncate_on_char_boundary(value: &str, max_len: usize) -> &str {
    if value.len() <= max_len {
        return value;
    }
    let mut end = max_len;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::ChromeDriverProcess;
    use crate::{ChromeDriverConfig, ChromeDriverLogLevel};
    use assertr::prelude::*;
    // The process tests below drive fake shell-script executables and therefore only run on Unix.
    #[cfg(unix)]
    use crate::{
        CancellationToken, ChromeForTestingError, GracefulShutdown,
        cache::{CacheDir, CacheLease},
        policy::LifecyclePolicy,
        test_support::{
            FakeChromedriverBinaryBuilder, FixtureServer, ResponseSpec, TestDirectory, cache_lease,
        },
    };
    #[cfg(unix)]
    use std::{collections::HashMap, time::Duration};
    #[cfg(unix)]
    use tokio::process::Command;

    #[test]
    fn random_port_is_encoded_explicitly() {
        let command = ChromeDriverProcess::command(
            std::path::Path::new("chromedriver"),
            &ChromeDriverConfig::default(),
        );
        let args = command.as_std().get_args().collect::<Vec<_>>();

        assert_that!(args).is_equal_to(vec![
            std::ffi::OsStr::new("--port=0"),
            std::ffi::OsStr::new("--log-level=INFO"),
        ]);
    }

    #[test]
    fn configured_log_level_is_passed_to_chromedriver() {
        let config = ChromeDriverConfig::builder()
            .log_level(ChromeDriverLogLevel::Warning)
            .build();
        let command = ChromeDriverProcess::command(std::path::Path::new("chromedriver"), &config);
        let args = command.as_std().get_args().collect::<Vec<_>>();

        assert_that!(args.last().copied())
            .is_equal_to(Some(std::ffi::OsStr::new("--log-level=WARNING")));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn early_exit_is_reported_with_its_status() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-startup-output-closed")?;
        let executable = FakeChromedriverBinaryBuilder::new()
            .exit(0)
            .write(directory.path().join("fake-chromedriver"))
            .await?;

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
        let executable = FakeChromedriverBinaryBuilder::new()
            .on_termination_print("shutdown-complete")
            .announce_port(status_server.port())
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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
    async fn history_and_subscription_split_output_without_overlap() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("chromedriver-history-subscription")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let executable = FakeChromedriverBinaryBuilder::new()
            .on_termination_print("shutdown-complete")
            .print_crlf_line("crlf-terminated")
            .announce_port(status_server.port())
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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
        let (history, mut subscription) = process.subscribe_output_with_history();
        process.terminate().await?;

        let history = history
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_that!(history.iter().any(|line| line == "crlf-terminated"))
            .with_detail_message("a `\\r\\n` terminator must be stripped completely")
            .is_true();
        let mut subscribed = Vec::new();
        while let Ok(line) = subscription.recv().await {
            subscribed.push(line.line);
        }
        assert_that!(subscribed).contains_exactly(["shutdown-complete".to_owned()]);
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
        let executable = FakeChromedriverBinaryBuilder::new()
            .announce_port(status_server.port())
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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
        let executable = FakeChromedriverBinaryBuilder::new()
            .announce_port(status_server.port())
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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
        let executable = FakeChromedriverBinaryBuilder::new()
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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
        let executable = FakeChromedriverBinaryBuilder::new()
            .announce_port(reported)
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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
        let pid_file = directory.path().join("pid");
        let started_file = directory.path().join("started");
        let executable = FakeChromedriverBinaryBuilder::new()
            .record_pid(&pid_file)
            .create_file(&started_file)
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
            .await?;

        let cancellation = CancellationToken::new();
        let cancel_after_start = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while tokio::fs::metadata(&started_file).await.is_err() {
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
        let pid = tokio::fs::read_to_string(&pid_file).await?;
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
        let release = directory.path().join("release");
        let executable = FakeChromedriverBinaryBuilder::new()
            .announce_port(status_server.port())
            .wait_for_file(&release)
            .print_line("exiting on my own")
            .write(directory.path().join("fake-chromedriver"))
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
        let executable = FakeChromedriverBinaryBuilder::new()
            .print_line("warming up")
            .announce_port("soon")
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
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

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn unbounded_startup_timeout_does_not_overflow() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-unbounded-startup")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
        )]))
        .await?;
        let executable = FakeChromedriverBinaryBuilder::new()
            .announce_port(status_server.port())
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
            .await?;
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(Duration::MAX)
            .build();

        let process = launch(
            ChromeDriverLaunchRequest {
                executable,
                cache_lease: test_cache_lease(&directory).await?,
                config: ChromeDriverConfig::default(),
                cancellation: CancellationToken::new(),
            },
            &test_status_client()?,
            &lifecycle,
        )
        .await?;
        process.terminate().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn not_ready_status_reports_the_last_probe() -> Result<(), rootcause::Report> {
        let directory = TestDirectory::new("chromedriver-not-ready-status")?;
        let status_server = FixtureServer::start(HashMap::from([(
            "/status".to_owned(),
            ResponseSpec::body(
                br#"{"value":{"ready":false,"message":"still warming up"}}"#.to_vec(),
            ),
        )]))
        .await?;
        let executable = FakeChromedriverBinaryBuilder::new()
            .announce_port(status_server.port())
            .idle_until_terminated()
            .write(directory.path().join("fake-chromedriver"))
            .await?;
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(Duration::from_millis(300))
            .build();

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
        .expect_err("a driver that never reports readiness must fail startup");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::ChromeDriverNotReady { .. }
        ))
        .is_true();
        assert_that!(format!("{error:?}")).contains("still warming up");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn terminated_process_can_be_dropped_outside_a_multithreaded_runtime()
    -> Result<(), rootcause::Report> {
        let multi_thread = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let directory = TestDirectory::new("chromedriver-terminate-current-thread")?;
        let (process, _status_server) = multi_thread.block_on(async {
            let status_server = FixtureServer::start(HashMap::from([(
                "/status".to_owned(),
                ResponseSpec::body(br#"{"value":{"ready":true}}"#.to_vec()),
            )]))
            .await?;
            let executable = FakeChromedriverBinaryBuilder::new()
                .announce_port(status_server.port())
                .idle_until_terminated()
                .write(directory.path().join("fake-chromedriver"))
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
            Ok::<_, rootcause::Report>((process, status_server))
        })?;

        // Terminating settles the drop guard, so the handle is dropped without a runtime check.
        let current_thread = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        current_thread.block_on(process.terminate())?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn exit_is_reported_while_a_child_holds_the_output_open() -> Result<(), rootcause::Report>
    {
        let directory = TestDirectory::new("chromedriver-exit-output-held")?;
        let executable = FakeChromedriverBinaryBuilder::new()
            .spawn_child_holding_output()
            .exit(3)
            .write(directory.path().join("fake-chromedriver"))
            .await?;
        let lifecycle = LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .driver_startup_timeout(Duration::from_millis(300))
            .build();

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
        .expect_err("a process that exits before startup must fail");

        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::ExitedDuringStartup { status, .. } if status.code() == Some(3)
        ))
        .is_true();
        Ok(())
    }

    #[cfg(unix)]
    fn test_shutdown() -> GracefulShutdown {
        GracefulShutdown::builder()
            .unix_sigterm(Duration::from_secs(2))
            .windows_ctrl_break(Duration::from_secs(2))
            .build()
    }

    #[cfg(unix)]
    fn test_lifecycle() -> LifecyclePolicy {
        LifecyclePolicy::builder()
            .graceful_shutdown(test_shutdown())
            .build()
    }

    #[cfg(unix)]
    async fn test_cache_lease(directory: &TestDirectory) -> Result<CacheLease, rootcause::Report> {
        Ok(cache_lease(&directory.path().join("cache")).await?)
    }

    #[cfg(unix)]
    fn test_status_client() -> Result<reqwest::Client, rootcause::Report> {
        Ok(reqwest::Client::builder().no_proxy().build()?)
    }

    #[cfg(unix)]
    struct ChromeDriverLaunchRequest {
        executable: std::path::PathBuf,
        cache_lease: CacheLease,
        config: ChromeDriverConfig,
        cancellation: CancellationToken,
    }

    #[cfg(unix)]
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
