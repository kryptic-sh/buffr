//! SQLite schema + forward-only migrations for [`crate::Permissions`].
//!
//! Same `schema_version` table pattern as the other buffr stores: one
//! row per applied migration, monotonically increasing. Append new
//! migrations to [`MIGRATIONS`]; never rewrite an old entry.

use rusqlite::{Connection, params};

use crate::PermError;

/// Forward-only migrations. Index `i` here corresponds to schema
/// version `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema. One row per (origin, capability); `decision`
    // is a serde-rendered `snake_case` enum string ("allow" / "deny");
    // `set_at` is unix-epoch seconds, used for `all()` ordering.
    r#"
    CREATE TABLE IF NOT EXISTS permissions (
      origin     TEXT NOT NULL,
      capability TEXT NOT NULL,
      decision   TEXT NOT NULL,
      set_at     INTEGER NOT NULL,
      PRIMARY KEY (origin, capability)
    );
    CREATE INDEX IF NOT EXISTS idx_permissions_set_at
      ON permissions(set_at DESC);
    "#,
];

/// Run all pending migrations.
pub(crate) fn apply(conn: &mut Connection) -> Result<(), PermError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")
        .map_err(|source| PermError::Migrate { source, version: 0 })?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|source| PermError::Migrate { source, version: 0 })?;

    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|source| PermError::Migrate { source, version })?;
        tx.execute_batch(sql)
            .map_err(|source| PermError::Migrate { source, version })?;
        tx.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            params![version],
        )
        .map_err(|source| PermError::Migrate { source, version })?;
        tx.commit()
            .map_err(|source| PermError::Migrate { source, version })?;
    }

    Ok(())
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}
