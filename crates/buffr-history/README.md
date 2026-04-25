# buffr-history

SQLite-backed browsing history for buffr. Phase 5 data layer — no UI yet.

Wired into the live runtime via the CEF `LoadHandler` / `DisplayHandler` in
[`buffr-core::handlers`](../buffr-core/src/handlers.rs); see
[`PLAN.md`](../../PLAN.md) Phase 5 for the broader feature roadmap.

## Public API

```rust
use buffr_history::{History, Transition};

let h = History::open("/path/to/history.sqlite")?;
h.record_visit("https://example.com/", Some("Example"), Transition::Link)?;

let recent = h.recent(10)?; // newest first
let hits = h.search("exam", 25)?; // frecency-ranked
let removed = h.clear_range(from, to)?; // window delete
```

| Method                         | Purpose                                                          |
| ------------------------------ | ---------------------------------------------------------------- |
| `History::open`                | Open or create DB, apply migrations.                             |
| `History::open_in_memory`      | Ephemeral DB (tests, future private-window profiles).            |
| `History::record_visit`        | Insert (or dedupe-update) a visit.                               |
| `History::update_latest_title` | Patch the most recent visit's title (used by `on_title_change`). |
| `History::search`              | Frecency-ranked substring search over `url` + `title`.           |
| `History::recent`              | Most-recent-first slice.                                         |
| `History::clear_range`         | Delete `[from, to)` window.                                      |
| `History::count`               | Row count (diagnostics).                                         |

`HistoryError` wraps `rusqlite::Error` via `#[from]` so callers don't need a
direct dep on `rusqlite`.

## Schema (v1)

Forward-only migrations, recorded in a `schema_version` table.

```sql
CREATE TABLE visits (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  url           TEXT NOT NULL,
  title         TEXT,
  visit_time    INTEGER NOT NULL,            -- unix epoch seconds
  transition    TEXT NOT NULL DEFAULT 'link' -- link | reload | form_submit | generated | other
);
CREATE INDEX idx_visits_url       ON visits(url);
CREATE INDEX idx_visits_time      ON visits(visit_time DESC);
CREATE INDEX idx_visits_url_time  ON visits(url, visit_time DESC);
```

Pragmas tuned per connection: `journal_mode=WAL`, `synchronous=NORMAL`,
`foreign_keys=ON`.

## Behaviour

- **URL canonicalisation**: every URL goes through `url::Url::parse`, the
  round-trip drops trailing whitespace and normalises case in the scheme/host.
  Unparseable URLs are dropped at `debug` log level — the call returns `Ok(())`
  so `LoadHandler` callbacks don't poison.
- **Skip schemes**: `about:`, `chrome:`, `cef:`, `data:`, `file:` are never
  recorded. Phase 4 will surface this list as a config knob.
- **Dedupe**: if a row exists for the same canonical URL with `visit_time`
  inside the last 60 seconds, that row is updated in place (`visit_time` bumped;
  non-empty title overrides). Otherwise we `INSERT`. Single transaction, no
  UPSERT trickery.

## Frecency formula

```
score = visit_count * 2 + recency_bonus
recency_bonus = 10 if max(visit_time) >= now - 7 days else 0
```

`url LIKE %q% OR title LIKE %q%`, grouped by `url`, ordered by
`score DESC, visit_time DESC`. Substring match is enough for v1.

FTS5 lands in **Phase 5b** with a migration (`MATCH` queries + ranking via
`bm25`). The current schema's indexes don't get in the way of that follow-up.

## Concurrency

`Mutex<rusqlite::Connection>`. Public methods take `&self` and lock per call.
CEF callbacks are short — contention is a non-issue at human typing rates. The
omnibar landing in Phase 3 will likely keep this same model; we'll switch to
`r2d2_sqlite` only if a long-running query starts blocking the load-handler
path.

## Testing

`cargo test -p buffr-history` runs the full suite against an in-memory DB. Time
is injected via a `Clock` trait and `MockClock` helper so dedupe-window boundary
tests don't need to sleep.

## Storage location

Production binary writes to `<data>/history.sqlite`, where `<data>` is whatever
`directories::ProjectDirs::from("sh", "kryptic", "buffr")` resolves — on Linux
that's `~/.local/share/buffr/`. See [`docs/dev.md`](../../docs/dev.md) "Storage"
section.
