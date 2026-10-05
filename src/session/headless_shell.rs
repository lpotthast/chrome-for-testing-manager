//! Chrome Headless Shell launch, capability validation, and `DevTools` readiness.
//!
//! `ChromeDriver` attaches to an already-running shell through `goog:chromeOptions.debuggerAddress`.
//! Options it cannot apply to a running browser are rejected, and the shell is terminated only
//! after `WebDriver` cleanup.

use crate::cache::CacheLease;
use crate::policy::LifecyclePolicy;
use crate::process_support::{ManagedProcess, StartupLine, StartupStream};
use crate::{
    CancellationToken, ChromeForTestingArtifact, ChromeForTestingError, LoadedBrowserPackage,
    Result,
};
use rootcause::{bail, prelude::ResultExt};
use std::process::ExitStatus;
use thirtyfour::ChromiumLikeCapabilities;
use tokio::process::Command;
use tokio::time::Instant;

const DEFAULT_REMOTE_DEBUGGING_ARG: &str = "--remote-debugging-port=0";

#[derive(Debug)]
pub(crate) struct HeadlessShellSession {
    process: ManagedProcess,
    /// Declared last so that it is released only after the process was dropped.
    cache_lease: CacheLease,
}

impl HeadlessShellSession {
    /// The shell's recent output, formatted as a report attachment, or `None` if there is none.
    pub(crate) fn formatted_recent_output(&self) -> Option<String> {
        crate::process_support::format_output(&self.process.recent_output())
    }

    pub(crate) async fn terminate(self) -> Result<ExitStatus> {
        let Self {
            process,
            cache_lease,
        } = self;
        let result = process.terminate().await;
        drop(cache_lease);
        result
    }

    /// Launch the shell, wait for `DevTools`, open an initial page, and point `caps` at it.
    ///
    /// The whole startup, including the initial page request, is bounded by the lifecycle's
    /// Headless Shell startup deadline.
    pub(crate) async fn launch(
        loaded: &LoadedBrowserPackage,
        caps: &mut thirtyfour::ChromeCapabilities,
        devtools_client: &reqwest::Client,
        lifecycle: &LifecyclePolicy,
        cancellation: &CancellationToken,
    ) -> Result<HeadlessShellSession> {
        crate::check_cancelled(cancellation)?;

        let executable = loaded.browser_executable();
        // `prepare_caps` points the capabilities at the cached shell. The shell is launched here
        // and `ChromeDriver` only attaches to it, so drop that binary; any other binary a caller
        // configured remains and is rejected below.
        if caps.binary().as_deref() == executable.to_str() {
            caps.unset_binary();
        }
        let mut command = Command::new(executable);
        command.args(launch_args(caps)?);
        let process = ManagedProcess::spawn(
            "chrome-headless-shell",
            ChromeForTestingArtifact::ChromeHeadlessShell,
            executable,
            command,
            lifecycle.graceful_shutdown().clone(),
        )?;

        let startup_timeout = lifecycle.headless_shell_startup_timeout();
        let deadline = Instant::now() + startup_timeout;
        let (process, ()) = process
            .start(cancellation, async |process| {
                let debugger_address = process
                    .wait_for_startup_line(
                        StartupStream::Stderr,
                        startup_timeout,
                        classify_startup_line,
                    )
                    .await?;
                // Open an initial page so that `ChromeDriver` has a target to attach to.
                let page_error = || ChromeForTestingError::CreateInitialBrowserPage {
                    debugger_address: debugger_address.clone(),
                };
                let request = async {
                    devtools_client
                        .put(format!("http://{debugger_address}/json/new?about:blank"))
                        .send()
                        .await?
                        .error_for_status()
                };
                match tokio::time::timeout_at(deadline, request).await {
                    Ok(response) => response.map(drop).context_with(page_error)?,
                    Err(_) => {
                        return Err(process.startup_timeout_error(startup_timeout).attach(format!(
                            "DevTools at {debugger_address} did not open the initial page in time"
                        )));
                    }
                }
                caps.set_debugger_address(&debugger_address)
                    .context(ChromeForTestingError::ConfigureSessionCapabilities)
            })
            .await?;

        Ok(Self {
            process,
            cache_lease: loaded.cache_lease(),
        })
    }
}

