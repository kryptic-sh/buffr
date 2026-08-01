# buffr — developer setup

## Prerequisites

- Rust **1.95** — the MSRV (`rust-version` in the root `Cargo.toml`), pinned for
  local builds by `rust-toolchain.toml` (`channel = "1.95.0"`); `rustup`
  installs it automatically on first build. CI does not use the pin: every job
  in `.github/workflows/ci.yml` sets up `stable`.
- A C/C++ toolchain (CEF links against system libraries).
- Linux: `libgtk-3`, `libnss3`, `libnspr4`, `libatk1.0`, `libatk-bridge2.0`,
  `libxcomposite1`, `libxdamage1`, `libxrandr2`, `libxkbcommon0`,
  `libxshmfence1`, `libdrm2`, `libgbm1`, `libpango-1.0`, `libasound2`,
  `libx11-xcb1`, `libcups2`, `libxss1`, `libxtst6`.
- macOS 12+, Xcode command-line tools.
- Windows 10+, MSVC build tools.

For a Mac-specific first-run checklist, including Homebrew packages and the
plain `cargo run` CEF layout, see [`docs/macos-running.md`](./macos-running.md).

## First build

```sh
git clone git@github.com:kryptic-sh/buffr.git
cd buffr

# Vendor the CEF binary distribution (several hundred MB extracted).
# Drops files under `vendor/cef/<platform>/`.
cargo xtask fetch-cef

# Build the workspace (default-members builds all three binaries).
cargo build

# Run. Three binaries exist, so bare `cargo run` is ambiguous — name one.
# `buffr` is the supervisor and spawns `buffr-app` from its own directory.
cargo run --bin buffr
```

`cargo xtask fetch-cef` accepts:

- `--platform <PLATFORM>` (alias `--target`) — override host detection, useful
  when cross-prepping. Accepted values, from `fetch_cef` in `xtask/src/main.rs`:
  `linux64` (default on Linux), `linuxarm64`, `macosarm64`, `macosx64`,
  `windows64`, `windowsarm64`.
- `--version <PREFIX>` — version prefix to match in the Spotify CDN
  (`index.json`). Defaults to `CEF_VERSION_PREFIX` in `xtask/src/main.rs`, which
  must match the libcef version the `cef` crate binds. That pairing is
  load-bearing: `cef 148.x` wraps libcef `147.0.14`, so the prefix is `147.`.

Override the CEF tree location with `CEF_PATH=...` (mirrors
`tauri-apps/cef-rs`). When unset, `crates/buffr-cef/build.rs` falls back to
`vendor/cef/<platform>/`.

`vendor/cef/` is in `.gitignore`. Re-run `cargo xtask fetch-cef` after bumping
the `cef` crate version.

## Layout

```
buffr/
├── apps/
│   ├── buffr/              # supervisor binary (spawns + restarts buffr-app)
│   ├── buffr-app/          # browser binary (window, CEF lifecycle, chrome)
│   ├── buffr-helper/       # CEF subprocess helper (macOS Helper.app)
│   └── buffr-poc/          # EXCLUDED from the workspace — see below
├── crates/
│   ├── buffr-engine/       # BrowserEngine trait, routing, buffr:// server
│   ├── buffr-cef/          # CEF integration: host, handlers, build.rs
│   ├── buffr-core/         # engine-agnostic core: hints, edit, updates, …
│   ├── buffr-modal/        # vim page-mode FSM + keymap trie
│   ├── buffr-ui/           # chrome: statusline, tab strip, input bar
│   ├── buffr-config/       # config loading (TOML) + hot reload
│   ├── buffr-store/        # shared SQLite open/tune + migration runner
│   ├── buffr-history/      # history store
│   ├── buffr-bookmarks/    # bookmark store + Netscape import
│   ├── buffr-downloads/    # download tracking
│   ├── buffr-zoom/         # per-domain zoom persistence
│   ├── buffr-permissions/  # per-origin permission store
│   ├── buffr-view-source/  # buffr-src: rendering
│   └── buffr-webkit/       # EXCLUDED from the workspace — see below
├── xtask/                  # cargo xtask: fetch-cef, packaging
├── fuzz/                   # EXCLUDED from the workspace (cargo-fuzz)
├── vendor/cef/             # downloaded CEF binaries (gitignored)
├── docs/                   # this file
└── TODO.md                 # near-term task list
```

`crates/buffr-webkit` (an experimental WPE WebKit backend) and `apps/buffr-poc`
(a Wayland subsurface-embedding proof of concept built on it) are in the
`exclude` list in the root `Cargo.toml`. They are Linux-only, need
`wpewebkit-2.0` system packages that CI does not install, and are **not built by
CI**. Build them by hand:

