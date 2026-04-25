//! SQLite schema + forward-only migrations for [`crate::Bookmarks`].
//!
//! Same `schema_version` table pattern as `buffr-history`: one row per
//! applied migration, monotonically increasing. Append new migrations
//! to [`MIGRATIONS`]; never rewrite an old entry.

use rusqlite::{Connection, params};

use crate::BookmarkError;

/// Forward-only migrations. Index `i` here corresponds to schema
/// version `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema. `bookmarks` table holds one row per
    // canonical URL (UNIQUE constraint enforces upsert-by-URL).
    // `bookmark_tags` is a many-to-many join table: tags are stored
    // lowercase / trimmed in the application layer, so SQL-side tag
    // queries can do exact-match lookups via the secondary index.
    r#"
    CREATE TABLE IF NOT EXISTS bookmarks (
      id          INTEGER PRIMARY KEY AUTOINCREMENT,
      url         TEXT NOT NULL UNIQUE,
      title       TEXT,
      added       INTEGER NOT NULL,
      modified    INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS bookmark_tags (
      bookmark_id INTEGER NOT NULL REFERENCES bookmarks(id) ON DELETE CASCADE,
      tag         TEXT NOT NULL,
      PRIMARY KEY (bookmark_id, tag)
    );
    CREATE INDEX IF NOT EXISTS idx_bookmark_tags_tag ON bookmark_tags(tag);
    CREATE INDEX IF NOT EXISTS idx_bookmarks_modified ON bookmarks(modified DESC);
    "#,
];

/// Run all pending migrations.
pub(crate) fn apply(conn: &mut Connection) -> Result<(), BookmarkError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")
        .map_err(|source| BookmarkError::Migrate { source, version: 0 })?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|source| BookmarkError::Migrate { source, version: 0 })?;

    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|source| BookmarkError::Migrate { source, version })?;
        tx.execute_batch(sql)
            .map_err(|source| BookmarkError::Migrate { source, version })?;
        tx.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            params![version],
        )
        .map_err(|source| BookmarkError::Migrate { source, version })?;
        tx.commit()
            .map_err(|source| BookmarkError::Migrate { source, version })?;
    }

    Ok(())
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}
