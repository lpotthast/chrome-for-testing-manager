//! Aggregate configuration for the lower-level manager facade.
//!
//! Shared policy types live independently in [`crate::policy`]; this module combines them with
//! cache-location selection for [`crate::ChromeForTestingManager`].

use crate::policy::{LifecyclePolicy, NetworkPolicy};
use std::path::PathBuf;
use typed_builder::TypedBuilder;

/// Focused configuration for resolver, artifact-store, process, and session services.
#[derive(Debug, Clone, TypedBuilder)]
pub struct ChromeForTestingManagerConfig {
    /// Optional cache directory. The platform-specific per-user cache is used when absent.
    #[builder(default, setter(strip_option(fallback = cache_dir_opt)))]
    cache_dir: Option<PathBuf>,

    /// HTTP policy shared by networked services.
    #[builder(default)]
    network: NetworkPolicy,

    /// Process and session lifecycle policy.
    #[builder(default)]
    lifecycle: LifecyclePolicy,
}

impl Default for ChromeForTestingManagerConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ChromeForTestingManagerConfig {
    pub(crate) fn into_parts(self) -> (Option<PathBuf>, NetworkPolicy, LifecyclePolicy) {
        (self.cache_dir, self.network, self.lifecycle)
    }
}
