//! SQLite-backed browsing history for buffr (Phase 5).
//!
//! Phase-5 scope: a pure data layer. No UI, no IPC, no FTS5 yet.
//! Visits land here via a CEF `LoadHandler` wired up in
//! `buffr-core::handlers`; later phases query `search` / `recent`
//! from the omnibar (Phase 3 chrome) and from a `buffr query history`
//! CLI (post-1.0).
//!
//! # Schema (v1)
//!
//! One `visits` table, one row per visit. Columns: `id`, `url`,
//! `title`, `visit_time` (unix epoch seconds), `transition`. Indexes
//! cover lookup-by-url, recent-first ordering, and the
//! `(url, visit_time)` pair the dedupe path needs. Migrations are
//! forward-only; see [`schema`].
//!
//! # Frecency
//!
//! [`History::search`] ranks by a simple `visit_count * 2 + recency_bonus`
//! formula — `recency_bonus = 10` if the most recent visit is within
//! the last 7 days, else `0`. Substring match (`LIKE %q%`) over both
//! `url` and `title`. Good enough for v1; Phase 5b lands FTS5 with a
//! migration. Documented in `crates/buffr-history/README.md`.
//!
//! # Concurrency
//!
//! [`History`] wraps `Mutex<rusqlite::Connection>`. All public methods
//! take `&self` and acquire the lock per call. CEF callbacks are
//! short, lock contention won't matter at human typing rates. If the
//! omnibar ever needs to query mid-keystroke we'll switch to a
//! connection pool (`r2d2_sqlite`) — not now.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, trace};

pub mod schema;

/// Schemes we never record. `about:` covers `about:blank` /
/// `about:srcdoc`; `cef:` and `chrome:` cover internal browser UI;
/// `data:` and `file:` are noisy and privacy-sensitive. Phase 4 will
/// surface this list as a config knob.
const SKIP_SCHEMES: &[&str] = &["about", "cef", "chrome", "data", "file"];

/// Dedupe window. If a row exists for the same canonical URL whose
/// `visit_time` is within this many seconds of `now`, we update that
/// row in place rather than inserting. Prevents reload spam from
/// blowing up the table.
const DEDUPE_WINDOW_SECS: i64 = 60;

/// Recent-bonus window for the frecency formula. Visits inside this
/// window get `+10` added to their score so a yesterday-visited page
/// outranks a once-visited-six-months-ago page even with equal visit
/// counts.
const RECENCY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// One row of the `visits` table, decoded into Rust types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub visit_time: DateTime<Utc>,
    pub transition: Transition,
}

/// What kind of navigation produced a visit. Stored as a string so
/// adding variants doesn't require a schema migration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    /// User clicked a link or typed a URL into the omnibar.
    Link,
    /// Explicit page reload (`r`, `<C-r>`, etc.).
    Reload,
    /// Form submission landed on this URL.
    FormSubmit,
    /// Omnibar autocomplete pick.
    Generated,
    /// Anything else / unclassified.
    Other,
}

impl Transition {
    fn as_str(self) -> &'static str {
        match self {
            Transition::Link => "link",
            Transition::Reload => "reload",
            Transition::FormSubmit => "form_submit",
            Transition::Generated => "generated",
            Transition::Other => "other",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "link" => Transition::Link,
            "reload" => Transition::Reload,
            "form_submit" => Transition::FormSubmit,
            "generated" => Transition::Generated,
            _ => Transition::Other,
        }
    }
}

/// Errors surfaced from [`History`]. `rusqlite::Error` is wrapped via
/// `#[from]` so the public surface stays small — callers don't need
/// to depend on `rusqlite` directly to handle errors.
#[derive(Debug, Error)]
pub enum HistoryError {
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
    #[error("invalid url")]
    Url {
        #[from]
        source: url::ParseError,
    },
    #[error("history mutex poisoned")]
    Poisoned,
}

