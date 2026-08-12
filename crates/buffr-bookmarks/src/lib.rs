//! SQLite-backed bookmarks store for buffr (Phase 5).
//!
//! Phase-5 scope: a pure data layer. No UI, no IPC. Mirrors the
//! [`buffr_history`] crate's shape — one `Mutex<Connection>`, forward-
//! only migrations, no FTS5 yet.
//!
//! # Schema (v1)
//!
//! Two tables: `bookmarks` (one row per canonical URL) and
//! `bookmark_tags` (many-to-many join). See [`schema`].
//!
//! # Behaviour
//!
//! - URLs are canonicalised through `url::Url::parse`. Failed parse →
//!   [`BookmarkError::Url`].
//! - [`Bookmarks::add`] is **upsert by URL**: if the URL already exists
//!   the title / tags / `modified` get overwritten, no error.
//! - Tags are normalised on the way in — lowercase, trimmed, deduped,
//!   empty entries dropped. Stored lowercase so `by_tag` is a plain
//!   equality lookup.
//! - [`Bookmarks::search`] does case-insensitive substring match over
//!   url, title, and any tag, with ordering
//!   `title-match > url-match > tag-match`, then `modified DESC`, then
//!   `id DESC`. The match, the rank and any cap run in SQL, and tags
//!   for the result set come back in one further query — two
//!   round-trips per call regardless of profile size. Interactive
//!   callers should use [`Bookmarks::search_limited`].
//! - [`Bookmarks::import_netscape`] parses the Netscape Bookmark File
//!   Format (Chrome/Firefox/Edge export shape) via a regex walker —
//!   the format is loose enough that a real HTML parser is overkill.
//!   HREFs and titles are HTML-entity-decoded, and the whole import is
//!   one transaction: all-or-nothing.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;

use buffr_store::ts_to_dt;
use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, trace};

pub mod schema;

/// Strongly-typed bookmark id. New-type around `i64` so callers can't
/// accidentally pass a history id or a tab id where a bookmark id is
/// expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BookmarkId(pub i64);

/// One bookmark, decoded into Rust types. Tags are sorted alpha so
/// equality checks in tests don't depend on insertion order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub id: BookmarkId,
    pub url: String,
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub added: DateTime<Utc>,
    pub modified: DateTime<Utc>,
}

/// Errors surfaced from [`Bookmarks`]. `rusqlite::Error` is wrapped via
/// `#[from]` so callers don't need to depend on `rusqlite` directly.
#[derive(Debug, Error)]
pub enum BookmarkError {
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
    /// The on-disk schema is newer than this binary understands —
    /// the profile was written by a newer buffr. Refusing beats
    /// silently running old code against a newer schema.
    #[error("database schema v{found} is newer than supported v{supported}")]
    SchemaTooNew { found: i64, supported: i64 },
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
    #[error("bookmarks mutex poisoned")]
    Poisoned,
}

/// SQLite-backed bookmarks store.
pub struct Bookmarks {
    conn: Mutex<Connection>,
}

