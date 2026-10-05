//! Scoped `thirtyfour` sessions and auxiliary browser lifecycle support.
//!
//! Session operations own connection and cleanup ordering, including the separate Chrome Headless
//! Shell process required for attached sessions.

mod builder;
pub(crate) mod headless_shell;

pub use builder::SessionBuilder;

use crate::ChromeForTestingError;
use rootcause::Report;
use rootcause::prelude::ResultExt;
use std::ops::Deref;

/// A browser session. Used to control the browser.
///
/// When using `thirtyfour` (feature), this has a `Deref` impl to `thirtyfour::WebDriver`, so this
/// session can be seen as the `driver`.
#[derive(Debug)]
pub struct Session {
    pub(crate) driver: thirtyfour::WebDriver,
}

impl Session {
    /// Quit the browser session.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying `WebDriver` session cannot be closed.
    pub(crate) async fn quit(self) -> Result<(), Report<ChromeForTestingError>> {
        self.driver
            .quit()
            .await
            .context(ChromeForTestingError::QuitSession)
    }
}

impl Deref for Session {
    type Target = thirtyfour::WebDriver;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}
