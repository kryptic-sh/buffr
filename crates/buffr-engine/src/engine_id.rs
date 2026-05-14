//! [`EngineId`] — stable identifier for a registered engine backend.

use serde::{Deserialize, Serialize};

/// Stable identifier for a registered engine backend. Examples: `"cef"`,
/// `"webkit"`, `"blink-cdp"`. Lower-case, snake-case, ASCII-only by
/// convention (not enforced — the router compares ids by equality, so
/// whatever string was registered is what must be referenced in config).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EngineId(String);

impl EngineId {
    /// Construct an `EngineId` from any `Into<String>` value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EngineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for EngineId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for EngineId {
    fn from(s: String) -> Self {
        Self(s)
    }
}
