//! Chrome Headless Shell launch, capability validation, and `DevTools` readiness.
//!
//! Attached sessions reject options that `ChromeDriver` cannot apply and terminate the browser only
//! after `WebDriver` cleanup.

use crate::policy::LifecyclePolicy;
use crate::process_support::{self, ManagedProcessHandle};
use crate::{
    CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, LoadedBrowserPackage,
    Result,
};
use rootcause::{Report, bail, option_ext::OptionExt, prelude::ResultExt};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thirtyfour::ChromiumLikeCapabilities;
use tokio::process::Command;
use tokio_process_tools::{GracefulShutdown, WaitForLineResult};

const BROWSER_STARTUP_OUTPUT_LINES: usize = 80;
const DEFAULT_REMOTE_DEBUGGING_ARG: &str = "--remote-debugging-port=0";

type RecentBrowserOutput = Arc<Mutex<VecDeque<String>>>;

#[derive(Debug)]
pub(crate) struct HeadlessShellSession {
    process: ManagedProcessHandle,
    shutdown: GracefulShutdown,
}

impl HeadlessShellSession {
    pub(crate) async fn terminate(self) -> Result<std::process::ExitStatus> {
        let Self {
            mut process,
            shutdown,
        } = self;
        process
            .terminate(shutdown)
            .await
            .context(ChromeForTestingError::TerminateProcess {
                artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
            })
    }

    pub(crate) async fn launch(
        loaded: &LoadedBrowserPackage,
        caps: &mut thirtyfour::ChromeCapabilities,
        shutdown: GracefulShutdown,
        devtools_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
        cancellation: &CancellationToken,
    ) -> Result<HeadlessShellSession> {
        crate::check_cancelled(cancellation)?;

        let executable = loaded.browser_executable();
        let mut command = Command::new(executable);
        command.args(HeadlessShellLaunchOptions::from_capabilities(caps)?.args);
        let process = process_support::spawn_guarded(
            "chrome-headless-shell",
            command,
            ChromeForTestingArtifact::ChromeHeadlessShell,
            executable,
            shutdown.clone(),
        )?;

        let (process, _debugger_address) = process_support::drive_startup(
            process,
            ChromeForTestingArtifact::ChromeHeadlessShell,
            executable,
            shutdown.clone(),
            cancellation,
            async |process| {
                Self::start_devtools(
                    Self::process_startup(process, executable, lifecycle.browser_startup_timeout()),
                    devtools_client,
                    caps,
                    cancellation,
                )
                .await
            },
        )
        .await?;

        Ok(Self { process, shutdown })
    }

