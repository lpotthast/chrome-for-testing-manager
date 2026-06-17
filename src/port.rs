use std::fmt::{Display, Formatter};

/// A TCP port bound (or to be bound) by a chromedriver process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port(u16);

impl Port {
    /// Create a typed port from a raw `u16`.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Return the raw `u16` port value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

impl From<u16> for Port {
    fn from(value: u16) -> Self {
        Self::new(value)
    }
}

impl AsRef<u16> for Port {
    fn as_ref(&self) -> &u16 {
        &self.0
    }
}

impl Display for Port {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// How chromedriver should pick the port it listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortRequest {
    /// Let the OS assign an unused port.
    Any,

    /// Bind to a specific port.
    Specific(Port),
}

impl From<u16> for PortRequest {
    fn from(value: u16) -> Self {
        Self::Specific(Port::new(value))
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
    fn port_from_u16_constructs_typed_port() {
        assert_that!(Port::from(8080u16)).is_equal_to(Port::new(8080));
        assert_that!(Port::from(8080u16).as_u16()).is_equal_to(8080);
    }

    #[test]
    fn port_request_from_u16_constructs_specific_port() {
        assert_that!(PortRequest::from(8080u16))
            .is_equal_to(PortRequest::Specific(Port::new(8080)));
    }
}
