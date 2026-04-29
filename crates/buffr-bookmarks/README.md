# buffr-bookmarks

SQLite-backed bookmark store for buffr. Phase 5 data layer — no UI yet.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Wired into the live runtime via an `Arc<Bookmarks>` passed to `BrowserHost::new`
alongside `Arc<History>`. Unlike history, no CEF callback writes here
automatically — bookmarks are user-action-driven; storage is populated via the
import CLI or (Phase 5b) an omnibar / chrome action.

## Public API

```rust
use buffr_bookmarks::{Bookmarks, BookmarkId};

let b = Bookmarks::open("/path/to/bookmarks.sqlite")?;
let id = b.add("https://example.com/", Some("Example"), &["news", "daily"])?;
let bm = b.get(id)?;
let all = b.all()?;                // most-recently-modified first
let rust = b.by_tag("rust")?;
let hits = b.search("example")?;   // title > url > tag, then modified DESC
let tags = b.all_tags()?;          // sorted alpha
let imported = b.import_netscape(&html)?;
b.update(id, Some(Some("New title")), Some(&["new", "tags"]))?;
b.remove(id)?;
```

| Method                       | Purpose                                                          |
| ---------------------------- | ---------------------------------------------------------------- |
| `Bookmarks::open`            | Open or create DB, apply migrations.                             |
| `Bookmarks::open_in_memory`  | Ephemeral DB (tests, future private-window profiles).            |
| `Bookmarks::add`             | Upsert by URL — existing rows get title/tags/modified rewritten. |
| `Bookmarks::remove`          | Delete by id; returns `true` iff a row went away.                |
| `Bookmarks::update`          | Patch title and/or tags on an existing id.                       |
| `Bookmarks::get`             | Fetch one bookmark by id.                                        |
| `Bookmarks::all`             | All bookmarks, `modified DESC`.                                  |
| `Bookmarks::by_tag`          | All bookmarks with a given tag (case-insensitive).               |
| `Bookmarks::search`          | Substring search over url / title / tag with rank ordering.      |
| `Bookmarks::all_tags`        | Distinct tags sorted alphabetically.                             |
| `Bookmarks::import_netscape` | Bulk-import a Netscape Bookmark File (HTML).                     |

`BookmarkError` wraps `rusqlite::Error` and `url::ParseError` via `#[from]` so
callers don't need a direct dep on either.

## Schema (v1)

Forward-only migrations recorded in a `schema_version` table — same pattern as
`buffr-history`.

```sql
CREATE TABLE bookmarks (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  url         TEXT NOT NULL UNIQUE,
  title       TEXT,
  added       INTEGER NOT NULL,           -- unix epoch seconds
  modified    INTEGER NOT NULL
);
CREATE TABLE bookmark_tags (
  bookmark_id INTEGER NOT NULL REFERENCES bookmarks(id) ON DELETE CASCADE,
  tag         TEXT NOT NULL,
  PRIMARY KEY (bookmark_id, tag)
);
CREATE INDEX idx_bookmark_tags_tag    ON bookmark_tags(tag);
CREATE INDEX idx_bookmarks_modified   ON bookmarks(modified DESC);
```

Pragmas per connection: `journal_mode=WAL`, `synchronous=NORMAL`,
`foreign_keys=ON`.

## Behaviour

- **URL canonicalisation**: every URL goes through `url::Url::parse`.
  Unparseable URLs return `BookmarkError::Url` (unlike the history path, which
  silently drops them — bookmarks are an explicit user action and a bad URL
  should be loud).
- **Upsert by URL**: `add()` matches on the canonical URL. If the row already
  exists, `title`, `tags`, and `modified` are overwritten; the original `added`
  timestamp is preserved.
- **Tag normalisation**: trim, lowercase, dedupe, drop empty entries. So
  `["RUST", "  rust  ", "rust", ""]` stores as `["rust"]`.
- **Search ordering**: title-substring matches outrank url-substring matches
  outrank tag-substring matches; ties break by `modified DESC`. Each bookmark
  appears at most once.

## Netscape import

`import_netscape(&self, html: &str)` parses the Netscape Bookmark File Format —
the de-facto export from Chrome, Firefox, and Edge. Uses four small regexes
rather than a full HTML parser; folder names from enclosing `<H3>` tags are
added as tags. Malformed entries (unparseable URL, missing `HREF`) are skipped;
the returned count reflects only successful inserts/upserts.

## Concurrency

`Mutex<rusqlite::Connection>`. Public methods take `&self` and lock per call.
Same model as `buffr-history`; see that crate's notes.

## Testing

`cargo test -p buffr-bookmarks` runs the full suite against an in-memory DB.

## Storage location

Production binary writes to `<data>/bookmarks.sqlite`; on Linux that's
`~/.local/share/buffr/bookmarks.sqlite`. See [`docs/dev.md`](../../docs/dev.md)
"Storage" section.

## License

MIT. See [LICENSE](../../LICENSE).
