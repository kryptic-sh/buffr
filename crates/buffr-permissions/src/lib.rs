//! SQLite-backed per-origin permissions store for buffr (Phase 5).
//!
//! Phase-5 scope: a pure data layer. No UI, no IPC. Mirrors the
//! [`buffr_zoom`] / [`buffr_history`] / [`buffr_bookmarks`] crate
//! shapes — `Mutex<Connection>`, forward-only migrations, no FTS5.
//!
//! # Decision precedence
//!
//! At prompt-time the CEF handler walks this precedence:
//!
//! 1. Stored `Decision::Allow` for `(origin, capability)` → callback
//!    fires synchronously with `Accept`.
//! 2. Stored `Decision::Deny` for `(origin, capability)` → callback
//!    fires synchronously with `Deny`.
//! 3. No row → enqueue for the UI thread to prompt.
//!
//! `Decision` is intentionally two-valued: there is no "ask every time"
//! state in the store. To reset to "ask", call [`Permissions::forget`]
//! or [`Permissions::forget_origin`].
//!
//! # Schema (v1)
//!
//! See [`schema`]. `(origin, capability)` is the primary key.
//!
//! # Capability mapping
//!
//! [`Capability`] mirrors the subset of `cef_permission_request_types_t`
//! that we surface to the user. Bits we don't have a named variant for
//! land in `Capability::Other(bit)`. The CEF handler is responsible for
//! splitting a bitmask into one row per bit before consulting the
//! store.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace;

pub mod schema;

/// Errors surfaced from [`Permissions`].
#[derive(Debug, Error)]
pub enum PermError {
    #[error("opening sqlite database failed")]
    Open {
        #[source]
        source: rusqlite::Error,
    },
    #[error("applying migration v{version} failed")]
    Migrate {
        #[source]
        source: rusqlite::Error,
        version: i64,
    },
    #[error("query failed")]
    Query {
        #[from]
        source: rusqlite::Error,
    },
    #[error("permissions mutex poisoned")]
    Poisoned,
    #[error("unrecognised decision {decision:?} in row")]
    UnknownDecision { decision: String },
    #[error("unrecognised capability {capability:?} in row")]
    UnknownCapability { capability: String },
}

/// Capability surface buffr exposes to the user. Mirrors a subset of
/// `cef_permission_request_types_t`; bits with no named variant land in
/// [`Capability::Other`].
///
/// `Other(bit)` carries the **single-bit** value (e.g. `2048` for
/// idle-detection). It is never a composite mask — the handler splits
/// composite masks before calling into the store.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Camera,
    Microphone,
    Geolocation,
    Notifications,
    Clipboard,
    Midi,
    /// Anything else CEF surfaces. The `u32` is a single-bit value
    /// matching `cef_permission_request_types_t`. Stored on disk as
    /// `other:<n>` so SQL queries on the table remain readable.
    Other(u32),
}

impl Capability {
    /// Stable string used as the SQLite primary-key fragment + the
    /// rendered form in `Display`. We don't use `serde_json` here —
    /// we want round-trip stability without a JSON dep, and the
    /// `Other(u32)` variant needs custom rendering anyway.
    pub fn as_storage_key(&self) -> String {
        match self {
            Capability::Camera => "camera".to_string(),
            Capability::Microphone => "microphone".to_string(),
            Capability::Geolocation => "geolocation".to_string(),
            Capability::Notifications => "notifications".to_string(),
            Capability::Clipboard => "clipboard".to_string(),
            Capability::Midi => "midi".to_string(),
            Capability::Other(bit) => format!("other:{bit}"),
        }
    }

    /// Inverse of [`Self::as_storage_key`]. Returns
    /// [`PermError::UnknownCapability`] for malformed input.
    pub fn from_storage_key(s: &str) -> Result<Self, PermError> {
        Ok(match s {
            "camera" => Capability::Camera,
            "microphone" => Capability::Microphone,
            "geolocation" => Capability::Geolocation,
            "notifications" => Capability::Notifications,
            "clipboard" => Capability::Clipboard,
            "midi" => Capability::Midi,
            other => {
                if let Some(rest) = other.strip_prefix("other:") {
                    let bit: u32 =
                        rest.parse()
                            .map_err(|_| PermError::UnknownCapability {
                                capability: other.to_string(),
                            })?;
                    Capability::Other(bit)
                } else {
                    return Err(PermError::UnknownCapability {
                        capability: other.to_string(),
                    });
                }
            }
        })
    }

    /// Human-readable label for the prompt UI. Plural-friendly so the
    /// list joins as "camera, microphone".
    pub fn human_label(&self) -> String {
        match self {
            Capability::Camera => "camera".to_string(),
            Capability::Microphone => "microphone".to_string(),
            Capability::Geolocation => "geolocation".to_string(),
            Capability::Notifications => "notifications".to_string(),
            Capability::Clipboard => "clipboard".to_string(),
            Capability::Midi => "midi".to_string(),
            Capability::Other(bit) => format!("other (bit {bit})"),
        }
    }
}