impl Bookmarks {
    /// Open or create the SQLite database at `path` and run any
    /// pending schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BookmarkError> {
        let mut conn = buffr_store::open_tuned(path.as_ref())
            .map_err(|source| BookmarkError::Open { source })?;
        register_lower(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database — for tests and short-lived ephemeral
    /// profiles (private windows, Phase 5 follow-up).
    pub fn open_in_memory() -> Result<Self, BookmarkError> {
        let mut conn =
            buffr_store::open_tuned_in_memory().map_err(|source| BookmarkError::Open { source })?;
        register_lower(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Add or update a bookmark by URL.
    ///
    /// **Upsert**: if `url` already canonicalises to an existing row,
    /// that row's `title`, `tags`, and `modified` are overwritten and
    /// the existing id is returned. `added` is preserved across
    /// upserts.
    pub fn add(
        &self,
        url: &str,
        title: Option<&str>,
        tags: &[&str],
    ) -> Result<BookmarkId, BookmarkError> {
        let mut conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let tx = conn.transaction()?;
        let id = add_in_tx(&tx, url, title, tags)?;
        tx.commit()?;
        Ok(id)
    }

    /// Remove a bookmark by id. Returns `true` iff a row was deleted.
    /// `bookmark_tags` rows are removed via `ON DELETE CASCADE`.
    pub fn remove(&self, id: BookmarkId) -> Result<bool, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let n = conn.execute("DELETE FROM bookmarks WHERE id = ?1", params![id.0])?;
        Ok(n > 0)
    }

    /// Update title and/or tags on an existing bookmark. `None` for
    /// either argument leaves that field untouched. Bumps `modified`
    /// only when something actually changes.
    pub fn update(
        &self,
        id: BookmarkId,
        title: Option<Option<&str>>,
        tags: Option<&[&str]>,
    ) -> Result<bool, BookmarkError> {
        if title.is_none() && tags.is_none() {
            return Ok(false);
        }
        let now = Utc::now().timestamp();
        let mut conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let tx = conn.transaction()?;

        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM bookmarks WHERE id = ?1",
                params![id.0],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(false);
        }

        if let Some(t) = title {
            let t_owned = t
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            tx.execute(
                "UPDATE bookmarks SET title = ?1, modified = ?2 WHERE id = ?3",
                params![t_owned, now, id.0],
            )?;
        }
        if let Some(new_tags) = tags {
            let normalised = normalise_tags(new_tags);
            tx.execute(
                "DELETE FROM bookmark_tags WHERE bookmark_id = ?1",
                params![id.0],
            )?;
            for tag in &normalised {
                tx.execute(
                    "INSERT OR IGNORE INTO bookmark_tags (bookmark_id, tag) VALUES (?1, ?2)",
                    params![id.0, tag],
                )?;
            }
            tx.execute(
                "UPDATE bookmarks SET modified = ?1 WHERE id = ?2",
                params![now, id.0],
            )?;
        }

        tx.commit()?;
        Ok(true)
    }

    /// Fetch a single bookmark by id.
    pub fn get(&self, id: BookmarkId) -> Result<Option<Bookmark>, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let row: Option<(i64, String, Option<String>, i64, i64)> = conn
            .query_row(
                "SELECT id, url, title, added, modified FROM bookmarks WHERE id = ?1",
                params![id.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((rid, url, title, added, modified)) = row else {
            return Ok(None);
        };
        let tags = load_tags(&conn, rid)?;
        Ok(Some(Bookmark {
            id: BookmarkId(rid),
            url,
            title,
            tags,
            added: ts_to_dt(added),
            modified: ts_to_dt(modified),
        }))
    }

    /// All bookmarks, most recently modified first.
    pub fn all(&self) -> Result<Vec<Bookmark>, BookmarkError> {
        self.all_limited(NO_LIMIT)
    }

    /// `all()` with a SQL-side `LIMIT`. `-1` means unlimited (SQLite's
    /// own convention).
    fn all_limited(&self, limit: i64) -> Result<Vec<Bookmark>, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT id, url, title, added, modified FROM bookmarks \
             ORDER BY modified DESC, id DESC LIMIT ?1",
        )?;
        let rows: Vec<BookmarkRow> = stmt
            .query_map(params![limit], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        rows_to_bookmarks(&conn, rows)
    }

    /// Bookmarks tagged with `tag` (case-insensitive — input is
    /// normalised the same way storage is).
    pub fn by_tag(&self, tag: &str) -> Result<Vec<Bookmark>, BookmarkError> {
        let needle = tag.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT b.id, b.url, b.title, b.added, b.modified FROM bookmarks b \
             JOIN bookmark_tags t ON t.bookmark_id = b.id \
             WHERE t.tag = ?1 \
             ORDER BY b.modified DESC, b.id DESC",
        )?;
        let rows: Vec<BookmarkRow> = stmt
            .query_map(params![needle], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        rows_to_bookmarks(&conn, rows)
    }

    /// Case-insensitive substring search across url, title, and tags.
    ///
    /// Ordering: title-match (rank 0) > url-match (rank 1) >
    /// tag-match (rank 2), then `modified DESC`, then `id DESC`. A
    /// bookmark is returned at most once even if it matches in several
    /// fields — the best (lowest-rank) match wins.
    ///
    /// Unbounded. Interactive callers (the omnibar) should prefer
    /// [`Bookmarks::search_limited`], which pushes the cap into SQL.
    pub fn search(&self, query: &str) -> Result<Vec<Bookmark>, BookmarkError> {
        self.search_limited(query, None)
    }

    /// [`Bookmarks::search`] with a SQL-side `LIMIT`.
    ///
    /// The match, the rank and the cap all run inside SQLite, and tags
    /// for the surviving rows are fetched in one extra round-trip — so
    /// an omnibar keystroke costs two queries regardless of how many
    /// bookmarks exist, instead of one-per-bookmark (M40).
    pub fn search_limited(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Bookmark>, BookmarkError> {
        let limit = limit.map_or(NO_LIMIT, |n| i64::try_from(n).unwrap_or(i64::MAX));
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return self.all_limited(limit);
        }
        let pattern = format!("%{}%", escape_like(&needle));
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare_cached(SEARCH_SQL)?;
        let rows: Vec<BookmarkRow> = stmt
            .query_map(params![pattern, limit], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        rows_to_bookmarks(&conn, rows)
    }

    /// All distinct tags, sorted alphabetically.
    pub fn all_tags(&self) -> Result<Vec<String>, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare("SELECT DISTINCT tag FROM bookmark_tags ORDER BY tag ASC")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Total bookmark count. Used by tests + diagnostics.
    pub fn count(&self) -> Result<usize, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM bookmarks", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Delete every bookmark + tag row. Returns the number of bookmark
    /// rows deleted (not tag rows). Tag rows go via
    /// `ON DELETE CASCADE`. Runs `VACUUM` afterward to shrink the file.
    /// Used by the `[privacy] clear_on_exit = ["bookmarks"]` shutdown
    /// hook in `apps/buffr` — bookmarks are user-explicit so this is
    /// only honored when the user lists `bookmarks` in `clear_on_exit`.
    pub fn clear_all(&self) -> Result<usize, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let n = conn.execute("DELETE FROM bookmarks", [])?;
        if let Err(err) = conn.execute("VACUUM", []) {
            tracing::warn!(error = %err, "bookmarks: VACUUM after clear_all failed");
        }
        Ok(n)
    }

    /// Parse a Netscape Bookmark File Format HTML document and import
    /// every `<A HREF="...">` it finds. Returns the count of
    /// bookmarks successfully inserted (or upserted).
    ///
    /// Folder names from enclosing `<H3>` tags are added as tags on
    /// every `<A>` inside that folder. The `TAGS=` attribute on each
    /// `<A>` (Chrome / Pinboard convention) is split on comma and
    /// merged.
    ///
    /// Implementation: regex-based walker. The Netscape format is
    /// loose HTML — unbalanced tags, no DTD, no closing `</DT>` — so a
    /// strict parser is overkill. We scan top-to-bottom, push folder
    /// names from `<H3>` onto a stack (popping on `</DL>`), and emit
    /// one upsert per `<A>`.
    ///
    /// **Atomic.** The whole walk runs in a single transaction (M42):
    /// a 5 000-entry export is one commit, and a SQL failure part-way
    /// through rolls the entire import back and returns `Err` rather
    /// than leaving a half-imported store behind an `Ok(partial)`.
    /// Entries whose `HREF` doesn't parse are still skipped
    /// individually — that's malformed input, not a store failure, and
    /// no SQL has been issued for them.
    ///
    /// HREFs and titles are HTML-entity-decoded (M41), so a real
    /// browser export of `https://example.com/?a=1&amp;b=2` is stored
    /// with a literal `&`.
    pub fn import_netscape(&self, html: &str) -> Result<usize, BookmarkError> {
        // Tokens we walk in document order. We can't reuse a single
        // regex because `regex` doesn't support overlapping captures,
        // and we genuinely need to know the relative ordering of
        // `<H3>`, `</DL>`, and `<A>` to maintain the folder stack.
        //
        // The open-tag body is `(?:[^>"]|"[^"]*")*`: either a
        // non-`>`-non-`"` char or a complete double-quoted run. A `>`
        // inside a quoted attribute value (a hand-authored file can
        // contain `<A HREF="https://x/?a=1>2&b=3">`) is part of the
        // value, and only an unquoted `>` closes the tag (A13). Side
        // effect: a tag with an unclosed quote no longer matches at
        // all, so that entry is skipped rather than corrupted —
        // acceptable degradation for malformed input.
        let attr_re = Regex::new(r#"(?i)(\w+)\s*=\s*"([^"]*)""#).expect("attr regex");
        // One regex, not three independent passes: the anchor alternative
        // comes first, so a whole `<A ...>...</A>` is consumed as a single
        // token and any `<H3>` / `</DL>` markup inside its label is never
        // seen as a folder token. The old three-regex tokenizer matched
        // `<H3>x</H3>` inside an anchor label, pushed "x" as a folder that
        // no `</DL>` ever popped, tagged every later anchor with it, and
        // popped the real folders one level early.
        let tok_re = Regex::new(
            r#"(?is)(?P<anchor><A\s+(?P<anchor_attrs>(?:[^>"]|"[^"]*")*)>(?P<anchor_label>.*?)</A>)|(?P<h3><H3(?P<h3_attrs>(?:[^>"]|"[^"]*")*)>(?P<h3_label>.*?)</H3>)|(?P<dl></DL>)"#,
        )
        .expect("netscape token regex");

        // Collect every match into one ordered token stream. A single
        // regex pass yields document order, so no re-sort is needed.
        enum Tok<'a> {
            FolderOpen(&'a str),
            FolderClose,
            Anchor { attrs: &'a str, label: &'a str },
        }
        let mut toks: Vec<Tok<'_>> = Vec::new();
        for m in tok_re.captures_iter(html) {
            if m.name("anchor").is_some() {
                toks.push(Tok::Anchor {
                    attrs: m.name("anchor_attrs").map(|m| m.as_str()).unwrap_or(""),
                    label: m.name("anchor_label").map(|m| m.as_str()).unwrap_or(""),
                });
            } else if m.name("h3").is_some() {
                toks.push(Tok::FolderOpen(
                    m.name("h3_label").map(|m| m.as_str()).unwrap_or(""),
                ));
            } else {
                toks.push(Tok::FolderClose);
            }
        }

        // Walk: folder stack mirrors `<DL>` nesting via `<H3>` opens
        // and `</DL>` closes. The Netscape format places one `<H3>`
        // immediately before its `<DL>`, and we never see the `<DL>`
        // open token here (we don't need it — the `<H3>` itself opens
        // the folder for tag-purposes).
        let mut folder_stack: Vec<String> = Vec::new();
        let mut count = 0usize;
        let mut conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let tx = conn.transaction()?;
        for tok in toks {
            match tok {
                Tok::FolderOpen(label) => {
                    let cleaned = strip_html(label).trim().to_string();
                    folder_stack.push(cleaned);
                }
                Tok::FolderClose => {
                    folder_stack.pop();
                }
                Tok::Anchor { attrs, label } => {
                    let mut href: Option<String> = None;
                    let mut tags: Vec<String> = Vec::new();
                    for m in attr_re.captures_iter(attrs) {
                        let key = m
                            .get(1)
                            .map(|x| x.as_str())
                            .unwrap_or("")
                            .to_ascii_uppercase();
                        let val = decode_entities(m.get(2).map(|x| x.as_str()).unwrap_or(""));
                        match key.as_str() {
                            "HREF" => href = Some(val.clone()),
                            "TAGS" => {
                                for t in val.split(',') {
                                    let trimmed = t.trim();
                                    if !trimmed.is_empty() {
                                        tags.push(trimmed.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let Some(href) = href else {
                        trace!("netscape import: <A> without HREF; skipping");
                        continue;
                    };
                    // Folders → tags. Filter empties so `<H3></H3>`
                    // doesn't poison the tag list.
                    for folder in &folder_stack {
                        if !folder.is_empty() {
                            tags.push(folder.clone());
                        }
                    }
                    let title = strip_html(label);
                    let title_opt = if title.trim().is_empty() {
                        None
                    } else {
                        Some(title)
                    };
                    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                    match add_in_tx(&tx, &href, title_opt.as_deref(), &tag_refs) {
                        Ok(_) => count += 1,
                        // Unparseable HREF — malformed input, not a
                        // store failure, and no SQL was issued. Skip
                        // the entry and keep the transaction alive.
                        Err(e @ BookmarkError::Url { .. }) => {
                            debug!(error = %e, href = %href, "netscape import: skipping malformed entry");
                        }
                        // Anything else is a real SQLite failure:
                        // bail out and let the `tx` drop roll the
                        // whole import back.
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        tx.commit()?;
        Ok(count)
    }
}

/// Row shape shared by every `SELECT` that hydrates a [`Bookmark`]:
/// `(id, url, title, added, modified)`.
type BookmarkRow = (i64, String, Option<String>, i64, i64);

/// SQLite's `LIMIT` sentinel for "no limit".
const NO_LIMIT: i64 = -1;

/// Match + rank + cap, all inside SQLite.
///
/// `?1` is the `LIKE` pattern (already lowercased and `%`/`_`-escaped),
/// `?2` the row cap (`-1` = unlimited).
///
/// The rank `CASE` reproduces the old Rust-side `if / else if / else if`
/// exactly — title beats url beats tag, first hit wins — and the
/// `ORDER BY rank, modified DESC, id DESC` reproduces the old stable
/// sort over an `all()` that was already ordered `modified DESC,
/// id DESC`. The `ELSE 2` arm is only ever reached for rows that got
/// into the result set via the tag `EXISTS`, so it can't mislabel a
/// non-matching row.
const SEARCH_SQL: &str = r#"
    SELECT b.id, b.url, b.title, b.added, b.modified,
           CASE
             WHEN buffr_lower(COALESCE(b.title, '')) LIKE ?1 ESCAPE '\' THEN 0
             WHEN buffr_lower(b.url) LIKE ?1 ESCAPE '\' THEN 1
             ELSE 2
           END AS match_rank
      FROM bookmarks b
     WHERE buffr_lower(COALESCE(b.title, '')) LIKE ?1 ESCAPE '\'
        OR buffr_lower(b.url) LIKE ?1 ESCAPE '\'
        OR EXISTS (SELECT 1 FROM bookmark_tags t
                    WHERE t.bookmark_id = b.id AND t.tag LIKE ?1 ESCAPE '\')
     ORDER BY match_rank ASC, b.modified DESC, b.id DESC
     LIMIT ?2
"#;

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookmarkRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

/// Register `buffr_lower(x)` — a Unicode-aware `lower()`.
///
/// SQLite's built-in `lower()` folds ASCII only, but `search` has
/// always compared `str::to_lowercase()` output on both sides. Pushing
/// the filter into SQL (M40) with the built-in would have silently
/// dropped non-ASCII case-insensitivity, so we register the same
/// folding SQLite-side. `LIKE '%needle%'` can't use an index either
/// way, so this costs nothing the Rust-side scan didn't already cost.
///
/// Tags are exempt: they're stored already-lowercased by
/// [`normalise_tags`], and the needle is lowercased too, so a plain
/// `LIKE` is an exact substring test for them.
fn register_lower(conn: &Connection) -> Result<(), BookmarkError> {
    conn.create_scalar_function(
        "buffr_lower",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| Ok(ctx.get::<String>(0)?.to_lowercase()),
    )
    .map_err(|source| BookmarkError::Open { source })?;
    Ok(())
}

/// Escape the `LIKE` metacharacters so a query of `50%` or `a_b`
/// matches literally. Pairs with `ESCAPE '\'` in [`SEARCH_SQL`].
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Upsert one bookmark inside an existing transaction.
///
/// Split out of [`Bookmarks::add`] so `import_netscape` can push
/// thousands of rows through a single transaction (M42) instead of
/// paying one mutex acquisition and one WAL commit per bookmark.
fn add_in_tx(
    tx: &Transaction<'_>,
    url: &str,
    title: Option<&str>,
    tags: &[&str],
) -> Result<BookmarkId, BookmarkError> {
    let canon = canonicalise(url)?;
    let normalised_tags = normalise_tags(tags);
    let title_owned = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let now = Utc::now().timestamp();

    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM bookmarks WHERE url = ?1",
            params![canon],
            |row| row.get(0),
        )
        .optional()?;

    let id = if let Some(id) = existing {
        tx.execute(
            "UPDATE bookmarks SET title = ?1, modified = ?2 WHERE id = ?3",
            params![title_owned, now, id],
        )?;
        tx.execute(
            "DELETE FROM bookmark_tags WHERE bookmark_id = ?1",
            params![id],
        )?;
        id
    } else {
        tx.execute(
            "INSERT INTO bookmarks (url, title, added, modified) VALUES (?1, ?2, ?3, ?3)",
            params![canon, title_owned, now],
        )?;
        tx.last_insert_rowid()
    };

    for tag in &normalised_tags {
        tx.execute(
            "INSERT OR IGNORE INTO bookmark_tags (bookmark_id, tag) VALUES (?1, ?2)",
            params![id, tag],
        )?;
    }

    Ok(BookmarkId(id))
}

/// Hydrate `rows` into [`Bookmark`]s, fetching every row's tags in a
/// single extra query instead of one per row (M40).
fn rows_to_bookmarks(
    conn: &Connection,
    rows: Vec<BookmarkRow>,
) -> Result<Vec<Bookmark>, BookmarkError> {
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let mut tags = load_tags_bulk(conn, &ids)?;
    Ok(rows
        .into_iter()
        .map(|(id, url, title, added, modified)| Bookmark {
            id: BookmarkId(id),
            url,
            title,
            tags: tags.remove(&id).unwrap_or_default(),
            added: ts_to_dt(added),
            modified: ts_to_dt(modified),
        })
        .collect())
}

/// Tags for many bookmarks at once, keyed by bookmark id and sorted
/// alpha within each id (same contract as [`load_tags`]).
///
/// Chunked so the bound-parameter count stays well under
/// `SQLITE_MAX_VARIABLE_NUMBER` no matter how large the profile is.
fn load_tags_bulk(
    conn: &Connection,
    ids: &[i64],
) -> Result<HashMap<i64, Vec<String>>, BookmarkError> {
    const CHUNK: usize = 500;
    let mut out: HashMap<i64, Vec<String>> = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT bookmark_id, tag FROM bookmark_tags \
             WHERE bookmark_id IN ({placeholders}) \
             ORDER BY bookmark_id ASC, tag ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, tag) = row?;
            out.entry(id).or_default().push(tag);
        }
    }
    Ok(out)
}

fn load_tags(conn: &Connection, bookmark_id: i64) -> Result<Vec<String>, BookmarkError> {
    let mut stmt =
        conn.prepare("SELECT tag FROM bookmark_tags WHERE bookmark_id = ?1 ORDER BY tag ASC")?;
    let rows = stmt
        .query_map(params![bookmark_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Parse + canonicalise a URL string.
fn canonicalise(input: &str) -> Result<String, BookmarkError> {
    let trimmed = input.trim();
    let parsed = url::Url::parse(trimmed)?;
    Ok(parsed.to_string())
}

/// Lowercase, trim, dedupe; drop empties.
fn normalise_tags(tags: &[&str]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for t in tags {
        let cleaned = t.trim().to_lowercase();
        if !cleaned.is_empty() {
            set.insert(cleaned);
        }
    }
    set.into_iter().collect()
}

/// Strip a small set of HTML tags from a string, then decode entities.
/// Netscape titles occasionally contain `<B>`, `<I>`, `<BR>`. We don't
/// need a real HTML parser — just drop the angle-bracketed bits.
///
/// Order matters: tags first, entities second. Decoding first would
/// turn a literal `&lt;b&gt;` into `<b>` and the tag-stripper would
/// then eat it.
fn strip_html(s: &str) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("strip_html regex is a constant"));
    decode_entities(re.replace_all(s, "").trim())
        .trim()
        .to_string()
}

/// Decode the five standard XML entities plus numeric character
/// references (`&#NN;`, `&#xHH;`).
///
/// Chrome / Firefox / Edge all escape `&` as `&amp;` in exported
/// HREFs, so without this a real export of
/// `https://example.com/?a=1&b=2` is stored — and later navigated
/// to — as `…?a=1&amp;b=2`. `url::Url::parse` happily accepts the
/// mangled form, which is what made the corruption silent (M41).
///
/// Single pass, left to right: the output is never re-scanned, so
/// `&amp;#38;` correctly yields the literal text `&#38;` rather than
/// double-decoding to `&`. Anything we don't recognise (`&nbsp;`, a
/// bare `&` in a query string) is emitted verbatim.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // Entity bodies are short. Cap the scan so a stray `&` in a
        // query string can't swallow a `;` hundreds of bytes later.
        const MAX_ENTITY_BODY: usize = 12;
        let Some((end, _)) = after
            .char_indices()
            .take(MAX_ENTITY_BODY)
            .find(|(_, c)| *c == ';')
        else {
            out.push('&');
            rest = after;
            continue;
        };
        let body = &after[..end];
        match decode_entity_body(body) {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(body);
                out.push(';');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The text between `&` and `;`. `None` for anything unrecognised.
fn decode_entity_body(body: &str) -> Option<char> {
    match body {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }
    let digits = body.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_runs_migrations() {
        let b = Bookmarks::open_in_memory().unwrap();
        assert_eq!(b.count().unwrap(), 0);
        assert_eq!(schema::latest_version(), 1);
    }

    #[test]
    fn add_three_then_all_orders_most_recent_first() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["foo"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        b.add("https://b.example/", Some("B"), &["foo"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        b.add("https://c.example/", Some("C"), &["foo"]).unwrap();

        assert_eq!(b.count().unwrap(), 3);
        let all = b.all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].url, "https://c.example/");
        assert_eq!(all[1].url, "https://b.example/");
        assert_eq!(all[2].url, "https://a.example/");
    }

    #[test]
    fn add_same_url_twice_upserts() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id1 = b.add("https://a.example/", Some("First"), &["t1"]).unwrap();
        let id2 = b
            .add("https://a.example/", Some("Second"), &["t2"])
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(b.count().unwrap(), 1);
        let bm = b.get(id1).unwrap().expect("exists");
        assert_eq!(bm.title.as_deref(), Some("Second"));
        assert_eq!(bm.tags, vec!["t2"]);
    }

    #[test]
    fn tags_normalised_lowercase_trimmed_deduped_empty_rejected() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id = b
            .add(
                "https://a.example/",
                None,
                &["RUST", "  rust  ", "rust", ""],
            )
            .unwrap();
        let bm = b.get(id).unwrap().expect("exists");
        assert_eq!(bm.tags, vec!["rust"]);
    }

    #[test]
    fn by_tag_filters() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["rust", "lang"])
            .unwrap();
        b.add("https://b.example/", Some("B"), &["python"]).unwrap();
        b.add("https://c.example/", Some("C"), &["rust"]).unwrap();

        let rust_hits = b.by_tag("rust").unwrap();
        assert_eq!(rust_hits.len(), 2);
        let urls: Vec<&str> = rust_hits.iter().map(|x| x.url.as_str()).collect();
        assert!(urls.contains(&"https://a.example/"));
        assert!(urls.contains(&"https://c.example/"));
        // Case-insensitive.
        assert_eq!(b.by_tag("RUST").unwrap().len(), 2);
    }

    #[test]
    fn search_orders_title_url_tag() {
        let b = Bookmarks::open_in_memory().unwrap();
        // Tag-only match.
        b.add("https://other.test/", Some("Other"), &["foobar"])
            .unwrap();
        // URL-only match.
        b.add("https://foobar.example/", Some("Unrelated"), &["nope"])
            .unwrap();
        // Title match.
        b.add("https://x.test/", Some("Foobar Frenzy"), &["nope"])
            .unwrap();

        let hits = b.search("foobar").unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title.as_deref(), Some("Foobar Frenzy"));
        assert_eq!(hits[1].url, "https://foobar.example/");
        assert_eq!(hits[2].url, "https://other.test/");
    }

    #[test]
    fn all_tags_sorted_unique() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", None, &["zeta", "alpha"])
            .unwrap();
        b.add("https://b.example/", None, &["alpha", "mid"])
            .unwrap();
        let tags = b.all_tags().unwrap();
        assert_eq!(tags, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn update_then_get_round_trip() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id = b.add("https://a.example/", Some("Old"), &["t1"]).unwrap();
        let changed = b
            .update(id, Some(Some("New")), Some(&["t2", "t3"]))
            .unwrap();
        assert!(changed);
        let bm = b.get(id).unwrap().expect("exists");
        assert_eq!(bm.title.as_deref(), Some("New"));
        assert_eq!(bm.tags, vec!["t2", "t3"]);
    }

    #[test]
    fn remove_returns_true_then_false() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id = b.add("https://a.example/", None, &[]).unwrap();
        assert!(b.remove(id).unwrap());
        assert!(!b.remove(id).unwrap());
        assert_eq!(b.count().unwrap(), 0);
    }

    #[test]
    fn clear_all_wipes_bookmarks_and_tags() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["t1", "t2"])
            .unwrap();
        b.add("https://b.example/", Some("B"), &["t3"]).unwrap();
        assert_eq!(b.count().unwrap(), 2);
        let removed = b.clear_all().unwrap();
        assert_eq!(removed, 2);
        assert_eq!(b.count().unwrap(), 0);
        // Tags also gone via FK cascade.
        assert!(b.all_tags().unwrap().is_empty());
        // Idempotent.
        assert_eq!(b.clear_all().unwrap(), 0);
    }

    #[test]
    fn invalid_url_errors() {
        let b = Bookmarks::open_in_memory().unwrap();
        let err = b.add("not a url", None, &[]);
        assert!(matches!(err, Err(BookmarkError::Url { .. })));
    }

    const NETSCAPE_FIXTURE: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks</H1>
<DL><p>
    <DT><H3>Rust</H3>
    <DL><p>
        <DT><A HREF="https://rust-lang.org/" ADD_DATE="1700000000">Rust language</A>
        <DT><A HREF="https://crates.io/" ADD_DATE="1700000001" TAGS="package,registry">crates.io</A>
    </DL><p>
    <DT><H3>News</H3>
    <DL><p>
        <DT><A HREF="https://news.example.com/a" ADD_DATE="1700000002">A</A>
        <DT><A HREF="https://news.example.com/b" ADD_DATE="1700000003">B</A>
        <DT><A HREF="https://news.example.com/c" ADD_DATE="1700000004">C</A>
    </DL><p>
</DL><p>
"#;

    #[test]
    fn import_netscape_5_bookmarks_2_folders() {
        let b = Bookmarks::open_in_memory().unwrap();
        let imported = b.import_netscape(NETSCAPE_FIXTURE).unwrap();
        assert_eq!(imported, 5);
        assert_eq!(b.count().unwrap(), 5);

        let rust_hits = b.by_tag("rust").unwrap();
        assert_eq!(rust_hits.len(), 2);
        let news_hits = b.by_tag("news").unwrap();
        assert_eq!(news_hits.len(), 3);

        // TAGS= attribute also imported.
        let pkg = b.by_tag("package").unwrap();
        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0].url, "https://crates.io/");
    }

    #[test]
    fn import_netscape_skips_malformed() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL>
            <DT><A HREF="https://ok.example/">Good</A>
            <DT><A HREF="not a url">Bad</A>
            <DT><A>NoHref</A>
        </DL>"#;
        let imported = b.import_netscape(html).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(b.count().unwrap(), 1);
    }