```sh
cargo build --manifest-path crates/buffr-webkit/Cargo.toml
cargo build --manifest-path apps/buffr-poc/Cargo.toml
```

## Running

```sh
RUST_LOG=buffr=debug,buffr_core=debug cargo run --bin buffr
```

To run the browser directly (without supervision):

```sh
RUST_LOG=buffr_app=debug,buffr_core=debug cargo run --bin buffr-app
```

### Wayland

**Linux requires a Wayland session.** `buffr-app` checks `XDG_SESSION_TYPE` at
startup — before CEF init or any window creation — and exits with a clear
message when it is not `wayland`. X11/XWayland is not a supported target.

The page is rendered off-screen (CEF windowless mode) and composited with the
chrome into one window via `wgpu` on every platform; there is no XWayland
round-trip and no CEF child window on Linux. See
[`docs/ui-stack.md`](./ui-stack.md).

## macOS bundling

CEF on macOS requires a strict app-bundle layout: the libcef framework must live
at `Contents/Frameworks/Chromium Embedded Framework.framework/`, and CEF's
helper subprocesses must be launched out of a nested
`Contents/Frameworks/Buffr Helper.app/`. The main binary loads the framework at
startup via `cef-rs`'s `LibraryLoader` (`helper=false`); the helper does the
same with `helper=true` so the framework path resolves relative to its own
deeper bundle position (`../../..` vs `../Frameworks`).

The `xtask bundle-macos` subcommand assembles all of this:

```sh
# Vendor a macOS CEF distribution (cross-fetch from a Linux dev box is fine).
cargo xtask fetch-cef --platform macosarm64

# Build + assemble Buffr.app under target/release/.
cargo xtask bundle-macos --release

# Optional ad-hoc signing (gatekeeper-bypassed local runs only).
codesign --force --deep --sign - target/release/Buffr.app

# Run.
open target/release/Buffr.app
```

Notes:

- The compiled helper binary is `buffr-helper` (with hyphen) but the bundle
  convention renames it to `Buffr Helper` (space-separated) during the copy. No
  Cargo changes needed.
- The bundle ships the full four-helper layout macOS's sandbox model expects:
  `Buffr Helper.app`, `Buffr Helper (GPU).app`, `Buffr Helper (Renderer).app`,
  and `Buffr Helper (Plugin).app`, each with its own plist from
  `xtask/templates/`. The bundle test in `xtask/src/main.rs` asserts all four
  exist.
- No `buffr.icns` is bundled yet; the plist references the file so Finder picks
  it up once we ship one. Until then macOS uses a generic app icon.
- The bundle script runs on Linux too — useful for catching script regressions
  in CI without booting a macOS runner. Real macOS CEF framework not on disk?
  Set `BUFFR_BUNDLE_FRAMEWORK_DIR=<any-dir>` to short-circuit the
  framework-existence check; bundle assembly still finishes, the resulting app
  just won't run.
- Distribution-grade signing + notarization is documented in
  [`docs/macos-signing.md`](./macos-signing.md). Phase 6 work.

## Linux packaging

Four Linux distribution paths — `deb`, `rpm`, `tarball`, `aur` — all producible
from a single Linux dev box:

```sh
cargo xtask package-linux --release --variant all
ls target/dist/linux/
# buffr-<version>-amd64.deb
# buffr-<version>-x86_64.rpm
# buffr-<version>-x86_64.tar.gz
```

`<version>` is the workspace version from the root `Cargo.toml`; the xtask
stamps it into every filename, so there is nothing to keep in sync by hand.

`dpkg-deb` is auto-detected; when it is missing the xtask leaves the staging
tree at `target/<profile>/buffr-deb/` and prints a warning rather than failing.
The AUR PKGBUILD is regenerated at `pkg/aur/PKGBUILD` with the current workspace
version on every run.

Full guide (layout, depends, glibc, sandbox caveats, signing TODO):
[`docs/packaging.md`](./packaging.md).

## Crash-restart supervisor

`buffr` IS the supervisor — it spawns `buffr-app` (the browser binary) as a
child process group, detects non-zero exit and UI hangs (via a heartbeat
socket), and relaunches it with a 250 ms cooldown. After 3 crashes/hangs in 30
seconds it halts and points at `~/.local/share/buffr/crashes/`. Unix and Windows
both have supervisor implementations (the Windows half uses Job Objects and a
named-pipe heartbeat).

The heartbeat socket and the clean-shutdown flag live inside a private per-uid
directory — `$XDG_RUNTIME_DIR/buffr`, else `$TMPDIR/buffr-<uid>` created `0700`
and re-verified (owner + mode + not-a-symlink) after creation.

