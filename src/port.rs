//! Validated port values and fixed-or-OS-assigned port requests.

use std::fmt::{Display, Formatter};
use std::num::NonZeroU16;

/// A nonzero TCP port bound (or to be bound) by a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port(NonZeroU16);

impl Port {
    /// Create a typed nonzero port from a raw `u16`.
    ///
    /// # Panics
    ///
    /// Panics when `value` is zero. Use [`Self::try_new`] when the value is not already known to
    /// be nonzero.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        match NonZeroU16::new(value) {
            Some(port) => Self(port),
            None => panic!("a bound TCP port must be nonzero"),
        }
    }

    /// Create a typed port when `value` is nonzero.
    #[must_use]
    pub const fn try_new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(port) => Some(Self(port)),
            None => None,
        }
    }

    /// Return the raw `u16` port value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0.get()
    }
}

impl Display for Port {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// How a process should pick the port it listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortRequest {
    /// Let the OS assign an unused port.
    Any,

    /// Bind to a specific port.
    Specific(Port),
}

impl From<u16> for PortRequest {
    fn from(value: u16) -> Self {
        Port::try_new(value).map_or(Self::Any, Self::Specific)
    }
}

impl From<Port> for PortRequest {
    fn from(value: Port) -> Self {
        Self::Specific(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;

    #[test]
    fn port_request_from_u16_constructs_specific_port() {
        assert_that!(PortRequest::from(8080u16))
            .is_equal_to(PortRequest::Specific(Port::new(8080)));
    }

    #[test]
    fn zero_u16_requests_an_os_assigned_port() {
        assert_that!(PortRequest::from(0u16)).is_equal_to(PortRequest::Any);
    }

    #[test]
    fn try_new_rejects_zero() {
        assert_that!(Port::try_new(0)).is_none();
    }

    #[test]
    #[should_panic(expected = "a bound TCP port must be nonzero")]
    fn new_rejects_zero() {
        let _ = Port::new(0);
    }
}