    #[test]
    fn import_netscape_ignores_markup_inside_anchor_labels() {
        // A hostile/malformed file can put `<H3>` inside an anchor label.
        // The old three-regex tokenizer treated it as a folder open: "x"
        // was pushed onto the folder stack with no `</DL>` to pop it,
        // every later anchor inherited tag "x", and the file's real
        // `</DL>`s popped one level too early. The anchor must be
        // consumed as one token so inner markup never becomes a folder.
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL>
            <DT><A HREF="https://ok.example/">lbl <H3>x</H3></A>
            <DT><A HREF="https://next.example/">Next</A>
            <DT><A HREF="https://third.example/">Third</A>
        </DL>"#;
        let imported = b.import_netscape(html).unwrap();
        assert_eq!(imported, 3);
        // No spurious "x" folder tag anywhere.
        assert!(b.by_tag("x").unwrap().is_empty());
        // Later anchors carry no inherited tag.
        let next = b.search("next.example").unwrap();
        assert_eq!(next.len(), 1);
        assert!(next[0].tags.is_empty());
    }

    // ----- M40: search / by_tag pushed into SQL -----

    #[test]
    fn search_empty_query_returns_all() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["t"]).unwrap();
        b.add("https://b.example/", Some("B"), &[]).unwrap();
        assert_eq!(b.search("   ").unwrap().len(), 2);
        assert_eq!(b.search("").unwrap(), b.all().unwrap());
    }

    #[test]
    fn search_is_case_insensitive_on_both_sides() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://x.test/", Some("MiXeD CaSe"), &[]).unwrap();
        assert_eq!(b.search("mixed case").unwrap().len(), 1);
        assert_eq!(b.search("MIXED CASE").unwrap().len(), 1);
    }

    #[test]
    fn search_folds_non_ascii_case_like_the_old_rust_filter() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://x.test/", Some("ÉCOLE Normale"), &[])
            .unwrap();
        // SQLite's built-in `lower()` is ASCII-only — this only passes
        // because of the registered `buffr_lower`.
        assert_eq!(b.search("école").unwrap().len(), 1);
    }

    #[test]
    fn search_best_rank_wins_when_several_fields_match() {
        let b = Bookmarks::open_in_memory().unwrap();
        // Matches in title AND url AND tag — must appear exactly once,
        // ranked as a title match.
        b.add("https://foobar.test/", Some("Foobar"), &["foobar"])
            .unwrap();
        b.add("https://other.test/", Some("Other"), &["foobar"])
            .unwrap();
        let hits = b.search("foobar").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://foobar.test/");
        assert_eq!(hits[1].url, "https://other.test/");
    }

    #[test]
    fn search_ties_break_by_modified_then_id_desc() {
        let b = Bookmarks::open_in_memory().unwrap();
        // Same second, same rank (all title matches) → id DESC.
        let ids: Vec<BookmarkId> = (0..4)
            .map(|i| {
                b.add(
                    &format!("https://t{i}.example/"),
                    Some(&format!("Tie {i}")),
                    &[],
                )
                .unwrap()
            })
            .collect();
        let hits = b.search("tie").unwrap();
        let got: Vec<BookmarkId> = hits.iter().map(|h| h.id).collect();
        let mut want = ids.clone();
        want.reverse();
        assert_eq!(got, want);
    }

    #[test]
    fn search_limited_caps_rows_but_keeps_ranking() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://zz.example/", Some("Unrelated"), &["needle"])
            .unwrap();
        b.add("https://needle.example/", Some("Unrelated"), &[])
            .unwrap();
        b.add("https://y.example/", Some("Needle Title"), &[])
            .unwrap();

        // Unlimited keeps the full ranked list.
        let all_hits = b.search("needle").unwrap();
        assert_eq!(all_hits.len(), 3);

        // Limited returns the same prefix, not an arbitrary subset.
        let two = b.search_limited("needle", Some(2)).unwrap();
        assert_eq!(two, all_hits[..2].to_vec());
        assert_eq!(b.search_limited("needle", Some(0)).unwrap().len(), 0);
        assert_eq!(b.search_limited("needle", None).unwrap(), all_hits);
        // Empty-query path honours the limit too.
        assert_eq!(b.search_limited("", Some(1)).unwrap().len(), 1);
    }

    #[test]
    fn search_treats_like_metacharacters_literally() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("50% off"), &[]).unwrap();
        b.add("https://b.example/", Some("50 percent off"), &[])
            .unwrap();
        b.add("https://c.example/", Some("a_b"), &[]).unwrap();
        b.add("https://d.example/", Some("axb"), &[]).unwrap();

        let pct = b.search("50%").unwrap();
        assert_eq!(pct.len(), 1);
        assert_eq!(pct[0].title.as_deref(), Some("50% off"));

        let underscore = b.search("a_b").unwrap();
        assert_eq!(underscore.len(), 1);
        assert_eq!(underscore[0].title.as_deref(), Some("a_b"));

        // A backslash in the query is literal too, not an escape.
        b.add("https://e.example/", Some(r"back\slash"), &[])
            .unwrap();
        assert_eq!(b.search(r"back\slash").unwrap().len(), 1);
    }

    #[test]
    fn search_matches_tag_substrings_not_just_whole_tags() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["programming"])
            .unwrap();
        let hits = b.search("gram").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tags, vec!["programming"]);
    }

    #[test]
    fn search_and_by_tag_hydrate_tags_in_bulk() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("Alpha"), &["z", "a", "m"])
            .unwrap();
        b.add("https://b.example/", Some("Alpha two"), &[]).unwrap();

        // Bulk load must still sort alpha within a bookmark, and must
        // leave tagless rows with an empty vec (not drop the row).
        let hits = b.search("alpha").unwrap();
        assert_eq!(hits.len(), 2);
        let a = hits.iter().find(|h| h.url == "https://a.example/").unwrap();
        let b_hit = hits.iter().find(|h| h.url == "https://b.example/").unwrap();
        assert_eq!(a.tags, vec!["a", "m", "z"]);
        assert!(b_hit.tags.is_empty());

        assert_eq!(b.by_tag("m").unwrap()[0].tags, vec!["a", "m", "z"]);
        assert_eq!(b.all().unwrap().len(), 2);
    }

    #[test]
    fn bulk_tag_load_spans_chunk_boundary() {
        let b = Bookmarks::open_in_memory().unwrap();
        // 600 rows > the 500-id chunk used by `load_tags_bulk`.
        for i in 0..600 {
            b.add(&format!("https://n{i}.example/"), Some("Chunky"), &["c"])
                .unwrap();
        }
        let all = b.all().unwrap();
        assert_eq!(all.len(), 600);
        assert!(all.iter().all(|bm| bm.tags == vec!["c"]));
        assert_eq!(b.search("chunky").unwrap().len(), 600);
    }

    // ----- M41: HTML entity decoding on import -----

    #[test]
    fn decode_entities_handles_named_and_numeric() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&quot;q&quot; &apos;a&apos;"), "\"q\" 'a'");
        assert_eq!(decode_entities("&#38;"), "&");
        assert_eq!(decode_entities("&#x26;"), "&");
        assert_eq!(decode_entities("Caf&#xE9;"), "Café");
        // Single pass — no double decoding.
        assert_eq!(decode_entities("&amp;#38;"), "&#38;");
        // Unknown / malformed left verbatim.
        assert_eq!(decode_entities("&nbsp;"), "&nbsp;");
        assert_eq!(decode_entities("a & b"), "a & b");
        assert_eq!(decode_entities("trailing&"), "trailing&");
        assert_eq!(decode_entities("&#x110000;"), "&#x110000;");
        assert_eq!(decode_entities("no entities here"), "no entities here");
    }

    /// Trimmed-down but byte-accurate shape of a Chrome export.
    const NETSCAPE_ESCAPED_FIXTURE: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks</H1>