```sh
# Default: supervisor auto-spawns buffr-app (found next to its own exe,
# then on $PATH).
./buffr

# Smoke-test the supervisor without a real browser binary:
BUFFR_CHILD_BIN=/bin/true ./buffr

# Tune or disable the hang watchdog:
./buffr --heartbeat-timeout 8
./buffr --heartbeat-disable
```

## Useful commands

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Where things live

| Concern                   | File                                        |
| ------------------------- | ------------------------------------------- |
| Supervisor / restart loop | `apps/buffr/src/main.rs`                    |
| Subprocess dispatch       | `apps/buffr-app/src/main.rs::main`          |
| `cef::App` impl           | `crates/buffr-cef/src/app.rs`               |
| Browser creation          | `crates/buffr-cef/src/host.rs`              |
| CEF callback handlers     | `crates/buffr-cef/src/handlers.rs`          |
| CEF link + resource copy  | `crates/buffr-cef/build.rs`                 |
| CEF download              | `xtask/src/main.rs::fetch_cef`              |
| Engine trait + routing    | `crates/buffr-engine/src/engine.rs`         |
| Page mode FSM             | `crates/buffr-modal/src/lib.rs`             |
| `hjkl-engine` integration | `crates/buffr-modal/src/edit_mode.rs`       |
| Statusline + font         | `crates/buffr-ui/src/lib.rs`                |
| Find-in-page sink         | `crates/buffr-core/src/find.rs`             |
| Hint / edit console IPC   | `crates/buffr-core/src/console_sentinel.rs` |
| Config schema + loader    | `crates/buffr-config/src/lib.rs`            |
| Shared SQLite plumbing    | `crates/buffr-store/src/lib.rs`             |
| History store             | `crates/buffr-history/src/lib.rs`           |
| Bookmarks store           | `crates/buffr-bookmarks/src/lib.rs`         |
| Downloads store           | `crates/buffr-downloads/src/lib.rs`         |

## UI

Chrome (statusline, tab strip, input bar, prompts) lives in `crates/buffr-ui`.
Rendering decisions are in [`docs/ui-stack.md`](./ui-stack.md): CEF renders the
page off-screen and the app composites page + chrome into one `winit` window
with `wgpu` on every platform. The 30-pixel statusline
(`buffr_ui::STATUSLINE_HEIGHT`) rasterizes glyphs with `fontdue` at a fixed 15
px, with per-glyph advance widths (`crates/buffr-ui/src/font.rs`). Find-in-page
is wired through `BrowserHost::start_find` / `stop_find`; the `--find <query>`
flag on `buffr-app` exercises the round trip headlessly.

## Storage

Per-user state resolves through `hjkl-config`'s XDG helpers — `$XDG_DATA_HOME`
(default `~/.local/share`) and `$XDG_CACHE_HOME` (default `~/.cache`), with
`buffr` as the directory name (`buffr-debug` in debug builds). The `directories`
crate is not used:

| Path                                            | Owner                                                                                           |
| ----------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `~/.local/share/buffr/` (CEF `root_cache_path`) | Cookies, `Local Storage`, IndexedDB, HTTP `Cache`, GPU shader cache.                            |
| `~/.local/share/buffr/engines/<id>/`            | Per-engine namespace. Computed and passed, but CEF ignores it — see [`config.md`](./config.md). |
| `~/.local/share/buffr/history.sqlite`           | History DB (`buffr-history`).                                                                   |
| `~/.local/share/buffr/bookmarks.sqlite`         | Bookmarks DB (`buffr-bookmarks`).                                                               |
| `~/.local/share/buffr/downloads.sqlite`         | Downloads DB (`buffr-downloads`).                                                               |
| `~/.local/share/buffr/zoom.sqlite`              | Per-site zoom levels (`buffr-zoom`).                                                            |
| `~/.local/share/buffr/permissions.sqlite`       | Per-origin permission decisions (`buffr-permissions`).                                          |
| `~/.local/share/buffr/favicons.sqlite`          | Favicon cache (`buffr_core::FaviconCache`).                                                     |
| `~/.local/share/buffr/session.json`             | Saved tab session (see [`multi-tab.md`](./multi-tab.md)).                                       |
| `~/.local/share/buffr/launch.json`              | Crash-loop tracker (`apps/buffr-app/src/crash_guard.rs`).                                       |
| `~/.local/share/buffr/usage-counters.json`      | Opt-in local telemetry counters (off by default).                                               |
| `~/.local/share/buffr/crashes/`                 | Opt-in panic reports, `<stamp>_<seq>.json` (off by default).                                    |
| `~/.local/share/buffr/update-cache.json`        | Cached GitHub release check (see [`updates.md`](./updates.md)).                                 |
| `~/.cache/buffr/`                               | Created at startup; used to derive the single-instance profile id. CEF stores nothing here.     |

