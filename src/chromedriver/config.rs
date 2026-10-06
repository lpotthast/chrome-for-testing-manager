//! Technical configuration for launching a managed `ChromeDriver` process.

use crate::port::PortRequest;
use chrome_for_testing::chromedriver::LogLevel;
use typed_builder::TypedBuilder;

/// Verbosity of the managed `ChromeDriver` process's own log output.
///
/// The log is part of the driver output observed through
/// [`crate::ChromeForTesting::subscribe_output`] and attached to startup errors. Disabling it
/// entirely is not offered, because `ChromeDriver` then no longer reports the port it listens on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChromeDriverLogLevel {
    /// Log all messages.
    All,

    /// Log debug messages and above.
    Debug,

    /// Log info messages and above, including every `WebDriver` command and response.
    #[default]
    Info,

    /// Log warnings and above.
    Warning,

    /// Log severe errors only.
    Severe,
}

impl ChromeDriverLogLevel {
    const fn as_log_level(self) -> LogLevel {
        match self {
            Self::All => LogLevel::All,
            Self::Debug => LogLevel::Debug,
            Self::Info => LogLevel::Info,
            Self::Warning => LogLevel::Warning,
            Self::Severe => LogLevel::Severe,
        }
    }
}

/// Configuration for the managed `ChromeDriver` process.
///
/// This is nested under [`crate::ChromeForTestingConfig`] for the high-level API and can also be
/// passed directly to [`crate::ChromeForTestingManager::launch_driver`] by lower-level callers.
/// The builder offers these setters:
///
/// - `port`: the port `ChromeDriver` listens on. Accepts anything implementing
///   `Into<PortRequest>`, such as a `u16` or a [`crate::Port`]. Defaults to
///   [`PortRequest::Any`], an OS-assigned port, which `0u16` requests as well.
/// - `log_level`: the verbosity of `ChromeDriver`'s own log output. Defaults to
///   [`ChromeDriverLogLevel::Info`].
///
/// ```
/// use chrome_for_testing_manager::ChromeDriverConfig;
///
/// let config = ChromeDriverConfig::builder().port(8080u16).build();
/// ```
#[derive(Debug, Clone, TypedBuilder)]
pub struct ChromeDriverConfig {
    /// The requested `ChromeDriver` port.
    #[builder(default = PortRequest::Any, setter(into))]
    port: PortRequest,

    /// The verbosity of `ChromeDriver`'s log output.
    #[builder(default)]
    log_level: ChromeDriverLogLevel,
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

    pub(crate) const fn log_level(&self) -> LogLevel {
        self.log_level.as_log_level()
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
