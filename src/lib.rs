//! Programmatic management of [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/)
//! installations.
//!
//! Resolves a `chrome` / `chromedriver` pair against the Chrome for Testing release index,
//! downloads it into a per-user cache, spawns `chromedriver` on a configurable or OS-assigned
//! port, and (with the default `thirtyfour` feature) provides managed `WebDriver` sessions that
//! tear down on drop.
//!
//! Start with [`Chromedriver::run`] / [`Chromedriver::run_default`]. Reach for
//! [`ChromeForTestingManager`] when you need finer control over the resolve / download / launch
//! steps.

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
pub(crate) mod version;

pub use chrome_for_testing::Channel;
pub use chrome_for_testing::Version;
pub use chromedriver::{Chromedriver, ChromedriverRunConfig, TerminationTimeouts};
pub use error::{ChromeForTestingArtifact, ChromeForTestingManagerError, Result};
pub use mgr::{ChromeForTestingManager, LoadedChromePackage};
pub use output::{
    DriverOutputInspectors, DriverOutputLine, DriverOutputListener, DriverOutputSource,
};
pub use port::{Port, PortRequest};
#[cfg(any(feature = "thirtyfour"))]
pub use session::Session;
pub use version::{SelectedVersion, VersionRequest};