    async fn start_devtools(
        startup: impl Future<Output = Result<String>>,
        client: &reqwest::Client,
        caps: &mut thirtyfour::ChromeCapabilities,
        cancellation: &CancellationToken,
    ) -> Result<String> {
        let debugger_address = startup.await?;
        Self::create_initial_page(client, &debugger_address, cancellation).await?;
        caps.set_debugger_address(&debugger_address)
            .context(ChromeForTestingError::ConfigureSessionCapabilities)?;
        Ok(debugger_address)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDebuggingArg {
    Port,
    Invalid,
}

#[derive(Debug)]
struct HeadlessShellLaunchOptions {
    args: Vec<String>,
}

impl HeadlessShellLaunchOptions {
    fn from_capabilities(caps: &thirtyfour::ChromeCapabilities) -> Result<Self> {
        use thirtyfour::BrowserCapabilitiesHelper;

        Self::validate_capabilities(caps)?;
        let mut launch_args = Vec::new();
        let mut remote_debugging_port_arg = None::<String>;
        for arg in caps.args() {
            match Self::classify_remote_debugging_arg(&arg) {
                Some(RemoteDebuggingArg::Invalid) => {
                    bail!(ChromeForTestingError::InvalidHeadlessShellRemoteDebuggingArg { arg });
                }
                Some(RemoteDebuggingArg::Port) => {
                    if let Some(first_arg) = &remote_debugging_port_arg {
                        bail!(
                            ChromeForTestingError::ConflictingHeadlessShellRemoteDebuggingArgs {
                                first_arg: first_arg.clone(),
                                second_arg: arg,
                            }
                        );
                    }
                    remote_debugging_port_arg = Some(arg);
                }
                None => launch_args.push(arg),
            }
        }
        launch_args.push(
            remote_debugging_port_arg.unwrap_or_else(|| DEFAULT_REMOTE_DEBUGGING_ARG.to_owned()),
        );
        Ok(Self { args: launch_args })
    }

    fn validate_capabilities(caps: &thirtyfour::ChromeCapabilities) -> Result<()> {
        let Some(serde_json::Value::Object(options)) = caps.as_ref().get("goog:chromeOptions")
        else {
            return Ok(());
        };
        if let Some(option) = options.keys().find(|option| option.as_str() != "args") {
            bail!(ChromeForTestingError::UnsupportedHeadlessShellCapability {
                option: option.clone(),
            });
        }
        Ok(())
    }

    fn classify_remote_debugging_arg(arg: &str) -> Option<RemoteDebuggingArg> {
        if arg == "--remote-debugging-pipe"
            || arg.starts_with("--remote-debugging-pipe=")
            || arg == "--remote-debugging-port"
        {
            return Some(RemoteDebuggingArg::Invalid);
        }
        let port = arg.strip_prefix("--remote-debugging-port=")?;
        Some(if port.parse::<u16>().is_ok() {
            RemoteDebuggingArg::Port
        } else {
            RemoteDebuggingArg::Invalid
        })
    }
}

impl HeadlessShellSession {
    async fn process_startup(
        process: &mut ManagedProcessHandle,
        executable: &Path,
        startup_timeout: Duration,
    ) -> Result<String> {
        let debugger_address = Arc::new(Mutex::new(None::<String>));
        let callback_address = Arc::clone(&debugger_address);
        let recent_output = Arc::new(Mutex::new(VecDeque::new()));
        let callback_output = Arc::clone(&recent_output);
        let startup_result = match process
            .stderr()
            .wait_for_line(
                startup_timeout,
                move |line| {
                    Self::push_recent_output(&callback_output, line.as_ref());
                    let Some(address) = Self::parse_devtools_address(&line) else {
                        return false;
                    };
                    *callback_address
                        .lock()
                        .expect("debugger address mutex is not poisoned") = Some(address);
                    true
                },
                process_support::startup_line_options(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                Self::log_recent_startup_output(executable, &recent_output);
                return Err(Report::new_sendsync(error).context(
                    ChromeForTestingError::WaitForStartup {
                        artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                        path: executable.to_owned(),
                        timeout: startup_timeout,
                    },
                ));
            }
        };

        match startup_result {
            WaitForLineResult::Matched => {}
            WaitForLineResult::StreamClosed => {
                Self::log_recent_startup_output(executable, &recent_output);
                bail!(ChromeForTestingError::StartupOutputClosed {
                    artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                    path: executable.to_owned(),
                });
            }
            WaitForLineResult::Timeout => {
                Self::log_recent_startup_output(executable, &recent_output);
                bail!(ChromeForTestingError::WaitForStartup {
                    artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                    path: executable.to_owned(),
                    timeout: startup_timeout,
                });
            }
        }
        debugger_address
            .lock()
            .expect("debugger address mutex is not poisoned")
            .clone()
            .context(ChromeForTestingError::WaitForStartup {
                artifact: ChromeForTestingArtifact::ChromeHeadlessShell,
                path: executable.to_owned(),
                timeout: startup_timeout,
            })
    }

    async fn create_initial_page(
        client: &reqwest::Client,
        debugger_address: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let response = crate::await_or_cancelled(
            cancellation,
            client
                .put(format!("http://{debugger_address}/json/new?about:blank"))
                .send(),
        )
        .await?
        .context(ChromeForTestingError::CreateInitialBrowserPage {
            debugger_address: debugger_address.to_owned(),
        })?;
        response
            .error_for_status()
            .context(ChromeForTestingError::CreateInitialBrowserPage {
                debugger_address: debugger_address.to_owned(),
            })?;
        Ok(())
    }

    fn push_recent_output(recent_output: &RecentBrowserOutput, line: &str) {
        let mut recent_output = recent_output
            .lock()
            .expect("recent browser output mutex is not poisoned");
        if recent_output.len() == BROWSER_STARTUP_OUTPUT_LINES {
            recent_output.pop_front();
        }
        recent_output.push_back(line.to_owned());
    }

    fn log_recent_startup_output(executable: &Path, recent_output: &RecentBrowserOutput) {
        let recent_output = recent_output
            .lock()
            .expect("recent browser output mutex is not poisoned");
        if recent_output.is_empty() {
            tracing::error!(
                path = %executable.display(),
                "Chrome Headless Shell exited before DevTools startup without captured stderr"
            );
        } else {
            tracing::error!(
                path = %executable.display(),
                output = %recent_output.iter().map(String::as_str).collect::<Vec<_>>().join("\n"),
                "Chrome Headless Shell startup failed"
            );
        }
    }

    fn parse_devtools_address(line: &str) -> Option<String> {
        let (_, suffix) = line.split_once("DevTools listening on ws://")?;
        let (address, _) = suffix.split_once('/')?;
        (!address.is_empty()).then(|| address.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_REMOTE_DEBUGGING_ARG, HeadlessShellLaunchOptions, HeadlessShellSession,
        RemoteDebuggingArg,
    };
    use crate::ChromeForTestingError;
    use assertr::prelude::*;
    use thirtyfour::ChromiumLikeCapabilities;

    #[test]
    fn parses_devtools_address() {
        assert_that!(HeadlessShellSession::parse_devtools_address(
            "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc"
        ))
        .is_equal_to(Some("127.0.0.1:9222".to_owned()));
    }

    #[test]
    fn classifies_remote_debugging_args() {
        assert_that!(HeadlessShellLaunchOptions::classify_remote_debugging_arg(
            "--remote-debugging-pipe"
        ))
        .is_equal_to(Some(RemoteDebuggingArg::Invalid));
        assert_that!(HeadlessShellLaunchOptions::classify_remote_debugging_arg(
            "--remote-debugging-port=0"
        ))
        .is_equal_to(Some(RemoteDebuggingArg::Port));
        assert_that!(HeadlessShellLaunchOptions::classify_remote_debugging_arg(
            "--remote-debugging-port"
        ))
        .is_equal_to(Some(RemoteDebuggingArg::Invalid));
    }

    #[test]
    fn launch_args_add_default_port() -> Result<(), rootcause::Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--headless=new")?;
        assert_that!(HeadlessShellLaunchOptions::from_capabilities(&caps)?.args)
            .is_equal_to(["--headless=new", DEFAULT_REMOTE_DEBUGGING_ARG]);
        Ok(())
    }

    #[test]
    fn launch_args_reject_options_that_cannot_apply_to_attached_shell()
    -> Result<(), rootcause::Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_binary("/tmp/different-browser")?;

        let error = HeadlessShellLaunchOptions::from_capabilities(&caps)
            .expect_err("ChromeDriver cannot apply binary after the shell is already running");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::UnsupportedHeadlessShellCapability { option }
                if option == "binary"
        ))
        .is_true();
        Ok(())
    }
}
