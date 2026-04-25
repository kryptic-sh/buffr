# buffr-downloads

SQLite-backed download tracking for [buffr](https://github.com/kryptic-sh/buffr)
(Phase 5).

Pure data layer — no UI, no IPC. CEF wires events into the store via
`BuffrDownloadHandler` in `buffr-core`; callers (`apps/buffr`, future
downloads pane) read through the [`Downloads`] handle.

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

`record_started` is **idempotent** on `cef_id`: the same `cef_id` always
maps to the same [`DownloadId`]. CEF can re-fire `OnBeforeDownload`
between cancel + retry, and we don't want duplicate rows.

`update_progress` is a **no-op** for terminal statuses
([`Completed`](DownloadStatus::Completed) /
[`Canceled`](DownloadStatus::Canceled) /
[`Failed`](DownloadStatus::Failed)). CEF occasionally emits one final
tick after `is_complete()` flips to true.

`clear_completed` only deletes [`Completed`](DownloadStatus::Completed)
rows. [`Failed`](DownloadStatus::Failed) and
[`Canceled`](DownloadStatus::Canceled) stay so the user can see why a
download didn't land.

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

`status` is a TEXT tag (`'in_flight' | 'completed' | 'canceled' |
'failed'`) so adding new variants doesn't need a migration. Forward-only
migrations are tracked in a `schema_version` table — see
[`schema`](src/schema.rs).

## CEF integration notes

`BuffrDownloadHandler` (in `buffr-core::handlers`) implements
`DownloadHandler`. Two callbacks fire:

- **`on_before_download`** — once per download, immediately. We resolve
  a target path under the configured `default_dir`, call
  `BeforeDownloadCallback::cont(target_path, /* show_dialog */ 0)`,
  and `record_started` the row.
- **`on_download_updated`** — many times per download. We
  `update_progress` while in-flight, and on `is_complete() == true`
  flip the row via `record_completed` (also handling `is_canceled` /
  `is_interrupted`).

The CEF API exposes byte counts as signed `i64`. We cast to `u64` for
storage; in practice neither value approaches `i64::MAX` for any
realistic download.

## `open_on_finish` caveats

When the resolved [`buffr_config::DownloadsConfig::open_on_finish`] is
`true` and a download flips to [`Completed`](DownloadStatus::Completed),
`buffr-core` spawns the platform open command:

| Platform | Command                  |
| -------- | ------------------------ |
| Linux    | `xdg-open <path>`        |
| macOS    | `open <path>`            |
| Windows  | `cmd /c start "" <path>` |

A spawn failure logs at `warn` and is otherwise silent — buffr never
blocks the browser thread on the user's launcher misbehaving. See
`crates/buffr-core/src/open_finder.rs`.

## License

MIT (workspace-inherited).
