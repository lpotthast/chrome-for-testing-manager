/// A browser session. Used to control the browser.
///
/// When using `thirtyfour` (feature), this has a `Deref` impl to `thirtyfour::WebDriver`, so this
/// session can be seen as the `driver`.
#[derive(Debug)]
pub struct Session {
    #[cfg(feature = "thirtyfour")]
    pub(crate) driver: thirtyfour::WebDriver,
}

#[cfg(feature = "thirtyfour")]
pub type SessionError = thirtyfour::error::WebDriverError;

#[cfg(not(feature = "thirtyfour"))]
pub type SessionError = anyhow::Error;

impl Session {
    pub async fn quit(self) -> Result<(), SessionError> {
        #[cfg(feature = "thirtyfour")]
        {
            self.driver.quit().await
        }

        #[cfg(not(feature = "thirtyfour"))]
        {
            unimplemented!()
        }
    }
}

#[cfg(feature = "thirtyfour")]
impl std::ops::Deref for Session {
    type Target = thirtyfour::WebDriver;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}