/// Trait used to inject "now" into [`History`]. The default impl reads
/// the system clock; tests substitute a [`MockClock`] so dedupe-window
/// boundary tests don't need to actually sleep.
pub trait Clock: Send + Sync + 'static {
    /// Current wall-clock instant in UTC. Resolution: seconds (we
    /// store unix-epoch seconds, not millis).
    fn now(&self) -> DateTime<Utc>;
}

/// System clock — `chrono::Utc::now()`. Default in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Manually-driven clock for tests. Construct via
/// [`MockClock::new`], advance with [`MockClock::advance`]. Implements
/// [`Clock`] through an internal `Arc<Mutex<DateTime<Utc>>>` so the
/// test harness can keep one handle for `advance` while a second
/// handle (sharing the same timeline) lives inside [`History`].
#[derive(Debug, Clone)]
pub struct MockClock {
    inner: Arc<Mutex<DateTime<Utc>>>,
}

impl MockClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(start)),
        }
    }

    pub fn advance(&self, secs: i64) {
        if let Ok(mut t) = self.inner.lock() {
            *t += chrono::Duration::seconds(secs);
        }
    }
}

impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        self.inner.lock().map(|t| *t).unwrap_or_else(|_| Utc::now())
    }
}

/// SQLite-backed history store.
pub struct History {
    conn: Mutex<Connection>,
    clock: Box<dyn Clock>,
}

