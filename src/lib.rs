#![allow(clippy::non_minimal_cfg)] // Keep provider feature lists easy to extend.

mod cache;
pub(crate) mod chromedriver;
mod download;
mod error;
pub(crate) mod mgr;
mod output;
pub(crate) mod port;
#[cfg(any(feature = "thirtyfour"))]
pub(crate) mod session;

pub use chrome_for_testing::Channel;
pub use chrome_for_testing::Version;
pub use chromedriver::Chromedriver;
pub use error::{ChromeForTestingArtifact, ChromeForTestingManagerError};
pub use mgr::{ChromeForTestingManager, VersionRequest};
pub use output::{
    ChromedriverRunConfig, DriverOutputLine, DriverOutputListener, DriverOutputSource,
};
pub use port::{Port, PortRequest};
#[cfg(any(feature = "thirtyfour"))]
pub use session::Session;
