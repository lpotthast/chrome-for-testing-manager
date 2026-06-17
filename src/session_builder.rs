use crate::ChromeForTestingManagerError;
use crate::chromedriver::Chromedriver;
use crate::mgr::{HeadlessShellSession, LoadedBrowserPackage};
use crate::session::Session;
use rootcause::prelude::ResultExt;
use rootcause::{IntoReportCollection, Report, markers::SendSync};
use thirtyfour::ChromiumLikeCapabilities;
use thirtyfour::prelude::WebDriverError;
use thirtyfour::{ChromeCapabilities, WebDriverBuilder};

/// Type-state marker: no capability customization will be applied to the session.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct InitialCaps;

/// Type-state marker: no `WebDriverBuilder` customization will be applied to the session.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct InitialConfig;

/// Type-state wrapper carrying a user-provided capabilities setup closure.
#[doc(hidden)]
pub struct CapsSetup<F>(F);

/// Type-state wrapper carrying a user-provided `WebDriverBuilder` setup closure.
#[doc(hidden)]
pub struct ConfigSetup<F>(F);

mod sealed {
    pub trait Sealed {}
}

#[doc(hidden)]
pub trait ApplyCaps: sealed::Sealed {
    fn apply(self, caps: &mut ChromeCapabilities) -> Result<(), WebDriverError>;
}

impl sealed::Sealed for InitialCaps {}
impl ApplyCaps for InitialCaps {
    fn apply(self, _caps: &mut ChromeCapabilities) -> Result<(), WebDriverError> {
        Ok(())
    }
}

impl<F> sealed::Sealed for CapsSetup<F> {}
impl<F> ApplyCaps for CapsSetup<F>
where
    F: FnOnce(&mut ChromeCapabilities) -> Result<(), WebDriverError>,
{
    fn apply(self, caps: &mut ChromeCapabilities) -> Result<(), WebDriverError> {
        self.0(caps)
    }
}

#[doc(hidden)]
pub trait ApplyConfig: sealed::Sealed {
    fn apply(self, builder: WebDriverBuilder) -> WebDriverBuilder;
}

impl sealed::Sealed for InitialConfig {}
impl ApplyConfig for InitialConfig {
    fn apply(self, builder: WebDriverBuilder) -> WebDriverBuilder {
        builder
    }
}

impl<F> sealed::Sealed for ConfigSetup<F> {}
impl<F> ApplyConfig for ConfigSetup<F>
where
    F: FnOnce(WebDriverBuilder) -> WebDriverBuilder,
{
    fn apply(self, builder: WebDriverBuilder) -> WebDriverBuilder {
        self.0(builder)
    }
}

/// A scoped, chainable builder for opening a `thirtyfour` [`Session`] against a running
/// [`Chromedriver`].
///
/// Obtained via [`Chromedriver::session`]. Optional setup steps:
///
/// - [`Self::with_caps`] mutates the [`ChromeCapabilities`] before the session opens (e.g. unset
///   headless, add Chrome args).
/// - [`Self::with_config`] receives the [`WebDriverBuilder`] and may configure the element poller,
///   request timeout, user-agent, or keep-alive flag.
///
/// Call [`Self::run`] to open the session and execute the user closure inside scoped, panic-safe
/// cleanup that always calls `WebDriver::quit().await`.
pub struct SessionBuilder<'a, C, B> {
    chromedriver: &'a Chromedriver,
    caps_setup: C,
    config_setup: B,
}

impl<'a> SessionBuilder<'a, InitialCaps, InitialConfig> {
    pub(crate) fn new(chromedriver: &'a Chromedriver) -> Self {
        Self {
            chromedriver,
            caps_setup: InitialCaps,
            config_setup: InitialConfig,
        }
    }
}

impl<'a, B> SessionBuilder<'a, InitialCaps, B> {
    /// Provide a closure that mutates the [`ChromeCapabilities`] used to create the session.
    pub fn with_caps<F>(self, f: F) -> SessionBuilder<'a, CapsSetup<F>, B>
    where
        F: FnOnce(&mut ChromeCapabilities) -> Result<(), WebDriverError>,
    {
        SessionBuilder {
            chromedriver: self.chromedriver,
            caps_setup: CapsSetup(f),
            config_setup: self.config_setup,
        }
    }
}

impl<'a, C> SessionBuilder<'a, C, InitialConfig> {
    /// Provide a closure that configures the [`WebDriverBuilder`] before the session is opened.
    pub fn with_config<F>(self, f: F) -> SessionBuilder<'a, C, ConfigSetup<F>>
    where
        F: FnOnce(WebDriverBuilder) -> WebDriverBuilder,
    {
        SessionBuilder {
            chromedriver: self.chromedriver,
            caps_setup: self.caps_setup,
            config_setup: ConfigSetup(f),
        }
    }
}