/// Recognize `DevTools listening on ws://<address>/...` and extract the address.
fn classify_startup_line(line: &str) -> StartupLine<String> {
    if !line.contains("DevTools listening on") {
        return StartupLine::Ignore;
    }
    parse_devtools_address(line).map_or(StartupLine::Unrecognized, StartupLine::Ready)
}

fn parse_devtools_address(line: &str) -> Option<String> {
    let (_, suffix) = line.split_once("DevTools listening on ws://")?;
    let (address, _) = suffix.split_once('/')?;
    (!address.is_empty()).then(|| address.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDebuggingArg {
    Port,
    Invalid,
}

/// The shell's command-line arguments: the capability args plus exactly one TCP remote debugging
/// port (an OS-assigned one unless the caller configured a port).
fn launch_args(caps: &thirtyfour::ChromeCapabilities) -> Result<Vec<String>> {
    use thirtyfour::BrowserCapabilitiesHelper;

    validate_capabilities(caps)?;
    let mut launch_args = Vec::new();
    let mut remote_debugging_port_arg = None::<String>;
    for arg in caps.args().into_iter().map(normalize_arg) {
        match classify_remote_debugging_arg(&arg) {
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
    launch_args
        .push(remote_debugging_port_arg.unwrap_or_else(|| DEFAULT_REMOTE_DEBUGGING_ARG.to_owned()));
    Ok(launch_args)
}

/// Prefix a switch given without its leading `--` (e.g. `user-agent=x`), like `ChromeDriver` does
/// for the Chrome it launches itself. Passed unchanged, the shell would open it as a URL instead.
fn normalize_arg(arg: String) -> String {
    if arg.starts_with("--") {
        arg
    } else {
        format!("--{arg}")
    }
}

/// Reject `goog:chromeOptions` other than `args`, which `ChromeDriver` cannot apply when attaching
/// to a running shell.
fn validate_capabilities(caps: &thirtyfour::ChromeCapabilities) -> Result<()> {
    let Some(serde_json::Value::Object(options)) = caps.as_ref().get("goog:chromeOptions") else {
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_REMOTE_DEBUGGING_ARG, RemoteDebuggingArg, classify_remote_debugging_arg,
        launch_args, parse_devtools_address,
    };
    use crate::ChromeForTestingError;
    use assertr::prelude::*;
    use thirtyfour::ChromiumLikeCapabilities;

    #[test]
    fn parses_devtools_address() {
        assert_that!(parse_devtools_address(
            "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc"
        ))
        .is_equal_to(Some("127.0.0.1:9222".to_owned()));
    }

    #[test]
    fn classifies_remote_debugging_args() {
        assert_that!(classify_remote_debugging_arg("--remote-debugging-pipe"))
            .is_equal_to(Some(RemoteDebuggingArg::Invalid));
        assert_that!(classify_remote_debugging_arg("--remote-debugging-port=0"))
            .is_equal_to(Some(RemoteDebuggingArg::Port));
        assert_that!(classify_remote_debugging_arg("--remote-debugging-port"))
            .is_equal_to(Some(RemoteDebuggingArg::Invalid));
    }

    #[test]
    fn launch_args_add_default_port() -> Result<(), rootcause::Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("--headless=new")?;
        assert_that!(launch_args(&caps)?)
            .is_equal_to(["--headless=new", DEFAULT_REMOTE_DEBUGGING_ARG]);
        Ok(())
    }

    #[test]
    fn launch_args_normalize_switches_without_leading_dashes() -> Result<(), rootcause::Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("user-agent=probe-agent")?;
        caps.add_arg("remote-debugging-port=9222")?;
        assert_that!(launch_args(&caps)?)
            .is_equal_to(["--user-agent=probe-agent", "--remote-debugging-port=9222"]);

        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.add_arg("remote-debugging-pipe")?;
        let error = launch_args(&caps).expect_err("a dashless pipe switch must be rejected too");
        assert_that!(matches!(
            error.current_context(),
            ChromeForTestingError::InvalidHeadlessShellRemoteDebuggingArg { arg }
                if arg == "--remote-debugging-pipe"
        ))
        .is_true();
        Ok(())
    }

    #[test]
    fn launch_args_reject_options_that_cannot_apply_to_attached_shell()
    -> Result<(), rootcause::Report> {
        let mut caps = thirtyfour::ChromeCapabilities::new();
        caps.set_binary("/tmp/different-browser")?;

        let error = launch_args(&caps)
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
