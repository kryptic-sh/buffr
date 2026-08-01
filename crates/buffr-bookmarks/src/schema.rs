//! SQLite schema + forward-only migrations for [`crate::Bookmarks`].
//!
//! Same `schema_version` table pattern as `buffr-history`: one row per
//! applied migration, monotonically increasing. Append new migrations
//! to [`MIGRATIONS`]; never rewrite an old entry.

use rusqlite::Connection;

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

/// Run all pending migrations. Thin wrapper over the shared runner in
/// [`buffr_store`] — the only crate-specific part is mapping the
/// failure into [`BookmarkError`].
pub(crate) fn apply(conn: &mut Connection) -> Result<(), BookmarkError> {
    buffr_store::apply(conn, MIGRATIONS).map_err(|e| match e {
        buffr_store::MigrationError::Sql { source, version } => {
            BookmarkError::Migrate { source, version }
        }
        buffr_store::MigrationError::TooNew { found, supported } => {
            BookmarkError::SchemaTooNew { found, supported }
        }
    })
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    buffr_store::latest_version(MIGRATIONS)
}
