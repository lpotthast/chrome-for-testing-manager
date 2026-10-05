//! Technical configuration for launching a managed `ChromeDriver` process.

use crate::port::PortRequest;
use typed_builder::TypedBuilder;

/// Configuration for the managed `ChromeDriver` process.
///
/// This is nested under [`crate::ChromeForTestingConfig`] for the high-level API and can also be
/// passed directly to [`crate::ChromeForTestingManager::launch_driver`] by lower-level callers.
///
/// ```
/// use chrome_for_testing_manager::ChromeDriverConfig;
///
/// let config = ChromeDriverConfig::builder().port(8080u16).build();
/// ```
#[derive(Debug, Clone, TypedBuilder)]
pub struct ChromeDriverConfig {
    /// The requested `ChromeDriver` port.
    ///
    /// Accepts anything implementing `Into<PortRequest>`, including a bare `u16` and [`crate::Port`].
    #[builder(default = PortRequest::Any, setter(into))]
    port: PortRequest,
}

impl Default for ChromeDriverConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ChromeDriverConfig {
    pub(crate) const fn port(&self) -> PortRequest {
        self.port
    }
}

#[cfg(test)]
mod tests {
    use super::ChromeDriverConfig;
    use crate::PortRequest;
    use assertr::prelude::*;

    #[test]
    fn port_zero_requests_os_assignment() {
        let config = ChromeDriverConfig::builder().port(0u16).build();

        assert_that!(config.port()).is_equal_to(PortRequest::Any);
    }
}
