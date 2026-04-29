# buffr — developer setup

## Prerequisites

- Rust **1.95** (pinned via `rust-toolchain.toml`; `rustup` will install
  automatically on first build).
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

# Vendor the CEF binary distribution (~500 MB extracted).
# Drops files under `vendor/cef/<platform>/`.
cargo xtask fetch-cef

# Build the workspace.
cargo build

# Run.
cargo run -p buffr-bin
```

`cargo xtask fetch-cef` accepts:

- `--platform <linux64 | macosarm64 | macosx64 | windows64>` — override the host
  detection (useful when cross-prepping).
- `--version <X.Y>` — version prefix to match in the Spotify CDN (`index.json`).
  Defaults to `147.` to track the `cef` crate.

Override the CEF tree location with `CEF_PATH=...` (mirrors
`tauri-apps/cef-rs`). When unset, `buffr-core/build.rs` falls back to
`vendor/cef/<platform>/`.

## CEF binary distribution — size + platform matrix

| Platform     | Archive (compressed) | Extracted | Notes                                |
| ------------ | -------------------- | --------- | ------------------------------------ |
| `linux64`    | ~140 MB              | ~480 MB   | Tier 1 (primary dev target).         |
| `macosarm64` | ~150 MB              | ~520 MB   | Tier 1 (`cargo xtask bundle-macos`). |
| `macosx64`   | ~150 MB              | ~520 MB   | Tier 1.                              |
| `windows64`  | ~165 MB              | ~530 MB   | Tier 2.                              |

`vendor/cef/` is in `.gitignore`. Re-run `cargo xtask fetch-cef` after bumping
the `cef` crate version.

## Layout

```
buffr/
├── apps/
│   ├── buffr/         # main binary (browser process)
│   └── buffr-helper/  # CEF subprocess helper (macOS Helper.app)
├── crates/
│   ├── buffr-core/    # CEF lifecycle + browser host + build.rs
│   ├── buffr-modal/   # vim-style mode + keybind engine
│   ├── buffr-ui/      # chrome, command palette, hint overlay
│   └── buffr-config/  # config loading (TOML)
├── xtask/             # cargo xtask: fetch-cef, etc.
├── vendor/cef/        # downloaded CEF binaries (gitignored)
├── docs/              # this file
└── PLAN.md            # phase roadmap
```

## Running

```sh
RUST_LOG=buffr=debug,buffr_core=debug cargo run -p buffr-bin
```

### Wayland

The default build embeds CEF as a windowed child of an X11 window — that's the
only mode CEF supports on Linux. On Wayland sessions buffr forces winit's X11
backend at startup (`EventLoopBuilderExtX11::with_x11()`), so the compositor
transparently proxies the X11 traffic through XWayland.

This works on every major Wayland desktop that ships XWayland — GNOME, KDE,
Sway, Hyprland — which is the default on essentially every distribution. Minimal
compositors without XWayland (e.g. a stock `weston` build) won't work until
native Wayland support lands.

Native Wayland (no XWayland round-trip) is Phase 3 work, gated behind the `osr`
feature:

```sh
# Currently panics at runtime — only compiles. Tracking issue: PLAN.md Phase 3.
cargo run -p buffr-bin --features osr
```

The OSR path will run CEF in windowless mode, blitting paint events onto a
winit-owned Wayland surface via wgpu.

## macOS bundling

CEF on macOS requires a strict app-bundle layout: the libcef framework must live
at `Contents/Frameworks/Chromium Embedded Framework.framework/`, and CEF's
helper subprocesses must be launched out of a nested
`Contents/Frameworks/buffr Helper.app/`. The main binary loads the framework at
startup via `cef-rs`'s `LibraryLoader` (`helper=false`); the helper does the
same with `helper=true` so the framework path resolves relative to its own
deeper bundle position (`../../..` vs `../Frameworks`).

The `xtask bundle-macos` subcommand assembles all of this:

```sh
# Vendor a macOS CEF distribution (cross-fetch from a Linux dev box is fine).
cargo xtask fetch-cef --platform macosarm64

# Build + assemble buffr.app under target/release/.
cargo xtask bundle-macos --release

# Optional ad-hoc signing (gatekeeper-bypassed local runs only).
codesign --force --deep --sign - target/release/buffr.app