/// Stored decision. Two-valued: a present row means a sticky decision,
/// an absent row means "ask the user".
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
        }
    }

    fn from_str(s: &str) -> Result<Self, PermError> {
        match s {
            "allow" => Ok(Decision::Allow),
            "deny" => Ok(Decision::Deny),
            other => Err(PermError::UnknownDecision {
                decision: other.to_string(),
            }),
        }
    }
}

/// One row in the permissions table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRow {
    pub origin: String,
    pub capability: Capability,
    pub decision: Decision,
    /// Wall-clock unix-epoch seconds at which the row was last
    /// inserted / updated. Useful for "show recent decisions" UIs.
    pub set_at: i64,
}

/// SQLite-backed per-origin permissions store.
pub struct Permissions {
    conn: Mutex<Connection>,
}

impl Permissions {
    /// Open or create the SQLite database at `path` and run any
    /// pending schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PermError> {
        let mut conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|source| PermError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database — for tests and short-lived ephemeral
    /// profiles (private windows).
    pub fn open_in_memory() -> Result<Self, PermError> {
        let mut conn = Connection::open_in_memory().map_err(|source| PermError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn tune(conn: &Connection) -> Result<(), PermError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| PermError::Open { source })?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| PermError::Open { source })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| PermError::Open { source })?;
        Ok(())
    }

    /// Look up the stored decision for `(origin, capability)`. `None`
    /// means "not yet decided — prompt the user".
    pub fn get(
        &self,
        origin: &str,
        capability: Capability,
    ) -> Result<Option<Decision>, PermError> {
        let conn = self.conn.lock().map_err(|_| PermError::Poisoned)?;
        let cap_key = capability.as_storage_key();
        let dec: Option<String> = conn
            .query_row(
                "SELECT decision FROM permissions WHERE origin = ?1 AND capability = ?2",
                params![origin, cap_key],
                |row| row.get(0),
            )
            .optional()?;
        match dec {
            None => Ok(None),
            Some(s) => Ok(Some(Decision::from_str(&s)?)),
        }
    }

    /// Insert or update the stored decision. Bumps `set_at`.
    pub fn set(
        &self,
        origin: &str,
        capability: Capability,
        decision: Decision,
    ) -> Result<(), PermError> {
        let now = current_unix_time();
        let conn = self.conn.lock().map_err(|_| PermError::Poisoned)?;
        let cap_key = capability.as_storage_key();
        conn.execute(
            "INSERT INTO permissions (origin, capability, decision, set_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(origin, capability) DO UPDATE SET \
               decision = excluded.decision, set_at = excluded.set_at",
            params![origin, cap_key, decision.as_str(), now],
        )?;
        trace!(origin, capability = ?capability, decision = ?decision, "permissions: set");
        Ok(())
    }

    /// Drop the stored decision for `(origin, capability)`. Returns
    /// `true` iff a row was deleted.
    pub fn forget(
        &self,
        origin: &str,
        capability: Capability,
    ) -> Result<bool, PermError> {
        let conn = self.conn.lock().map_err(|_| PermError::Poisoned)?;
        let cap_key = capability.as_storage_key();
        let n = conn.execute(
            "DELETE FROM permissions WHERE origin = ?1 AND capability = ?2",
            params![origin, cap_key],
        )?;
        Ok(n > 0)
    }

    /// Drop every stored decision for `origin`. Returns the number of
    /// rows deleted.
    pub fn forget_origin(&self, origin: &str) -> Result<usize, PermError> {
        let conn = self.conn.lock().map_err(|_| PermError::Poisoned)?;
        let n = conn.execute(
            "DELETE FROM permissions WHERE origin = ?1",
            params![origin],
        )?;
        Ok(n)
    }

    /// Snapshot of every row, ordered most-recently-set first then by
    /// origin / capability for stable test output.
    pub fn all(&self) -> Result<Vec<PermissionRow>, PermError> {
        let conn = self.conn.lock().map_err(|_| PermError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT origin, capability, decision, set_at \
             FROM permissions \
             ORDER BY set_at DESC, origin ASC, capability ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (origin, cap_key, dec_str, set_at) in rows {
            let capability = Capability::from_storage_key(&cap_key)?;
            let decision = Decision::from_str(&dec_str)?;
            out.push(PermissionRow {
                origin,
                capability,
                decision,
                set_at,
            });
        }
        Ok(out)
    }

    /// Wipe every row. Returns the count.
    pub fn clear(&self) -> Result<usize, PermError> {
        let conn = self.conn.lock().map_err(|_| PermError::Poisoned)?;
        let n = conn.execute("DELETE FROM permissions", [])?;
        Ok(n)
    }
}

