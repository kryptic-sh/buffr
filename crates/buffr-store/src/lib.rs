//! Shared SQLite plumbing for the buffr stores.
//!
//! `buffr-history`, `buffr-bookmarks`, `buffr-downloads`, `buffr-zoom`
//! and `buffr-permissions` all follow the same shape:
//!
//! - open the file with `READ_WRITE | CREATE`,
//! - apply three pragmas (`journal_mode=WAL`, `synchronous=NORMAL`,
//!   `foreign_keys=ON`),
//! - run a forward-only `MIGRATIONS: &[&str]` array against a
//!   `schema_version` table.
//!
//! That was five byte-identical copies. It lives here once now. Each
//! store keeps its own error enum and maps [`MigrationError`] into it
//! at the single call site, so no public error type changed shape.
//!
//! # Migration protocol
//!
//! `schema_version` holds one row per applied migration. Index `i` of
//! the `migrations` slice corresponds to version `i + 1`. On open we
//! read `MAX(version)` and run everything above it, each migration in
//! its own transaction together with its `schema_version` row.
//!
//! A stored version **above** `migrations.len()` means the database was
//! written by a newer buffr that knows migrations this binary does not.
//! Opening it read-write would silently corrupt the newer schema, so
//! [`apply`] refuses with [`MigrationError::TooNew`] rather than
//! carrying on.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;

/// Why [`apply`] gave up.
///
/// Callers map this into their own error enum; it is deliberately not
/// re-exported by any store.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// A migration statement (or the `schema_version` bookkeeping
    /// around it) failed. `version` is the migration being applied, or
    /// `0` for the bootstrap `CREATE TABLE` / version probe.
    #[error("applying migration v{version} failed")]
    Sql {
        #[source]
        source: rusqlite::Error,
        version: i64,
    },
    /// The database records a schema version this binary doesn't know
    /// about — it was written by a newer buffr.
    #[error("database schema v{found} is newer than supported v{supported}")]
    TooNew { found: i64, supported: i64 },
}

/// Open (creating if absent) the database at `path` and apply the
/// standard buffr pragmas.
pub fn open_tuned(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    tune(&conn)?;
    Ok(conn)
}

/// In-memory database with the same pragmas — tests and ephemeral
/// (private-window) profiles.
pub fn open_tuned_in_memory() -> rusqlite::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    tune(&conn)?;
    Ok(conn)
}

/// Apply per-connection pragmas. WAL gives non-blocking reads while a
/// writer is active; `synchronous=NORMAL` is safe under WAL and avoids
/// fsync-per-commit thrash. `foreign_keys=ON` is belt-and-braces —
/// not every store has FKs today, but every future migration that adds
/// them will Just Work.
fn tune(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Run every pending migration in `migrations`, leaving
/// `schema_version` at the new high-water mark.
pub fn apply(conn: &mut Connection, migrations: &[&str]) -> Result<(), MigrationError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")
        .map_err(|source| MigrationError::Sql { source, version: 0 })?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|source| MigrationError::Sql { source, version: 0 })?;

    let supported = latest_version(migrations);
    if current > supported {
        return Err(MigrationError::TooNew {
            found: current,
            supported,
        });
    }

    for (idx, sql) in migrations.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|source| MigrationError::Sql { source, version })?;
        tx.execute_batch(sql)
            .map_err(|source| MigrationError::Sql { source, version })?;
        tx.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            params![version],
        )
        .map_err(|source| MigrationError::Sql { source, version })?;
        tx.commit()
            .map_err(|source| MigrationError::Sql { source, version })?;
    }

    Ok(())
}

/// Highest version the given migration set knows about.
pub fn latest_version(migrations: &[&str]) -> i64 {
    migrations.len() as i64
}

/// Decode unix-epoch seconds into a UTC timestamp. Out-of-range values
/// clamp to the epoch rather than panicking — a corrupt row shouldn't
/// take the browser down.
#[cfg(feature = "chrono")]
pub fn ts_to_dt(secs: i64) -> chrono::DateTime<chrono::Utc> {
    use chrono::{DateTime, Utc};
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is in range"))
}

/// Wall-clock unix-epoch seconds. For stores that don't want chrono.
pub fn current_unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &[&str] = &[
        "CREATE TABLE a (id INTEGER PRIMARY KEY);",
        "CREATE TABLE b (id INTEGER PRIMARY KEY);",
    ];

    #[test]
    fn apply_runs_all_migrations_then_is_idempotent() {
        let mut conn = open_tuned_in_memory().unwrap();
        apply(&mut conn, M).unwrap();
        let v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        // Second run is a no-op, not a duplicate-key error.
        apply(&mut conn, M).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn apply_resumes_from_partial_version() {
        let mut conn = open_tuned_in_memory().unwrap();
        apply(&mut conn, &M[..1]).unwrap();
        apply(&mut conn, M).unwrap();
        let v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
    }

    #[test]
    fn apply_refuses_a_version_from_the_future() {
        let mut conn = open_tuned_in_memory().unwrap();
        apply(&mut conn, M).unwrap();
        // Pretend a newer buffr wrote v3.
        conn.execute("INSERT INTO schema_version(version) VALUES (3)", [])
            .unwrap();
        let err = apply(&mut conn, M).unwrap_err();
        assert!(matches!(
            err,
            MigrationError::TooNew {
                found: 3,
                supported: 2
            }
        ));
    }

    #[test]
    fn latest_version_counts_migrations() {
        assert_eq!(latest_version(M), 2);
        assert_eq!(latest_version(&[]), 0);
    }

    #[test]
    fn tune_sets_pragmas() {
        let conn = open_tuned_in_memory().unwrap();
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn ts_to_dt_clamps_out_of_range() {
        assert_eq!(ts_to_dt(0).timestamp(), 0);
        assert_eq!(ts_to_dt(i64::MAX).timestamp(), 0);
        assert_eq!(ts_to_dt(1_700_000_000).timestamp(), 1_700_000_000);
    }

    #[test]
    fn current_unix_time_is_plausible() {
        // Later than 2020-01-01, earlier than 2100-01-01.
        let now = current_unix_time();
        assert!(now > 1_577_836_800);
        assert!(now < 4_102_444_800);
    }
}
