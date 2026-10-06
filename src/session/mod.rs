//! Scoped `thirtyfour` sessions and auxiliary browser lifecycle support.
//!
//! Session operations own connection and cleanup ordering, including the separate Chrome Headless
//! Shell process required for attached sessions.

mod builder;
pub(crate) mod headless_shell;

pub use builder::SessionBuilder;

use crate::ChromeForTestingError;
use rootcause::prelude::ResultExt;
use rootcause::{Report, report};
use std::ops::Deref;
use std::time::Duration;

/// A browser session, handed to the closure of [`SessionBuilder::run`].
///
/// Dereferences to [`thirtyfour::WebDriver`], so the session can be used as the driver. Use
/// [`Self::driver`] (or clone it) where an owned or explicitly typed driver is needed.
#[derive(Debug)]
pub struct Session {
    pub(crate) driver: thirtyfour::WebDriver,
}

impl Session {
    /// Return the `WebDriver` controlling this session's browser.
    #[must_use]
    pub const fn driver(&self) -> &thirtyfour::WebDriver {
        &self.driver
    }

    /// Quit the browser session, waiting at most `timeout`.
    ///
    /// If quitting fails or times out, the driver handle is leaked instead of dropped:
    /// `thirtyfour` would otherwise retry the quit synchronously while dropping the handle,
    /// blocking a runtime worker. The error is reported either way.
    pub(crate) async fn quit_within(
        self,
        timeout: Duration,
    ) -> Result<(), Report<ChromeForTestingError>> {
        let handle = self.driver.clone();
        let result = match tokio::time::timeout(timeout, self.driver.quit()).await {
            Ok(result) => result.context(ChromeForTestingError::QuitSession),
            Err(_) => Err(report!(ChromeForTestingError::QuitSessionTimeout {
                timeout
            })),
        };
        if result.is_err() {
            // The handle cannot have quit successfully, so leaking cannot fail.
            let _ = handle.leak();
        }
        result
    }
}

impl Deref for Session {
    type Target = thirtyfour::WebDriver;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}

impl AsRef<thirtyfour::WebDriver> for Session {
    fn as_ref(&self) -> &thirtyfour::WebDriver {
        &self.driver
    }
}
