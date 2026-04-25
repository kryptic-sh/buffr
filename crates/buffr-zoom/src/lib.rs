//! SQLite-backed per-site zoom-level store for buffr (Phase 5).
//!
//! Phase-5 scope: a pure data layer. No UI, no IPC. Mirrors the
//! [`buffr_history`] / [`buffr_bookmarks`] / [`buffr_downloads`] crate
//! shapes — `Mutex<Connection>`, forward-only migrations, no FTS5.
//!
//! # Schema (v1)
//!
//! One `zoom` table; see [`schema`]. `domain` is the primary key —
//! exactly one zoom level per host.
//!
//! # Domain extraction
//!
//! Callers should funnel URLs through [`domain_for_url`] before
//! calling [`ZoomStore::get`] / [`ZoomStore::set`]:
//!
//! - `https://example.com/foo` → `example.com`
//! - `http://EXAMPLE.com:8080/` → `example.com:8080` (port preserved
//!   because zoom is per-origin, not per-eTLD+1)
//! - `about:blank`, `data:text/html,...`, anything without a host →
//!   the global key [`GLOBAL_KEY`] = `"_global_"`.
//! - `file:///foo` → `_global_`. We deliberately collapse local files
//!   to a single key so a project tree shares one zoom level.
//!
//! # Zoom-level semantics
//!
//! CEF's `set_zoom_level(0.0)` resets to the default. We follow that:
//! `get` returns `0.0` for unset domains, and a stored `0.0` round-trips
//! identically. The `LoadHandler` skip path checks `level == 0.0` to
//! avoid a redundant CEF call on unzoomed domains.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;
use tracing::trace;

pub mod schema;

/// Sentinel domain key used when a URL has no host (about:, data:,
/// file:). Listed first so anyone reading the crate sees it without
/// having to grep.
pub const GLOBAL_KEY: &str = "_global_";

/// Errors surfaced from [`ZoomStore`].
#[derive(Debug, Error)]
pub enum ZoomError {
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
    #[error("zoom mutex poisoned")]
    Poisoned,
}

/// SQLite-backed per-site zoom level store.
pub struct ZoomStore {
    conn: Mutex<Connection>,
}

impl ZoomStore {
    /// Open or create the SQLite database at `path` and run any
    /// pending schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ZoomError> {
        let mut conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|source| ZoomError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database — for tests and short-lived ephemeral
    /// profiles (private windows).
    pub fn open_in_memory() -> Result<Self, ZoomError> {
        let mut conn = Connection::open_in_memory().map_err(|source| ZoomError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Apply per-connection pragmas. Same shape as the other stores.
    fn tune(conn: &Connection) -> Result<(), ZoomError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| ZoomError::Open { source })?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| ZoomError::Open { source })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| ZoomError::Open { source })?;
        Ok(())
    }

    /// Look up the zoom level for `domain`. Returns `0.0` for any
    /// unrecorded domain — that matches CEF's default and lets the
    /// caller skip a redundant `set_zoom_level(0.0)` round-trip.
    pub fn get(&self, domain: &str) -> Result<f64, ZoomError> {
        let conn = self.conn.lock().map_err(|_| ZoomError::Poisoned)?;
        let level: Option<f64> = conn
            .query_row(
                "SELECT level FROM zoom WHERE domain = ?1",
                params![domain],
                |row| row.get(0),
            )
            .optional()?;
        Ok(level.unwrap_or(0.0))
    }

    /// Insert or update the zoom level for `domain`. Bumps `set_at` so
    /// `all()` can sort most-recent-first.
    pub fn set(&self, domain: &str, level: f64) -> Result<(), ZoomError> {
        let now = current_unix_time();
        let conn = self.conn.lock().map_err(|_| ZoomError::Poisoned)?;
        conn.execute(
            "INSERT INTO zoom (domain, level, set_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(domain) DO UPDATE SET level = excluded.level, set_at = excluded.set_at",
            params![domain, level, now],
        )?;
        trace!(domain, level, "zoom: set");
        Ok(())
    }

    /// Remove the row for `domain`. Returns `true` iff a row was
    /// deleted. Used by `ZoomReset`: the page-action dispatcher resets
    /// the live zoom and clears the persisted override.
    pub fn remove(&self, domain: &str) -> Result<bool, ZoomError> {
        let conn = self.conn.lock().map_err(|_| ZoomError::Poisoned)?;
        let n = conn.execute("DELETE FROM zoom WHERE domain = ?1", params![domain])?;
        Ok(n > 0)
    }

    /// Every (domain, level) pair, ordered by most-recently-set first.
    pub fn all(&self) -> Result<Vec<(String, f64)>, ZoomError> {
        let conn = self.conn.lock().map_err(|_| ZoomError::Poisoned)?;
        let mut stmt =
            conn.prepare("SELECT domain, level FROM zoom ORDER BY set_at DESC, domain ASC")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete every row. Returns the number deleted. Mirrors the other
    /// stores' `clear_all` shape — used by `--clear-zoom` and by the
    /// `[privacy] clear_on_exit` shutdown hook (zoom is treated as
    /// part of `LocalStorage` for now; explicit `--clear-zoom` is the
    /// user-facing knob).
    pub fn clear(&self) -> Result<usize, ZoomError> {
        let conn = self.conn.lock().map_err(|_| ZoomError::Poisoned)?;
        let n = conn.execute("DELETE FROM zoom", [])?;
        Ok(n)
    }
}

