use thiserror::Error;

/// A browser session. Used to control the browser.
///
/// When using `thirtyfour` (feature), this has a `Deref` impl to `thirtyfour::WebDriver`, so this
/// session can be seen as the `driver`.
#[derive(Debug)]
pub struct Session {
    #[cfg(feature = "thirtyfour")]
    pub(crate) driver: thirtyfour::WebDriver,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("The user code panicked:\n{reason}")]
    Panic {
        reason: String
    },

    #[cfg(feature = "thirtyfour")]
    #[error("thirtyfour WebDriverError")]
    Thirtyfour {
        #[from]
        source: thirtyfour::error::WebDriverError,
    },
}

impl Session {
    pub async fn quit(self) -> Result<(), SessionError> {
        #[cfg(feature = "thirtyfour")]
        {
            self.driver.quit().await.map_err(Into::into)
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