impl History {
    /// Open or create the SQLite database at `path` and run any
    /// pending schema migrations. Uses [`SystemClock`] for time.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HistoryError> {
        Self::open_with_clock(path, Box::new(SystemClock))
    }

    /// Open or create the database with a custom [`Clock`] impl. Used
    /// by tests to drive the dedupe window deterministically.
    pub fn open_with_clock(
        path: impl AsRef<Path>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, HistoryError> {
        let mut conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|source| HistoryError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// In-memory database — for tests and short-lived ephemeral
    /// profiles (private windows, Phase 5 follow-up).
    pub fn open_in_memory() -> Result<Self, HistoryError> {
        Self::open_in_memory_with_clock(Box::new(SystemClock))
    }

    /// In-memory database with a custom clock.
    pub fn open_in_memory_with_clock(clock: Box<dyn Clock>) -> Result<Self, HistoryError> {
        let mut conn =
            Connection::open_in_memory().map_err(|source| HistoryError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// Apply per-connection pragmas. WAL gives us non-blocking reads
    /// while a writer is active; `synchronous=NORMAL` is safe under
    /// WAL and avoids fsync-per-commit thrash. `foreign_keys=ON` is
    /// belt-and-braces: we don't have FKs in v1 but every future
    /// migration that adds them will Just Work.
    fn tune(conn: &Connection) -> Result<(), HistoryError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| HistoryError::Open { source })?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| HistoryError::Open { source })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| HistoryError::Open { source })?;
        Ok(())
    }

    /// Record a single visit. Performs URL canonicalisation, scheme
    /// filtering, and dedupe.
    ///
    /// - Skip-scheme URLs (`about:`, `chrome:`, `cef:`, `data:`,
    ///   `file:`) return `Ok(())` without inserting.
    /// - Unparseable URLs log at `debug` and return `Ok(())` — we'd
    ///   rather drop the row than poison the call site.
    /// - If a row exists for the canonical URL with `visit_time`
    ///   inside the last [`DEDUPE_WINDOW_SECS`], that row is updated
    ///   in place (new `visit_time`; non-empty `title` overrides).
    ///   Otherwise we insert.
    pub fn record_visit(
        &self,
        url: &str,
        title: Option<&str>,
        transition: Transition,
    ) -> Result<(), HistoryError> {
        let canon = match canonicalise(url) {
            Some(c) => c,
            None => {
                debug!(url, "history: unparseable url; skipping");
                return Ok(());
            }
        };
        if is_skip_scheme(&canon) {
            trace!(url = %canon, "history: skip-scheme; not recording");
            return Ok(());
        }

        let now = self.clock.now().timestamp();
        let cutoff = now - DEDUPE_WINDOW_SECS;
        let title = title.map(str::trim).filter(|s| !s.is_empty());

        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;

        let recent: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT id, title FROM visits \
                 WHERE url = ?1 AND visit_time >= ?2 \
                 ORDER BY visit_time DESC LIMIT 1",
                params![canon, cutoff],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((id, existing_title)) = recent {
            // Dedupe: bump visit_time. Keep existing title unless we
            // got a fresher non-empty one. Single UPDATE; rusqlite
            // does the right thing with `COALESCE`-via-Rust.
            let new_title: Option<String> = title.map(str::to_owned).or(existing_title);
            conn.execute(
                "UPDATE visits SET visit_time = ?1, title = ?2 WHERE id = ?3",
                params![now, new_title, id],
            )?;
        } else {
            conn.execute(
                "INSERT INTO visits (url, title, visit_time, transition) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![canon, title, now, transition.as_str()],
            )?;
        }
        Ok(())
    }

    /// Update the title of the most recent visit for `url`. Used by
    /// the optional `DisplayHandler::on_title_change` integration —
    /// CEF emits the title slightly after `on_load_end`.
    pub fn update_latest_title(&self, url: &str, title: &str) -> Result<(), HistoryError> {
        let canon = match canonicalise(url) {
            Some(c) => c,
            None => return Ok(()),
        };
        let title = title.trim();
        if title.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;
        conn.execute(
            "UPDATE visits SET title = ?1 \
             WHERE id = (SELECT id FROM visits WHERE url = ?2 \
                         ORDER BY visit_time DESC LIMIT 1)",
            params![title, canon],
        )?;
        Ok(())
    }

    /// Return the `limit` most recent visits, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, HistoryError> {
        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT id, url, title, visit_time, transition \
             FROM visits ORDER BY visit_time DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Search by substring (case-sensitive — SQLite `LIKE` is
    /// case-insensitive on ASCII, case-sensitive on non-ASCII; for v1
    /// that's fine since URLs are ASCII).
    ///
    /// Frecency formula:
    /// `score = visit_count * 2 + recency_bonus`, where
    /// `recency_bonus = 10` if the latest visit was inside
    /// [`RECENCY_WINDOW_SECS`] (7 days) else `0`.
    ///
    /// Result rows are deduplicated by `url`; `visit_time` reflects
    /// the most recent visit for that URL. Tie-break by `visit_time
    /// DESC` so two equally-scored entries surface the more recent
    /// one first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>, HistoryError> {
        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;
        let now = self.clock.now().timestamp();
        let recent_cutoff = now - RECENCY_WINDOW_SECS;
        let needle = format!("%{query}%");
        let mut stmt = conn.prepare(
            "SELECT id, url, title, visit_time, transition, score FROM ( \
               SELECT MAX(id) AS id, url, \
                      (SELECT title FROM visits v2 WHERE v2.url = visits.url \
                       ORDER BY visit_time DESC LIMIT 1) AS title, \
                      MAX(visit_time) AS visit_time, \
                      (SELECT transition FROM visits v2 WHERE v2.url = visits.url \
                       ORDER BY visit_time DESC LIMIT 1) AS transition, \
                      (COUNT(*) * 2 + CASE WHEN MAX(visit_time) >= ?1 THEN 10 ELSE 0 END) AS score \
               FROM visits \
               WHERE url LIKE ?2 OR title LIKE ?2 \
               GROUP BY url \
             ) ORDER BY score DESC, visit_time DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![recent_cutoff, needle, limit as i64], |row| {
                Ok(HistoryEntry {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    title: row.get(2)?,
                    visit_time: ts_to_dt(row.get(3)?),
                    transition: Transition::parse(&row.get::<_, String>(4)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete every visit with `from <= visit_time < to`. Returns the
    /// number of rows deleted. Used by Phase 5 "clear browsing data".
    pub fn clear_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<usize, HistoryError> {
        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;
        let n = conn.execute(
            "DELETE FROM visits WHERE visit_time >= ?1 AND visit_time < ?2",
            params![from.timestamp(), to.timestamp()],
        )?;
        Ok(n)
    }

    /// Total row count. O(1) with the index but we don't care — this
    /// is for tests / startup logging only.
    pub fn count(&self) -> Result<usize, HistoryError> {
        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM visits", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Delete every visit row. Returns the number deleted. Used by the
    /// `[privacy] clear_on_exit = ["history"]` shutdown hook in
    /// `apps/buffr`. Runs `VACUUM` afterward to actually shrink the DB
    /// file rather than just marking pages free.
    pub fn clear_all(&self) -> Result<usize, HistoryError> {
        let conn = self.conn.lock().map_err(|_| HistoryError::Poisoned)?;
        let n = conn.execute("DELETE FROM visits", [])?;
        // VACUUM cannot run inside a transaction. Failure is non-fatal —
        // the DELETE already removed the data, VACUUM is just storage
        // hygiene.
        if let Err(err) = conn.execute("VACUUM", []) {
            tracing::warn!(error = %err, "history: VACUUM after clear_all failed");
        }
        Ok(n)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        visit_time: ts_to_dt(row.get(3)?),
        transition: Transition::parse(&row.get::<_, String>(4)?),
    })
}

fn ts_to_dt(secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"))
}

/// Parse + canonicalise a URL string. Returns `None` when parsing
/// fails. We round-trip via `url::Url` so trailing whitespace, mixed
/// case in the scheme/host, and default ports get normalised.
fn canonicalise(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    url::Url::parse(trimmed).ok().map(|u| u.to_string())
}

fn is_skip_scheme(canonical_url: &str) -> bool {
    let scheme = match canonical_url.split_once(':') {
        Some((s, _)) => s.to_ascii_lowercase(),
        None => return true,
    };
    SKIP_SCHEMES.contains(&scheme.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_clock(secs: i64) -> MockClock {
        MockClock::new(DateTime::<Utc>::from_timestamp(secs, 0).expect("ts"))
    }

    #[test]
    fn open_in_memory_runs_migrations() {
        let h = History::open_in_memory().unwrap();
        assert_eq!(h.count().unwrap(), 0);
        assert_eq!(schema::latest_version(), 1);
    }

    #[test]
    fn record_three_visits_recent_orders_newest_first() {
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();
        h.record_visit("https://a.example/", Some("A"), Transition::Link)
            .unwrap();
        clock.advance(120);
        h.record_visit("https://b.example/", Some("B"), Transition::Link)
            .unwrap();
        clock.advance(120);
        h.record_visit("https://c.example/", Some("C"), Transition::Link)
            .unwrap();

        assert_eq!(h.count().unwrap(), 3);
        let recent = h.recent(10).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].url, "https://c.example/");
        assert_eq!(recent[1].url, "https://b.example/");
        assert_eq!(recent[2].url, "https://a.example/");
    }

    #[test]
    fn dedupe_within_window_updates_in_place() {
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();
        h.record_visit("https://a.example/", Some("A"), Transition::Link)
            .unwrap();
        clock.advance(5);
        h.record_visit("https://a.example/", Some("A"), Transition::Link)
            .unwrap();
        assert_eq!(h.count().unwrap(), 1);
    }

    #[test]
    fn dedupe_boundary_inserts_after_window() {
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();
        h.record_visit("https://a.example/", Some("A"), Transition::Link)
            .unwrap();
        clock.advance(61);
        h.record_visit("https://a.example/", Some("A"), Transition::Link)
            .unwrap();
        assert_eq!(h.count().unwrap(), 2);
    }

    #[test]
    fn dedupe_keeps_existing_title_when_new_is_empty() {
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();
        h.record_visit("https://a.example/", Some("Original"), Transition::Link)
            .unwrap();
        clock.advance(10);
        h.record_visit("https://a.example/", None, Transition::Link)
            .unwrap();
        let recent = h.recent(1).unwrap();
        assert_eq!(recent[0].title.as_deref(), Some("Original"));
    }

    #[test]
    fn search_ranks_by_frecency() {
        // a.example: visited 3x within recency window → score 6 + 10 = 16
        // b.example: visited 1x within recency window → score 2 + 10 = 12
        // c.example: visited 5x but ages ago → score 10 + 0 = 10
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();

        for _ in 0..5 {
            h.record_visit("https://c.example/old", Some("Old"), Transition::Link)
                .unwrap();
            clock.advance(120);
        }
        // Jump 30 days forward so `c.example` falls outside the
        // 7-day recency window.
        clock.advance(30 * 24 * 3600);

        for _ in 0..3 {
            h.record_visit("https://a.example/new", Some("New A"), Transition::Link)
                .unwrap();
            clock.advance(120);
        }
        h.record_visit("https://b.example/new", Some("New B"), Transition::Link)
            .unwrap();

        let results = h.search("example", 10).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].url, "https://a.example/new");
        assert_eq!(results[1].url, "https://b.example/new");
        assert_eq!(results[2].url, "https://c.example/old");
    }

    #[test]
    fn clear_range_deletes_window() {
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();
        h.record_visit("https://a.example/", None, Transition::Link)
            .unwrap();
        let mid = clock.now();
        clock.advance(120);
        h.record_visit("https://b.example/", None, Transition::Link)
            .unwrap();
        clock.advance(120);
        h.record_visit("https://c.example/", None, Transition::Link)
            .unwrap();

        let to = clock.now();
        let removed = h
            .clear_range(
                mid + chrono::Duration::seconds(1),
                to + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(h.count().unwrap(), 1);
        assert_eq!(h.recent(1).unwrap()[0].url, "https://a.example/");
    }

    #[test]
    fn skip_scheme_returns_ok_without_insert() {
        let h = History::open_in_memory().unwrap();
        h.record_visit("about:blank", None, Transition::Link)
            .unwrap();
        h.record_visit("chrome://settings", None, Transition::Link)
            .unwrap();
        h.record_visit("data:text/plain,hi", None, Transition::Link)
            .unwrap();
        assert_eq!(h.count().unwrap(), 0);
    }

    #[test]
    fn unparseable_url_returns_ok_without_insert() {
        let h = History::open_in_memory().unwrap();
        h.record_visit("not a url at all", None, Transition::Link)
            .unwrap();
        assert_eq!(h.count().unwrap(), 0);
    }

    #[test]
    fn clear_all_wipes_table_and_reports_count() {
        let h = History::open_in_memory().unwrap();
        h.record_visit("https://a.example/", None, Transition::Link)
            .unwrap();
        h.record_visit("https://b.example/", None, Transition::Link)
            .unwrap();
        h.record_visit("https://c.example/", None, Transition::Link)
            .unwrap();
        assert_eq!(h.count().unwrap(), 3);
        let removed = h.clear_all().unwrap();
        assert_eq!(removed, 3);
        assert_eq!(h.count().unwrap(), 0);
        // Idempotent: second call returns 0, doesn't error.
        assert_eq!(h.clear_all().unwrap(), 0);
    }

    #[test]
    fn update_latest_title_only_touches_most_recent() {
        let clock = fixed_clock(1_000_000_000);
        let h = History::open_in_memory_with_clock(Box::new(clock.clone())).unwrap();
        h.record_visit("https://a.example/", Some("first"), Transition::Link)
            .unwrap();
        clock.advance(61);
        h.record_visit("https://a.example/", Some("second"), Transition::Link)
            .unwrap();
        h.update_latest_title("https://a.example/", "fresh")
            .unwrap();
        let recent = h.recent(2).unwrap();
        assert_eq!(recent[0].title.as_deref(), Some("fresh"));
        assert_eq!(recent[1].title.as_deref(), Some("first"));
    }
}