<DL><p>
    <DT><H3 ADD_DATE="1700000000" LAST_MODIFIED="1700000009">R&amp;D</H3>
    <DL><p>
        <DT><A HREF="https://example.com/?a=1&amp;b=2" ADD_DATE="1700000001">Tom &amp; Jerry</A>
        <DT><A HREF="https://example.com/q?s=%22x%22&amp;t=1" ADD_DATE="1700000002">Quote &quot;x&quot; &#38; more</A>
        <DT><A HREF="https://example.com/e?v=a&#38;w=b" ADD_DATE="1700000003">Caf&#xe9; &lt;b&gt;bold&lt;/b&gt;</A>
    </DL><p>
</DL><p>
"#;

    #[test]
    fn import_netscape_decodes_entities_in_href_and_title() {
        let b = Bookmarks::open_in_memory().unwrap();
        assert_eq!(b.import_netscape(NETSCAPE_ESCAPED_FIXTURE).unwrap(), 3);

        // Folder name entity-decoded before it becomes a tag.
        let folder = b.by_tag("r&d").unwrap();
        assert_eq!(folder.len(), 3);

        let urls: Vec<String> = b.all().unwrap().into_iter().map(|x| x.url).collect();
        assert!(urls.contains(&"https://example.com/?a=1&b=2".to_string()));
        assert!(urls.contains(&"https://example.com/q?s=%22x%22&t=1".to_string()));
        assert!(urls.contains(&"https://example.com/e?v=a&w=b".to_string()));
        // The corrupted forms must not be present.
        assert!(!urls.iter().any(|u| u.contains("&amp;")));

        let titles: Vec<String> = b
            .all()
            .unwrap()
            .into_iter()
            .filter_map(|x| x.title)
            .collect();
        assert!(titles.contains(&"Tom & Jerry".to_string()));
        assert!(titles.contains(&"Quote \"x\" & more".to_string()));
        // Real `<b>` tags stripped; the escaped ones survive as text.
        assert!(titles.contains(&"Café <b>bold</b>".to_string()));
    }

    #[test]
    fn import_netscape_strips_real_tags_but_keeps_escaped_ones() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL>
            <DT><A HREF="https://a.example/"><B>Bold</B> title</A>
        </DL>"#;
        b.import_netscape(html).unwrap();
        let bm = &b.all().unwrap()[0];
        assert_eq!(bm.title.as_deref(), Some("Bold title"));
    }

    // ----- A13: quoted attribute values may contain `>` -----

    #[test]
    fn import_netscape_keeps_gt_inside_quoted_href() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL><DT><A HREF="https://x/?a=1>2&b=3">label</A></DL>"#;
        assert_eq!(b.import_netscape(html).unwrap(), 1);
        assert_eq!(b.count().unwrap(), 1);
        let bm = &b.all().unwrap()[0];
        // `url::Url` percent-encodes `>` in the query, so the stored URL
        // is the full canonical form — the old regex truncated it at the
        // first `>` inside the quoted HREF.
        assert_eq!(bm.url, "https://x/?a=1%3E2&b=3");
        assert_eq!(bm.title.as_deref(), Some("label"));
    }

    #[test]
    fn import_netscape_keeps_gt_inside_quoted_h3_title() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html =
            r#"<DL><DT><H3 TITLE="a>b">Folder</H3><DL><DT><A HREF="https://y/">Y</A></DL></DL>"#;
        assert_eq!(b.import_netscape(html).unwrap(), 1);
        let folder = b.by_tag("folder").unwrap();
        assert_eq!(folder.len(), 1);
        assert_eq!(folder[0].url, "https://y/");
        assert_eq!(folder[0].title.as_deref(), Some("Y"));
    }

    #[test]
    fn import_netscape_normal_anchor_unchanged() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL><DT><A HREF="https://example.com/">Example</A></DL>"#;
        assert_eq!(b.import_netscape(html).unwrap(), 1);
        let bm = &b.all().unwrap()[0];
        assert_eq!(bm.url, "https://example.com/");
        assert_eq!(bm.title.as_deref(), Some("Example"));
    }

    // ----- M42: import is one transaction -----

    #[test]
    fn import_netscape_rolls_back_on_sql_failure() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://pre.example/", Some("Pre-existing"), &[])
            .unwrap();
        {
            let conn = b.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON bookmarks \
                 WHEN new.url LIKE '%boom%' \
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();
        }
        let html = r#"<DL>
            <DT><A HREF="https://one.example/">One</A>
            <DT><A HREF="https://boom.example/">Boom</A>
            <DT><A HREF="https://three.example/">Three</A>
        </DL>"#;
        let err = b.import_netscape(html);
        assert!(
            matches!(err, Err(BookmarkError::Query { .. })),
            "expected a store error, got {err:?}"
        );
        // Nothing from the import survived, but the pre-existing row did.
        assert_eq!(b.count().unwrap(), 1);
        assert_eq!(b.all().unwrap()[0].url, "https://pre.example/");
    }

    #[test]
    fn import_netscape_still_skips_bad_urls_without_aborting() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL>
            <DT><A HREF="https://one.example/">One</A>
            <DT><A HREF="not a url">Bad</A>
            <DT><A HREF="https://three.example/">Three</A>
        </DL>"#;
        assert_eq!(b.import_netscape(html).unwrap(), 2);
        assert_eq!(b.count().unwrap(), 2);
    }

    #[test]
    fn import_netscape_commits_everything_in_one_go() {
        let b = Bookmarks::open_in_memory().unwrap();
        let mut html = String::from("<DL>");
        for i in 0..200 {
            html.push_str(&format!(
                "<DT><A HREF=\"https://bulk{i}.example/\">Bulk {i}</A>"
            ));
        }
        html.push_str("</DL>");
        assert_eq!(b.import_netscape(&html).unwrap(), 200);
        assert_eq!(b.count().unwrap(), 200);
    }

    // ----- M54: refuse a schema from the future -----

    #[test]
    fn schema_newer_than_binary_is_refused() {
        let b = Bookmarks::open_in_memory().unwrap();
        let mut conn = b.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            params![schema::latest_version() + 1],
        )
        .unwrap();
        let err = schema::apply(&mut conn).unwrap_err();
        assert!(
            matches!(err, BookmarkError::SchemaTooNew { .. }),
            "got {err:?}"
        );
    }
}
