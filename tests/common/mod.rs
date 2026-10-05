//! Shared configuration and browser-flow helpers for public integration tests.

#[allow(dead_code)]
pub mod browser_flow;

use chrome_for_testing_manager::ChromeForTestingConfig;
use std::path::PathBuf;

/// Build an integration-test config using the shared integration-test cache.
#[allow(dead_code)]
pub fn chrome_config() -> ChromeForTestingConfig {
    ChromeForTestingConfig::builder()
        .cache_dir(cache_dir())
        .build()
}

/// The cache shared by all integration tests, under Cargo's per-target temporary directory (which
/// honors `CARGO_TARGET_DIR`). Concurrent tests coordinate through the cache's file locks.
pub fn cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("integration-test-cache")
        .join("shared")
}
