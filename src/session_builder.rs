use crate::ChromeForTestingManagerError;
use crate::chromedriver::Chromedriver;
use crate::session::Session;
use rootcause::prelude::ResultExt;
use rootcause::{IntoReportCollection, Report, markers::SendSync};
use thirtyfour::prelude::WebDriverError;
use thirtyfour::{ChromeCapabilities, WebDriverBuilder};

/// Type-state marker: no capability customization will be applied to the session.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct DefaultCaps;

/// Type-state marker: no `WebDriverBuilder` customization will be applied to the session.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct DefaultConfig;

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

impl sealed::Sealed for DefaultCaps {}
impl ApplyCaps for DefaultCaps {
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

impl sealed::Sealed for DefaultConfig {}
impl ApplyConfig for DefaultConfig {
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

impl<'a> SessionBuilder<'a, DefaultCaps, DefaultConfig> {
    pub(crate) fn new(chromedriver: &'a Chromedriver) -> Self {
        Self {
            chromedriver,
            caps_setup: DefaultCaps,
            config_setup: DefaultConfig,
        }
    }
}

impl<'a, B> SessionBuilder<'a, DefaultCaps, B> {
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

impl<'a, C> SessionBuilder<'a, C, DefaultConfig> {
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
        let builder = thirtyfour::WebDriver::builder(format!("http://localhost:{port}"), caps);
        let driver = self
            .config_setup
            .apply(builder)
            .connect()
            .await
            .context(ChromeForTestingManagerError::StartWebDriverSession { port })?;

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
                std::panic::resume_unwind(payload);
            }
        };

        let quit_result = session.quit().await;

        match (user_result, quit_result) {
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
        }
    }
}
