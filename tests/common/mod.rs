//! Shared configuration and browser-flow helpers for public integration tests.

#[allow(dead_code)]
pub mod browser_flow;

use chrome_for_testing_manager::ChromeForTestingConfig;
use std::path::PathBuf;

/// Build an integration-test config rooted under this checkout's `target/` directory.
#[allow(dead_code)]
pub fn chrome_config() -> ChromeForTestingConfig {
    ChromeForTestingConfig::builder()
        .cache_dir(cache_dir())
        .build()
}

pub fn cache_dir() -> PathBuf {
    std::env::current_dir()
        .expect("integration test process has a current directory")
        .join("target")
        .join("integration-test-cache")
        .join("shared")
}