# Run.
open target/release/buffr.app
```

Notes:

- The compiled helper binary is `buffr-helper` (with hyphen) but the bundle
  convention renames it to `buffr Helper` (space-separated) during the copy. No
  Cargo changes needed.
- This round ships a single `buffr Helper.app` used for all subprocess types.
  macOS's full sandbox model wants `Helper`, `Helper (GPU)`,
  `Helper (Renderer)`, and `Helper (Plugin)` — that split is deferred to Phase 6
  when proper signing + sandbox entitlements land.
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

Three Linux distribution paths, all producible from a single Linux dev box:

```sh
cargo xtask package-linux --release --variant all
ls target/dist/linux/
# buffr-0.0.1-x86_64.AppImage
# buffr-0.0.1-amd64.deb
```

`appimagetool` and `dpkg-deb` are auto-detected; if either is missing the xtask
leaves the staging directory in place and prints a warning rather than failing.
The AUR PKGBUILD is regenerated at `pkg/aur/PKGBUILD` with the current workspace
version on every run.

Full guide (layout, depends, glibc, sandbox caveats, signing TODO):
[`docs/packaging.md`](./packaging.md).

## Useful commands

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Where things live

| Concern                   | File                                |
| ------------------------- | ----------------------------------- |
| Subprocess dispatch       | `apps/buffr/src/main.rs::main`      |
| `cef::App` impl           | `crates/buffr-core/src/app.rs`      |
| Browser creation          | `crates/buffr-core/src/host.rs`     |
| CEF callback handlers     | `crates/buffr-core/src/handlers.rs` |
| CEF link + resource copy  | `crates/buffr-core/build.rs`        |
| CEF download              | `xtask/src/main.rs::fetch_cef`      |
| Page mode FSM             | `crates/buffr-modal/src/lib.rs`     |
| `hjkl-engine` integration | `crates/buffr-modal/src/host.rs`    |
| Statusline + bitmap font  | `crates/buffr-ui/src/lib.rs`        |
| Find-in-page sink         | `crates/buffr-core/src/find.rs`     |
| Config schema + loader    | `crates/buffr-config/src/lib.rs`    |
| History store             | `crates/buffr-history/src/lib.rs`   |
| Bookmarks store           | `crates/buffr-bookmarks/src/lib.rs` |
| Downloads store           | `crates/buffr-downloads/src/lib.rs` |

## UI

Phase 3 chrome (statusline today; tab strip / command bar / hint mode later)
lives in `crates/buffr-ui`. Rendering decisions are in
[`docs/ui-stack.md`](./ui-stack.md): the page and chrome are composited into
the buffr `winit` window on Linux/macOS, while Windows uses a native CEF child
window for the page area. The 24-pixel statusline draws via a hand-rolled 6x10 bitmap font in
`crates/buffr-ui/src/font.rs`. Find-in-page is wired through
`BrowserHost::start_find` / `stop_find`; a `--find <query>` CLI flag on
`apps/buffr` exercises the round trip without a command bar (the Phase 3b
dependency that blocks `Find { forward }` action UI).

## Storage

Per-user state buffr writes lives under `directories::ProjectDirs` resolution
for `sh.kryptic.buffr`. On Linux that's:

| Path                                       | Owner                                                           |
| ------------------------------------------ | --------------------------------------------------------------- |
| `~/.cache/buffr/`                          | CEF cache (cookies, GPU shader cache).                          |
| `~/.local/share/buffr/history.sqlite`      | History DB (Phase 5, `buffr-history`).                          |
| `~/.local/share/buffr/bookmarks.sqlite`    | Bookmarks DB (Phase 5, `buffr-bookmarks`).                      |
| `~/.local/share/buffr/downloads.sqlite`    | Downloads DB (Phase 5, `buffr-downloads`).                      |
| `~/.local/share/buffr/zoom.sqlite`         | Per-site zoom levels (Phase 5, `buffr-zoom`).                   |
| `~/.local/share/buffr/permissions.sqlite`  | Per-origin permission decisions (Phase 5, `buffr-permissions`). |
| `~/.local/share/buffr/usage-counters.json` | Opt-in local telemetry counters (Phase 6; off by default).      |
| `~/.local/share/buffr/crashes/`            | Opt-in panic reports (Phase 6; off by default).                 |

`history.sqlite` runs in WAL mode, so you'll also see `history.sqlite-wal` /
`history.sqlite-shm` next to it during a live session — that's normal. Schema
migrations are forward-only and recorded in a `schema_version` table; see
[`crates/buffr-history/README.md`](../crates/buffr-history/README.md) for the
schema and frecency formula.

macOS uses `~/Library/Application Support/sh.kryptic.buffr/` and
`~/Library/Caches/sh.kryptic.buffr/`; Windows uses
`%APPDATA%\kryptic\buffr\data\` / `%LOCALAPPDATA%\kryptic\buffr\cache\`.

## Config

`buffr-config` reads `~/.config/buffr/config.toml` (or the OS-specific XDG
equivalent). Schema reference: [`docs/config.md`](./config.md). A copy-pasteable
defaults file ships at [`config.example.toml`](../config.example.toml) at the
repo root — drop it into `$XDG_CONFIG_HOME/buffr/config.toml` to start
customising.

```sh
buffr --check-config            # validate ~/.config/buffr/config.toml
buffr --print-config            # dump the resolved (defaults + overrides) TOML
buffr --config /tmp/foo.toml    # use a non-default path
buffr --homepage about:blank    # override general.homepage for one run
```

## Bookmarks

`buffr-bookmarks` ships an SQLite-backed bookmark store with tag support and a
Netscape HTML importer. Schema and Netscape parsing notes live in
[`crates/buffr-bookmarks/README.md`](../crates/buffr-bookmarks/README.md).
There's no UI yet (Phase 5b alongside the omnibar); the CLI flags exist for
import + debugging:

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
through. Schema lives in
[`crates/buffr-zoom/README.md`](../crates/buffr-zoom/README.md).

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
  window in the same process) is Phase 5 tabs work.

## Clear-on-exit

`[privacy] clear_on_exit` (in `config.toml`) lists data categories that buffr
wipes after the event loop returns and before `cef::shutdown()`. Cookies route
through CEF's global `CookieManager::delete_cookies`; cache and local-storage
are directory-tree wipes under `Settings::root_cache_path`; history / bookmarks
/ downloads call `clear_all` on their respective stores. See
[`config.example.toml`](../config.example.toml) for the full list of valid
entries.
