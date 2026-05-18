//! [`EngineId`] — stable identifier for a registered engine backend.

use serde::{Deserialize, Serialize};

/// Stable identifier for a registered engine backend. Examples: `"cef"`,
/// `"webkit"`, `"blitz"`. Lower-case, snake-case, ASCII-only by
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn assert_serialize<T: serde::Serialize>() {}
    fn assert_deserialize<T: for<'de> serde::Deserialize<'de>>() {}

    #[test]
    fn engine_id_serde_traits_implemented() {
        // Compile-time proof that EngineId implements Serialize + Deserialize.
        assert_serialize::<EngineId>();
        assert_deserialize::<EngineId>();
    }

    #[test]
    fn engine_id_display_matches_inner() {
        let id = EngineId::new("cef");
        assert_eq!(format!("{id}"), "cef");
        assert_eq!(id.as_str(), "cef");
    }

    #[test]
    fn engine_id_from_str_constructs_correctly() {
        let id: EngineId = "webkit".into();
        assert_eq!(id.as_str(), "webkit");

        let id2 = EngineId::from("webkit");
        assert_eq!(id, id2);
    }

    #[test]
    fn engine_id_from_string_constructs_correctly() {
        let id = EngineId::from("blink".to_string());
        assert_eq!(id.as_str(), "blink");
    }

    #[test]
    fn engine_id_hash_eq_consistent() {
        let a = EngineId::new("cef");
        let b = EngineId::new("cef");
        let c = EngineId::new("webkit");

        let mut set = HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&b), "equal ids must hash the same");
        assert!(!set.contains(&c), "different id must not be present");

        set.insert(c.clone());
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn engine_id_clone_eq() {
        let id = EngineId::new("servo");
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }
}