/// Extract the canonical zoom-key for a URL.
///
/// - URLs with a host → `host[:port]` lowercased.
/// - URLs without a host (about:, data:, file:, javascript:) and any
///   parse error → [`GLOBAL_KEY`].
///
/// Defined as a free function so the runtime + tests share one
/// canonicalisation path. Callers in `buffr-core` use this directly
/// rather than re-parsing.
pub fn domain_for_url(url: &str) -> String {
    let parsed = match url::Url::parse(url.trim()) {
        Ok(u) => u,
        Err(_) => return GLOBAL_KEY.to_string(),
    };
    let Some(host) = parsed.host_str() else {
        return GLOBAL_KEY.to_string();
    };
    let host_lower = host.to_ascii_lowercase();
    match parsed.port() {
        Some(p) => format!("{host_lower}:{p}"),
        None => host_lower,
    }
}

/// Wall-clock unix-epoch seconds. Same shape as the other stores'
/// `Utc::now().timestamp()` — we don't pull `chrono` in for one call.
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
        let z = ZoomStore::open_in_memory().unwrap();
        assert!(z.all().unwrap().is_empty());
        assert_eq!(schema::latest_version(), 1);
    }

    #[test]
    fn set_then_get_round_trip() {
        let z = ZoomStore::open_in_memory().unwrap();
        z.set("example.com", 1.5).unwrap();
        let level = z.get("example.com").unwrap();
        assert!((level - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn get_unknown_returns_zero() {
        let z = ZoomStore::open_in_memory().unwrap();
        let level = z.get("nope.example").unwrap();
        assert_eq!(level, 0.0);
    }

    #[test]
    fn set_twice_second_wins() {
        let z = ZoomStore::open_in_memory().unwrap();
        z.set("example.com", 0.5).unwrap();
        z.set("example.com", -0.25).unwrap();
        let level = z.get("example.com").unwrap();
        assert!((level - -0.25).abs() < f64::EPSILON);
        // Still one row.
        assert_eq!(z.all().unwrap().len(), 1);
    }

    #[test]
    fn remove_existing_returns_true_missing_returns_false() {
        let z = ZoomStore::open_in_memory().unwrap();
        z.set("example.com", 1.0).unwrap();
        assert!(z.remove("example.com").unwrap());
        assert!(!z.remove("example.com").unwrap());
        assert_eq!(z.get("example.com").unwrap(), 0.0);
    }

    #[test]
    fn all_returns_set_at_desc() {
        let z = ZoomStore::open_in_memory().unwrap();
        // Three writes; sleep just enough that set_at differs across
        // rows. Resolution is seconds — too fast and ordering ties.
        z.set("a.example", 0.5).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        z.set("b.example", 1.0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        z.set("c.example", 1.5).unwrap();
        let all = z.all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "c.example");
        assert_eq!(all[1].0, "b.example");
        assert_eq!(all[2].0, "a.example");
    }

    #[test]
    fn clear_wipes_everything() {
        let z = ZoomStore::open_in_memory().unwrap();
        z.set("a.example", 0.5).unwrap();
        z.set("b.example", 1.0).unwrap();
        let n = z.clear().unwrap();
        assert_eq!(n, 2);
        assert!(z.all().unwrap().is_empty());
    }

    #[test]
    fn domain_for_url_extracts_host_lowercase() {
        assert_eq!(domain_for_url("https://Example.COM/foo"), "example.com");
        assert_eq!(
            domain_for_url("http://example.com:8080/"),
            "example.com:8080"
        );
        assert_eq!(
            domain_for_url("https://sub.example.com/"),
            "sub.example.com"
        );
    }

    #[test]
    fn domain_for_url_falls_back_to_global() {
        assert_eq!(domain_for_url("about:blank"), GLOBAL_KEY);
        assert_eq!(domain_for_url("data:text/plain,hi"), GLOBAL_KEY);
        assert_eq!(domain_for_url("file:///tmp/foo.html"), GLOBAL_KEY);
        assert_eq!(domain_for_url("javascript:void(0)"), GLOBAL_KEY);
        assert_eq!(domain_for_url("not a url"), GLOBAL_KEY);
        assert_eq!(domain_for_url(""), GLOBAL_KEY);
    }

    #[test]
    fn zero_level_stored_and_returned() {
        let z = ZoomStore::open_in_memory().unwrap();
        z.set("example.com", 0.0).unwrap();
        // 0.0 is the CEF default; storing it is allowed and round-trips
        // identically to "not set" from get()'s perspective.
        assert_eq!(z.get("example.com").unwrap(), 0.0);
    }
}