**CEF state is under `XDG_DATA_HOME`, not `XDG_CACHE_HOME`.** `buffr-app` passes
the data dir as CEF's `root_cache_path`, so cookies and local storage sit
alongside the SQLite stores. The XDG spec allows `~/.cache` contents to be
deleted without warning, which is not survivable for a browser profile.

`history.sqlite` runs in WAL mode, so you'll also see `history.sqlite-wal` /
`history.sqlite-shm` next to it during a live session — that's normal. Schema
migrations are forward-only and recorded in a `schema_version` table; the
migration runner is `crates/buffr-store/src/lib.rs` and the frecency query lives
in `crates/buffr-history/src/lib.rs`.

macOS and Windows use the **same** XDG layout — there is no
`~/Library/Application Support` or `%APPDATA%` special case, and
`$XDG_DATA_HOME` / `$XDG_CACHE_HOME` are honored everywhere.

## Config

`buffr-config` reads `~/.config/buffr/config.toml` — the same path on Linux,
macOS, and Windows, with `$XDG_CONFIG_HOME` honored everywhere (debug builds use
`buffr-debug`). Schema reference: [`docs/config.md`](./config.md). A
copy-pasteable defaults file ships at
[`config.example.toml`](../config.example.toml) at the repo root — drop it into
`$XDG_CONFIG_HOME/buffr/config.toml` to start customising.

```sh
buffr --check-config            # validate ~/.config/buffr/config.toml
buffr --print-config            # dump the resolved (defaults + overrides) TOML
buffr --config /tmp/foo.toml    # use a non-default path
buffr --homepage about:blank    # override general.homepage for one run
```

## Bookmarks

`buffr-bookmarks` ships an SQLite-backed bookmark store with tag support and a
Netscape HTML importer. Schema and Netscape parsing notes are in the module docs
at `crates/buffr-bookmarks/src/lib.rs`. There's no bookmarks UI yet; the CLI
flags exist for import + debugging:

```sh
# Import a Netscape HTML export (Chrome / Firefox / Edge "Export bookmarks…").
buffr --import-bookmarks ~/Downloads/bookmarks.html
# Stdout: `imported N bookmarks`

# List every stored bookmark (id\turl\ttitle\t[tag,tag]).
buffr --list-bookmarks

# List every distinct tag, sorted alphabetically.
buffr --list-bookmarks-tags
```

All three flags short-circuit before CEF init, so they work without a display
server.

## Zoom

`buffr-zoom` ships an SQLite-backed per-site zoom-level store. The CEF
`LoadHandler::on_load_end` callback restores the persisted level for the domain
on every load; the `ZoomIn` / `ZoomOut` / `ZoomReset` page actions write
through. Schema lives in the module docs at `crates/buffr-zoom/src/lib.rs`.

```sh
# Print every override (`<domain>\t<level>`).
buffr --list-zoom

# Wipe every override.
buffr --clear-zoom
```

Both flags short-circuit before CEF init.

## Private mode

```sh
buffr --private
```

Private mode roots the entire profile under a `tempfile::TempDir`
(`$TMPDIR/buffr-private-<pid>-<rand>/{cache,data}`) and opens every SQLite store
in-memory. The tempdir is deleted on shutdown; nothing persists across restarts.
The window title is stamped `buffr — PRIVATE — NORMAL` so the privacy state is
obvious from the taskbar.

Caveats:

- This is single-window incognito, not Tor-Browser-grade compartmentalisation.
  There is no IPC isolation from other buffr processes; running a persistent and
  a private buffr concurrently shares the same renderer/GPU service-worker pool.
- The clear-on-exit hook is a no-op in private mode — the tempdir's `Drop`
  already removes everything.
- Multi-profile / per-window incognito (one persistent window plus one private
  window in the same process) is **not implemented** — `--private` is a
  whole-process switch.

## Clear-on-exit

`[privacy] clear_on_exit` (in `config.toml`) lists data categories that buffr
wipes after the event loop returns and before `cef::shutdown()`. Cookies route
through CEF's global cookie manager; history / bookmarks / downloads call
`clear_all` on their respective stores. See
[`config.example.toml`](../config.example.toml) for the full list of valid
entries.

The `cache` and `local_storage` entries are **currently broken**:
`run_clear_on_exit` in `apps/buffr-app/src/main.rs` deletes
`<XDG_CACHE_HOME>/buffr/Cache` and `<XDG_CACHE_HOME>/buffr/Local Storage`, but
CEF writes both under its `root_cache_path`, which is the data dir. Both deletes
therefore hit a directory that was never populated and log
`clear_on_exit: dir absent — skipping`.
