use crate::ChromeForTestingManagerError;
use rootcause::Report;
use rootcause::prelude::ResultExt;

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
    pub(crate) async fn quit(self) -> Result<(), Report<ChromeForTestingManagerError>> {
        self.driver
            .quit()
            .await
            .context(ChromeForTestingManagerError::QuitSession)
    }
}

impl std::ops::Deref for Session {
    type Target = thirtyfour::WebDriver;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}