fn current_unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_runs_migrations() {
        let p = Permissions::open_in_memory().unwrap();
        assert!(p.all().unwrap().is_empty());
        assert_eq!(schema::latest_version(), 1);
    }

    #[test]
    fn set_then_get_round_trip() {
        let p = Permissions::open_in_memory().unwrap();
        p.set("https://a.example", Capability::Camera, Decision::Allow)
            .unwrap();
        let d = p.get("https://a.example", Capability::Camera).unwrap();
        assert_eq!(d, Some(Decision::Allow));
    }

    #[test]
    fn get_unknown_returns_none() {
        let p = Permissions::open_in_memory().unwrap();
        assert!(
            p.get("https://nope.example", Capability::Camera)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn set_twice_second_wins() {
        let p = Permissions::open_in_memory().unwrap();
        p.set("https://a.example", Capability::Camera, Decision::Allow)
            .unwrap();
        p.set("https://a.example", Capability::Camera, Decision::Deny)
            .unwrap();
        assert_eq!(
            p.get("https://a.example", Capability::Camera).unwrap(),
            Some(Decision::Deny)
        );
        assert_eq!(p.all().unwrap().len(), 1);
    }

    #[test]
    fn forget_existing_returns_true_missing_returns_false() {
        let p = Permissions::open_in_memory().unwrap();
        p.set(
            "https://a.example",
            Capability::Microphone,
            Decision::Allow,
        )
        .unwrap();
        assert!(p.forget("https://a.example", Capability::Microphone).unwrap());
        assert!(!p.forget("https://a.example", Capability::Microphone).unwrap());
        assert!(
            p.get("https://a.example", Capability::Microphone)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forget_origin_removes_all_capabilities() {
        let p = Permissions::open_in_memory().unwrap();
        p.set("https://a.example", Capability::Camera, Decision::Allow)
            .unwrap();
        p.set(
            "https://a.example",
            Capability::Microphone,
            Decision::Allow,
        )
        .unwrap();
        p.set(
            "https://b.example",
            Capability::Notifications,
            Decision::Deny,
        )
        .unwrap();
        let n = p.forget_origin("https://a.example").unwrap();
        assert_eq!(n, 2);
        // Other origin untouched.
        assert_eq!(
            p.get("https://b.example", Capability::Notifications)
                .unwrap(),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn clear_returns_count() {
        let p = Permissions::open_in_memory().unwrap();
        p.set("https://a", Capability::Camera, Decision::Allow)
            .unwrap();
        p.set("https://b", Capability::Geolocation, Decision::Deny)
            .unwrap();
        assert_eq!(p.clear().unwrap(), 2);
        assert!(p.all().unwrap().is_empty());
    }

    #[test]
    fn capability_storage_key_round_trip() {
        for cap in [
            Capability::Camera,
            Capability::Microphone,
            Capability::Geolocation,
            Capability::Notifications,
            Capability::Clipboard,
            Capability::Midi,
            Capability::Other(2048),
        ] {
            let key = cap.as_storage_key();
            let back = Capability::from_storage_key(&key).unwrap();
            assert_eq!(back, cap);
        }
    }

    #[test]
    fn capability_from_unknown_string_errors() {
        let err = Capability::from_storage_key("not_a_thing").unwrap_err();
        match err {
            PermError::UnknownCapability { capability } => {
                assert_eq!(capability, "not_a_thing");
            }
            other => panic!("expected UnknownCapability, got {other:?}"),
        }
    }

    #[test]
    fn capability_other_must_have_numeric_suffix() {
        assert!(Capability::from_storage_key("other:abc").is_err());
        assert!(Capability::from_storage_key("other:").is_err());
        assert!(Capability::from_storage_key("other:42").is_ok());
    }

    #[test]
    fn all_returns_set_at_desc_with_stable_secondary_order() {
        let p = Permissions::open_in_memory().unwrap();
        p.set("https://a", Capability::Camera, Decision::Allow)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        p.set("https://b", Capability::Microphone, Decision::Deny)
            .unwrap();
        let all = p.all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].origin, "https://b");
        assert_eq!(all[1].origin, "https://a");
    }

    #[test]
    fn other_capability_round_trip_through_store() {
        let p = Permissions::open_in_memory().unwrap();
        p.set(
            "https://a.example",
            Capability::Other(2048),
            Decision::Deny,
        )
        .unwrap();
        let row = p
            .get("https://a.example", Capability::Other(2048))
            .unwrap();
        assert_eq!(row, Some(Decision::Deny));
    }

    #[test]
    fn decision_serialises_snake_case() {
        // Confirms `serde(rename_all = "snake_case")` so the on-disk
        // string matches the constant in Decision::as_str.
        let json = serde_json::to_string(&Decision::Allow).unwrap();
        assert_eq!(json, "\"allow\"");
        let json = serde_json::to_string(&Decision::Deny).unwrap();
        assert_eq!(json, "\"deny\"");
    }

    #[test]
    fn capability_human_label_distinguishes_other() {
        assert_eq!(Capability::Camera.human_label(), "camera");
        assert!(Capability::Other(8192).human_label().contains("8192"));
    }
}
