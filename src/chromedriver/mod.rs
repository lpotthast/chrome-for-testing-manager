//! `ChromeDriver` configuration, launch, process ownership, and output observation.
//!
//! This private namespace owns the technical driver boundary. The user-facing
//! [`crate::ChromeForTesting`] facade composes it without exposing driver mechanics by default.

mod config;
pub(crate) mod output;
pub(crate) mod process;

pub use config::{ChromeDriverConfig, ChromeDriverLogLevel};
