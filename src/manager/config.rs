//! Aggregate configuration for the lower-level manager facade.
//!
//! The policy types live in [`crate::policy`]. This module combines them with the cache location
//! for [`crate::ChromeForTestingManager`].

use crate::policy::{LifecyclePolicy, NetworkPolicy};
use std::path::PathBuf;
use typed_builder::TypedBuilder;

/// Configuration of a [`crate::ChromeForTestingManager`], passed to
/// [`crate::ChromeForTestingManager::new_with_config`].
///
/// Every setting has a default. The builder offers these setters:
///
/// - `cache_dir` (or `cache_dir_opt`): the cache root. Defaults to the platform's per-user cache
///   directory.
/// - `network`: the [`NetworkPolicy`] with HTTP deadlines.
/// - `lifecycle`: the [`LifecyclePolicy`] with startup, shutdown, and session-cleanup timing.
#[derive(Debug, Clone, TypedBuilder)]
pub struct ChromeForTestingManagerConfig {
    /// The cache root, or `None` for the platform's per-user cache directory.
    #[builder(default, setter(into, strip_option(fallback = cache_dir_opt)))]
    pub(crate) cache_dir: Option<PathBuf>,

    /// HTTP deadlines.
    #[builder(default)]
    pub(crate) network: NetworkPolicy,

    /// Process and session lifecycle policy.
    #[builder(default)]
    pub(crate) lifecycle: LifecyclePolicy,
}

impl Default for ChromeForTestingManagerConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}
