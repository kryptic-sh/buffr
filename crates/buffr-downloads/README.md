# buffr-downloads

SQLite-backed download tracking for buffr. Phase 5 data layer.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Pure data layer — no UI, no IPC. CEF wires events into the store via
`BuffrDownloadHandler` in `buffr-core`; callers (`apps/buffr`, future downloads
pane) read through the `Downloads` handle.

## Public API

```rust
use buffr_downloads::{Downloads, DownloadId, DownloadStatus};
use std::path::Path;

let store = Downloads::open("/var/lib/buffr/downloads.sqlite")?;

// Record a new in-flight download (idempotent on cef_id).
let id = store.record_started(
    /* cef_id */    7,
    /* url */       "https://example.com/file.zip",
    /* suggested */ "file.zip",
    /* mime */      Some("application/zip"),
    /* total */     Some(1024),
)?;

store.update_progress(id, 512, Some(1024))?;
store.record_completed(id, Path::new("/home/me/Downloads/file.zip"))?;

// Read.
let recent = store.all(20)?;
let active = store.in_flight()?;
let cleared = store.clear_completed()?;
```

`record_started` is **idempotent** on `cef_id`: the same `cef_id` always maps to
the same `DownloadId`. CEF can re-fire `OnBeforeDownload` between cancel + retry
— no duplicate rows.

`update_progress` is a **no-op** for terminal statuses (`Completed` / `Canceled`
/ `Failed`). CEF occasionally emits one final tick after `is_complete()` flips
to true.

`clear_completed` only deletes `Completed` rows. `Failed` and `Canceled` rows
stay so the user can see why a download didn't land.

## Schema

```sql
CREATE TABLE downloads (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  cef_id          INTEGER NOT NULL UNIQUE,
  url             TEXT NOT NULL,
  suggested_name  TEXT NOT NULL,
  mime            TEXT,
  total_bytes     INTEGER,
  received_bytes  INTEGER NOT NULL DEFAULT 0,
  status          TEXT NOT NULL,
  started_at      INTEGER NOT NULL,
  finished_at     INTEGER,
  full_path       TEXT,
  failure         TEXT
);
CREATE INDEX idx_downloads_status ON downloads(status, started_at DESC);
CREATE INDEX idx_downloads_cef_id ON downloads(cef_id);
```

`status` is a TEXT tag (`'in_flight' | 'completed' | 'canceled' | 'failed'`) so
adding new variants doesn't need a migration. Forward-only migrations tracked in
a `schema_version` table.

## CEF integration

`BuffrDownloadHandler` in `buffr-core::handlers` implements `DownloadHandler`.
Two callbacks fire:

- **`on_before_download`** — once per download; resolves a target path, calls
  `BeforeDownloadCallback::cont`, and `record_started` the row.
- **`on_download_updated`** — many times; calls `update_progress` while
  in-flight; on `is_complete() == true` flips the row via `record_completed`
  (also handling `is_canceled` / `is_interrupted`).

## `open_on_finish` caveats

When `buffr_config::DownloadsConfig::open_on_finish` is `true` and a download
completes, `buffr-core` spawns the platform open command:

| Platform | Command                  |
| -------- | ------------------------ |
| Linux    | `xdg-open <path>`        |
| macOS    | `open <path>`            |
| Windows  | `cmd /c start "" <path>` |

A spawn failure logs at `warn` and is otherwise silent.

## Concurrency

`Mutex<rusqlite::Connection>`. Public methods take `&self` and lock per call.
Same model as `buffr-history`.

## Storage location

Production binary writes to `<data>/downloads.sqlite`; on Linux that's
`~/.local/share/buffr/downloads.sqlite`. See [`docs/dev.md`](../../docs/dev.md)
"Storage" section.

## License

MIT. See [LICENSE](../../LICENSE).
