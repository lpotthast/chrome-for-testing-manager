use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port(pub u16);

impl From<u16> for Port {
    fn from(value: u16) -> Self {
        Self(value)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortRequest {
    Any,
    Specific(Port),
}