impl<C, B> SessionBuilder<'_, C, B>
where
    C: ApplyCaps,
    B: ApplyConfig,
{
    /// Open a [`Session`], hand it to the user closure, and tear it down once the closure resolves
    /// or panics.
    ///
    /// Cleanup runs regardless of outcome. A panic in the user closure is caught, the session is
    /// quit, and the panic is resumed afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error if capability setup, session creation, the user closure, or the quit call
    /// fails. A user error combined with a quit error is reported with the quit error attached as
    /// a child.
    pub async fn run<T, E, F>(self, f: F) -> Result<T, Report<ChromeForTestingManagerError>>
    where
        F: for<'b> AsyncFnOnce(&'b Session) -> Result<T, E>,
        E: IntoReportCollection<SendSync>,
    {
        use futures::FutureExt;

        let chromedriver = self.chromedriver;
        let port = chromedriver.port();

        let mut caps = chromedriver.mgr.prepare_caps(&chromedriver.loaded)?;
        self.caps_setup
            .apply(&mut caps)
            .context(ChromeForTestingManagerError::ConfigureSessionCapabilities)?;
        let mut headless_shell = match &chromedriver.loaded {
            LoadedBrowserPackage::Chrome(_) => None,
            LoadedBrowserPackage::ChromeHeadlessShell(headless_shell_package) => {
                let headless_shell = chromedriver
                    .mgr
                    .launch_headless_shell_session(
                        headless_shell_package,
                        &caps,
                        chromedriver.graceful_shutdown.clone(),
                    )
                    .await?;

                caps.set_debugger_address(headless_shell.debugger_address())
                    .context(ChromeForTestingManagerError::ConfigureSessionCapabilities)?;

                Some(headless_shell)
            }
        };
        let builder = thirtyfour::WebDriver::builder(format!("http://localhost:{port}"), caps);
        let driver = match self
            .config_setup
            .apply(builder)
            .connect()
            .await
            .context(ChromeForTestingManagerError::StartWebDriverSession { port })
        {
            Ok(driver) => driver,
            Err(err) => {
                if let Err(termination_err) = terminate_headless_shell(headless_shell.take()).await
                {
                    tracing::warn!(
                        error = %termination_err,
                        "failed to terminate Chrome Headless Shell after WebDriver session start failure"
                    );
                }
                return Err(err);
            }
        };

        let session = Session { driver };

        let maybe_panicked = core::panic::AssertUnwindSafe(f(&session))
            .catch_unwind()
            .await;

        let user_result = match maybe_panicked {
            Ok(result) => result.context(ChromeForTestingManagerError::RunSessionCallback),
            Err(payload) => {
                if let Err(quit_err) = session.quit().await {
                    tracing::error!(
                        "Failed to quit WebDriver session after user callback panic: {quit_err:?}"
                    );
                }
                if let Err(terminate_err) = terminate_headless_shell(headless_shell).await {
                    tracing::error!(
                        "Failed to terminate Chrome Headless Shell after user callback panic: {terminate_err:?}"
                    );
                }
                std::panic::resume_unwind(payload);
            }
        };

        let quit_result = session.quit().await;
        let browser_termination_result = terminate_headless_shell(headless_shell).await;

        let session_result = match (user_result, quit_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(quit_err)) => Err(quit_err),
            (Err(user_err), Ok(())) => Err(user_err),
            (Err(mut user_err), Err(quit_err)) => {
                tracing::error!(
                    "Failed to quit WebDriver session after user failure: {quit_err:?}"
                );
                user_err
                    .children_mut()
                    .push(quit_err.into_dynamic().into_cloneable());
                Err(user_err)
            }
        };

        match (session_result, browser_termination_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(terminate_err)) => Err(terminate_err),
            (Err(session_err), Ok(())) => Err(session_err),
            (Err(mut session_err), Err(terminate_err)) => {
                tracing::error!(
                    "Failed to terminate Chrome Headless Shell after session failure: {terminate_err:?}"
                );
                session_err
                    .children_mut()
                    .push(terminate_err.into_dynamic().into_cloneable());
                Err(session_err)
            }
        }
    }
}

async fn terminate_headless_shell(
    headless_shell: Option<HeadlessShellSession>,
) -> Result<(), Report<ChromeForTestingManagerError>> {
    if let Some(headless_shell) = headless_shell {
        headless_shell.terminate().await?;
    }
    Ok(())
}
