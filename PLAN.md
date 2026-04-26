# buffr — Plan

Living roadmap. Scope, phases, deliverables. Update as work lands.

## Vision

Vim-modal browser. Native shell, GPU-accelerated compositing via CEF. Keyboard
first. No Electron. No web UI for chrome. Snappy, low memory, good battery.

## Non-Goals

- Not a browser engine. Use Chromium via CEF; do not fork Blink.
- Not a re-skin of Chromium. Chrome UI is bespoke and native.
- No extension store v1. WebExtensions support is post-1.0.
- No mobile.

## Target Platforms

| Platform | Tier | Notes                                     |
| -------- | ---- | ----------------------------------------- |
| Linux    | 1    | Wayland + X11. Primary dev target.        |
| macOS    | 1    | Cocoa host. Helper bundle + code signing. |
| Windows  | 2    | Win32/HWND host. MSI installer.           |

## Architecture

```
+-----------------------------+        +------------------------+
|        apps/buffr           |        |   apps/buffr-helper    |
|  main process, UI, modal    |        |   CEF subprocess       |
|  +------------------------+ |  IPC   |  (renderer / gpu /     |
|  | buffr-ui (chrome)      | |<------>|   utility processes)   |
|  | buffr-modal (engine)   | |        +------------------------+
|  | buffr-config (TOML)    | |
|  | buffr-core (CEF host)  | |
|  +------------------------+ |
+-----------------------------+
       |
       | depends on
       v
+----------------------------------------------+
|   crates.io: hjkl-engine, hjkl-buffer,       |
|              hjkl-editor                     |
|   (vim FSM + rope + editor, no_std + alloc)  |
+----------------------------------------------+
```

- **`buffr-core`** — CEF lifecycle, `CefApp`/`CefClient`, browser host,
  off-screen or windowed rendering, navigation, downloads, profile/cache dirs.
- **`buffr-modal`** — **thin layer**: page-mode FSM (normal/visual/command/
  hint) + dispatcher for browser actions (scroll, tab switch, omnibar, hint).
  **Does not implement vim text editing** — delegates to `hjkl-editor` for
  edit-mode (typing in text fields, `<textarea>`, `contenteditable`). Owns
  page-mode keymap parser; edit-mode keymap comes from `hjkl-engine::Keymap`.
- **`buffr-ui`** — native window, tab strip, statusline, command palette, hint
  overlay, omnibar. GPU-composited surface hosting CEF view.
- **`buffr-config`** — TOML loader, schema, hot-reload, XDG/`directories`
  resolution, default config baked in.
- **`apps/buffr`** — main entry. Initializes CEF, loads config, wires modal
  engine to UI, opens initial window.
- **`apps/buffr-helper`** — subprocess entry for CEF child processes (renderer,
  GPU, utility). Must be tiny and fast.

### Edit-mode integration with `hjkl-*`

When the user enters a text field on a page (focus event from CEF) and presses
`i` / `a` / `I` / `A` / etc., buffr enters **edit-mode**:

1. CEF V8 binding reads the field's current value into a mirrored
   `hjkl_buffer::Rope`.
2. `hjkl_editor::Editor<Rope, BuffrHost>` is constructed; receives all
   subsequent keystrokes.
3. Per render frame, `Editor::take_changes()` returns `Vec<Edit>` which buffr
   forwards to CEF as DOM updates (`element.value = ...` or
   `Range.replaceWith`).
4. On `<Esc>` → exit edit-mode → return to page-mode.

`BuffrHost` impls `hjkl_engine::Host`:

- `write_clipboard` → CEF clipboard API (fire-and-forget).
- `read_clipboard` → cached value from CEF clipboard read on focus.
- `Host::Intent` carries buffr-specific events (LSP-equivalents are absent; may
  include `RequestAutocomplete` for form field hints).

Crate features for hjkl: `default-features = false`, no `crossterm` or `ratatui`
(browser context). `std` feature enabled (buffr is non-wasm; full std
available).

### Why hjkl, not in-tree

- Avoids reimplementing vim FSM, motion grammar, registers, undo tree — all
  already designed for sqeel + reusable.
- Multicursor primitive (helix-style) lands for free in form fields.
- Updates flow through one crates.io dependency.
- See `kryptic-sh/hjkl` repo for full spec + stability contract.

## Phases

### Phase 0 — Scaffold ✅

- [x] Workspace `Cargo.toml` with crates + apps.
- [x] Stub `lib.rs` / `main.rs` per crate.
- [x] README, LICENSE, rustfmt.

### Phase 1 — CEF up

Goal: empty native window renders `https://example.com` via CEF.

- [x] CEF binary distribution: download script (`xtask fetch-cef`) per platform,
      pinned to `cef` crate version (147).
- [x] `build.rs` resolves `CEF_PATH`, links `libcef`, copies resources/locales
      next to target.
- [x] `apps/buffr-helper` minimal: forwards argv to `cef::execute_process`,
      exits with returned code.
- [x] `buffr-core::App`: `CefApp` impl, `on_before_command_line_processing`,
      profile/cache dir via `directories`.
- [x] `buffr-core::Host`: create browser, attach to native window handle.
- [x] `apps/buffr` main: init tracing, CEF init, open one tab, run loop.
- [x] Wayland: XWayland default; native Wayland gated behind `--features osr`
      (Phase 3 scope). `apps/buffr` forces the winit X11 backend via
      `EventLoopBuilderExtX11::with_x11()` so Wayland sessions transparently run
      via XWayland.
- [ ] CI: Linux build + smoke test (window opens, page loads, exits clean).
      Build job landed in `.github/workflows/ci.yml`; runtime smoke test still
      needs a display server in CI.
- [x] macOS Helper bundle: `cargo xtask bundle-macos` assembles
      `buffr.app/Contents/Frameworks/buffr Helper.app/` with embedded CEF
      framework. Single helper flavor; multi-helper split (GPU / Renderer /
      Plugin) deferred to Phase 6 alongside signing.

### Phase 2 — Modal engine

Goal: keystrokes routed through modal FSM; page actions work; edit-mode
delegates to `hjkl-editor`.

**Page-mode (buffr-modal, in-tree)**:

- [ ] `PageMode` enum: `Normal | Visual | Command | Hint | Pending`. (Edit-mode
      is a separate state that hands off to hjkl.)
- [ ] Key parser: vim notation → `KeyChord`. Handle `<C-...>`, `<S-...>`,
      `<M-...>`, `<leader>`, `<Space>`, literals.
- [ ] Keymap trie: prefix lookup, ambiguity timeout, count prefix, register
      prefix (`"a`).
- [ ] Page-action dispatch: `PageAction` enum mapped to host calls (scroll, tab
      next/prev/close, back/forward, reload, find, yank URL).
- [ ] Default bindings table (documented in `docs/keymap.md`).
- [ ] Unit tests: parser, trie, ambiguity, count, mode transitions.

**Edit-mode (delegates to hjkl)**:

- [ ] Add `hjkl-engine`, `hjkl-buffer`, `hjkl-editor` to workspace deps, pinned
      `=0.0.x`. No default features.
- [ ] `BuffrHost` struct impls `hjkl_engine::Host`:
      `write_clipboard`/`read_clipboard` via CEF clipboard API,
      `Host::Intent = BuffrEditIntent { RequestAutocomplete, ... }`.
- [ ] CEF V8 binding for focused text-field value get/set; `apps/buffr-helper`
      exposes JS bridge.
- [ ] On focus + `i`/`a`/`I`/`A` etc.: build `Rope` from field value, construct
      `Editor<Rope, BuffrHost>`, route keys to it.
- [ ] Per render frame: drain `Editor::take_changes()` → CEF DOM update via JS
      bridge.
- [ ] `<Esc>` → exit edit-mode → drop `Editor`, return to page-mode.
- [ ] Smoke test: open a page with `<textarea>`, focus it, type `iHello<Esc>` →
      field reads "Hello"; `dd` → field cleared.

### Phase 3 — UI chrome

Goal: tab strip + statusline + command line + omnibar, all native.

- [x] Window backend choice: `winit` for window/event loop; chrome painted into
      a `softbuffer = "0.4"` strip docked below the CEF child window. Decision
      recorded in [`docs/ui-stack.md`](./docs/ui-stack.md). `wgpu`/OSR
      compositor reserved for the hint-mode migration.
- [ ] Implement `crates/buffr-core/src/osr.rs` (currently scaffolded). Wire
      `OsrHost::new` to real CEF windowless mode + wgpu compositor so Wayland
      sessions can run natively without XWayland.
- [x] Tab strip: render via `buffr-ui::TabStrip` (softbuffer paint), wired to
      multi-tab `BrowserHost`. Active tab highlighted with accent stripe; pinned
      tabs marked with `*`; loading progress drawn at the bottom edge of each
      pill. Drag-reorder + middle-click-close are post-Phase-3.
- [x] Statusline: mode indicator, URL, progress, cert state, count buffer.
      Bundled 6x10 bitmap font in `buffr-ui::font`; widget renders into a
      `softbuffer::Surface`. Phase 3b adds load-progress / cert hookup.
- [x] Command line (`:`): input strip, completion against a static command list,
      dispatcher in `buffr-core::cmdline`. Supports `:q` / `:quit`, `:reload`,
      `:back`, `:forward`, `:open <url>`, `:tabnew` (logs only),
      `:set zoom in|out|reset`, `:bookmark <tags...>`, `:find <query>`,
      `:devtools`. Find query routing now uses the live
      `BrowserHost::start_find`.
- [x] Omnibar (`o`/`O`): search-or-URL resolver in `buffr-config::search`,
      suggestion source = history + bookmarks + search-engine fallback.
      `BrowserHost::navigate` performs the load on Enter.
- [x] Hint mode (`f`/`F`): DOM-injected `<div class="buffr-hint-overlay">`
      labels via `frame.execute_java_script`. CEF→Rust IPC uses the console-log
      scraping fallback (`__buffr_hint__:` sentinel) since the
      `cef_process_message_t` path needs a renderer-side `RenderProcessHandler`
      we don't otherwise need. `F` (background-tab) currently logs a warning and
      falls back to a same-tab click — multi-tab is post-Phase-3. See
      [`docs/hint-mode.md`](./docs/hint-mode.md).
- [x] Find-in-page (`/`, `?`, `n`, `N`): wired to CEF `find` API via
      `BrowserHost::start_find` / `stop_find` and a `BuffrFindHandler` that
      pumps `OnFindResult` into the statusline. UI for entering the query
      requires the command bar (Phase 3b); a `--find <query>` smoke flag on
      `apps/buffr` exercises the wiring without UI.

### Phase 4 — Config

Goal: user TOML config drives keymap, theme, startup, search engines.

- [ ] Schema: `[keymap]`, `[theme]`, `[startup]`, `[search]`, `[privacy]`.
- [ ] Loader: XDG (`$XDG_CONFIG_HOME/buffr/config.toml`), macOS app support dir,
      Windows `%APPDATA%\buffr\config.toml`. Resolved via `directories`.
- [ ] Validation: friendly errors with line/col via `toml` spans.
- [ ] Hot reload: file watcher → re-parse → diff → apply.
- [ ] `buffr --print-config` and `buffr --check-config`.

### Phase 5 — Browser features (1.0 cut)

- [x] Tabs: create/close/move/pin/duplicate, restore last session. Multi-tab
      `BrowserHost` manages a `Vec<Tab>` with monotonic `TabId`s; default keymap
      binds `gt` / `gT` / `<C-w>c` / `t` / `<C-w>n` (duplicate) / `<C-w>p`
      (pin). `:tabnew` opens an extra tab; `:q` closes the active tab and only
      quits when the last tab is gone. Session restored from
      `~/.local/share/buffr/session.json` (`{ url, pinned }` per entry);
      `--no-restore` and `--list-session` CLI flags drive testing. See
      [`docs/multi-tab.md`](./docs/multi-tab.md).
- [x] History: SQLite store, dedupe, frecency search for omnibar. Pure data
      layer in `crates/buffr-history`; wired live via the CEF `LoadHandler` in
      `buffr-core::handlers`. Phase 5b/c follow-ups:
  - [ ] FTS5 migration (replace `LIKE %q%` with `MATCH` + `bm25`).
  - [ ] Detect `Reload` transitions via `LoadHandler::on_load_start`
        (`transition_type` flag) — currently every visit is recorded as `Link`.
  - [ ] Expose `buffr query history --limit N --search foo` CLI when chrome /
        omnibar lands so the data is reachable without UI.
  - [ ] Surface `SKIP_SCHEMES` as a `[privacy]` config knob.
- [x] Bookmarks: tagged, SQLite-backed, CLI import (Netscape HTML). Pure data
      layer in `crates/buffr-bookmarks`; CLI flags `--import-bookmarks`,
      `--list-bookmarks`, `--list-bookmarks-tags` exposed on `apps/buffr`. UI
      wiring is Phase 5b alongside the omnibar.
- [x] Downloads: progress, open-on-finish, default dir from config. Pure data
      layer in `crates/buffr-downloads`; CEF `DownloadHandler` (in
      `buffr-core::handlers`) routes `OnBeforeDownload` / `OnDownloadUpdated`
      into the store. CLI flags `--list-downloads` and
      `--clear-completed-downloads` exposed on `apps/buffr`.
  - [ ] `ask_each_time` UI is Phase 3 chrome work; for now downloads silently
        land in `default_dir`.
- [x] Cookies/site storage: per-profile, clear-on-exit option. Wired via
      `[privacy] clear_on_exit` listing `cookies` / `cache` / `history` /
      `bookmarks` / `downloads` / `local_storage`. Cookies route through
      `cef::cookie_manager_get_global_manager().delete_cookies(None, None, None)`;
      cache + local-storage are directory-tree wipes under `root_cache_path`;
      history/bookmarks/downloads call `clear_all` on their respective stores.
- [x] Permissions prompt UI: camera, mic, geolocation, notifications. CEF
      `PermissionHandler` (`on_request_media_access_permission` +
      `on_show_permission_prompt`) routes uncached requests onto a shared
      `Mutex<VecDeque<PendingPermission>>`. The UI thread drains one per tick
      and renders a 60 px [`PermissionsPrompt`] strip; `a`/`d` resolve once,
      `A`/`D`/`s` persist into `crates/buffr-permissions` (SQLite,
      `<data>/permissions.sqlite`). Decision precedence: stored Allow > stored
      Deny > prompt. CLI flags `--list-permissions`, `--clear-permissions`,
      `--forget-origin`. See
      [`crates/buffr-permissions/README.md`](./crates/buffr-permissions/README.md).
- [x] Private window: ephemeral profile. `--private` CLI flag opens every store
      in-memory and roots `Settings::root_cache_path` at a `tempfile::TempDir`
      deleted on shutdown. Single-window incognito; multi-window-per-profile is
      post-Phase-5 tabs work.
- [x] Zoom per-site, persisted. `crates/buffr-zoom` SQLite store keyed by
      `host[:port]` (or `_global_` for hostless URLs). Apply on
      `LoadHandler::on_load_end`; persist on `ZoomIn` / `ZoomOut` / `ZoomReset`
      page actions. CLI: `--list-zoom`, `--clear-zoom`.
- [ ] DevTools toggle (`<C-S-i>`).

### Phase 6 — Polish & ship

- [x] Crash reporter (opt-in). Local-only panic-hook reporter writing
      `<data>/crashes/<timestamp>.json`. CEF native-crash capture via
      crashpad/breakpad is deferred — `BrowserProcessHandler` does not expose
      `on_uncaught_exception` in libcef-147; configuring crashpad requires
      shipping a `crashpad_handler` binary + symbol server. See
      [`docs/privacy.md`](./docs/privacy.md).
- [x] Update channel: GitHub releases API check + on-disk cache + statusline
      indicator. **No automatic binary replacement** — that needs signing
      infrastructure deferred to post-1.0. CLI flags `--check-for-updates`
      (live) and `--update-status` (cached). Disabled-able via
      `[updates] enabled = false` (zero network calls). See
      [`docs/updates.md`](./docs/updates.md).
- [x] Packaging:
  - [x] Linux: AppImage, `.deb`, AUR PKGBUILD.
        `cargo xtask package-linux     [--variant {appimage,deb,aur,all}]`
        produces all three under `target/dist/linux/`. Unsigned this round —
        release-pipeline signing is the next step. See
        [`docs/packaging.md`](./docs/packaging.md).
  - [x] macOS: `.app` + `.dmg`, **unsigned**. Bundle assembly via
        `cargo xtask bundle-macos` (Phase 1) now ships the four-helper layout
        (`Helper`, `Helper (GPU)`, `Helper (Renderer)`, `Helper (Plugin)`) so
        future signing only needs per-flavor entitlements + a path-resolver
        hook. `cargo xtask package-macos-dmg` wraps the bundle into
        `target/dist/macos/buffr-<ver>-<arch>.dmg` via `hdiutil` (macOS) or
        `genisoimage` (Linux fallback). Developer-ID signing + notarization are
        post-Phase-6 release-pipeline work — see
        [`docs/macos-signing.md`](./docs/macos-signing.md).
  - [x] Windows: MSI, **unsigned**. `cargo xtask package-windows-msi` renders a
        hand-rolled WiX 3 template (`xtask/templates/buffr.wxs`) and drives
        `candle.exe` + `light.exe` to produce
        `target/dist/windows/buffr-<ver>-x64.msi`. Authenticode signing is
        post-Phase-6 release-pipeline work — see
        [`docs/windows-packaging.md`](./docs/windows-packaging.md).
- [x] Telemetry: none by default; opt-in anonymous usage counters. Local-only
      JSON file at `<data>/usage-counters.json`. No network endpoint exists —
      buffr never phones home, even on opt-in. See
      [`docs/privacy.md`](./docs/privacy.md).
- [x] Accessibility pass: CEF renderer accessibility tree (off by default via
      `[accessibility] force_renderer_accessibility`); high-contrast theme
      palette (`[theme] high_contrast`); `--audit-keymap` CLI for keyboard-only
      verification; documented gap (native chrome AT bindings deferred post-1.0)
      in [`docs/accessibility.md`](./docs/accessibility.md).
- [x] Docs site: mdBook scaffold (`book.toml` + `docs/SUMMARY.md`).
      `.github/workflows/docs.yml` builds + uploads to GitHub Pages on push to
      main. DNS for `docs.buffr.kryptic.sh` is a TODO; until then the github.io
      preview URL is the canonical surface.

### Post-1.0

- WebExtensions subset (content scripts, browser_action, storage).
- Tree-style tabs / workspaces.
- Sync (encrypted, self-hostable).
- Reader mode.
- Container tabs (Firefox-style).
- Per-site script blocking, ad/tracker lists.

## Cross-Cutting Concerns

### Build & toolchain

- Pinned Rust via `rust-toolchain.toml`, tracking stable (matches `hjkl` MSRV
  policy).
- `cargo xtask` for: fetch-cef, package, sign, run-helper.
- Workspace lints: `clippy::pedantic` opt-in per crate, deny `unwrap` in release
  paths.
- Platform support matches `hjkl`: linux glibc 2.28+, macOS 12+ (universal),
  Windows 10+. CEF availability is the harder constraint — only ship buffr where
  CEF binaries exist.

### Dependencies on hjkl-\*

- `hjkl-engine = "=0.0.x"`, `hjkl-buffer = "=0.0.x"`, `hjkl-editor = "=0.0.x"`,
  all `default-features = false`, `features = ["std"]` (no `crossterm`, no
  `ratatui`).
- Lockstep-pinned with exact `=` until hjkl reaches 0.1.0.
- Local-dev override for working across both repos:
  ```toml
  [patch.crates-io]
  hjkl-engine = { path = "../hjkl/crates/hjkl-engine" }
  hjkl-buffer = { path = "../hjkl/crates/hjkl-buffer" }
  hjkl-editor = { path = "../hjkl/crates/hjkl-editor" }
  ```

### Testing

- Unit: per crate, especially `buffr-modal` parser/trie.
- Integration: spawn `buffr` headless, drive via IPC test harness, assert page
  load + key dispatch.
- Snapshot: chrome rendering via `insta` + offscreen capture.
- CI matrix: Linux (Wayland + X11), macOS, Windows. Per-PR Linux only; nightly
  full matrix.

### Security

- Process sandbox: rely on CEF's sandbox; verify enabled per-platform.
- Site isolation: default on.
- No remote debugging port unless `--debug-port` passed.
- Config files never executed; TOML only.
- Auto-update signature verification before apply.

### Performance budgets

- Cold start to first paint: < 400 ms on M1 / modern Linux laptop.
- Idle RAM (1 tab, about:blank): < 250 MB.
- Keystroke → action dispatch: < 4 ms p99 in modal engine.
- 60 fps tab switch on integrated GPU.

## Risk Register

| Risk                                         | Mitigation                                                                                     |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `cef` crate API churn at 147                 | Pin minor; vendor patches if needed.                                                           |
| CEF binary size + distribution friction      | `xtask fetch-cef`; cache in CI; mirror tarballs.                                               |
| Native chrome over CEF surface compositing   | Prototype early in Phase 3; fall back to OSR if hard.                                          |
| macOS code signing / notarization complexity | Set up Apple cert in CI before Phase 6.                                                        |
| Wayland vs X11 input handling differences    | Test both in CI from Phase 1.                                                                  |
| Modal engine ambiguity / timeout UX          | Vim-parity defaults; configurable via TOML.                                                    |
| `hjkl-*` 0.0.x churn breaks edit-mode        | Pin `=0.0.x`; lockstep update PR per hjkl release.                                             |
| DOM ↔ rope sync drift (concurrent edits)     | Pull model: `Editor::take_changes()` per frame; JS-side edits remap via `apply_external_edit`. |

## Milestones

| Tag      | Target       | Definition of done                               |
| -------- | ------------ | ------------------------------------------------ |
| `v0.1.0` | Phase 1 done | Linux: window opens, loads URL, exits clean.     |
| `v0.2.0` | Phase 2 done | Modal engine drives navigation; default keymap.  |
| `v0.3.0` | Phase 3 done | Native chrome: tabs, statusline, command, hints. |
| `v0.4.0` | Phase 4 done | TOML config, hot reload, custom keymaps.         |
| `v0.9.0` | Phase 5 done | Feature-complete for daily driving.              |
| `v1.0.0` | Phase 6 done | Signed packages on all tier-1 platforms.         |

## Open Questions

- Window/compositor: `winit + wgpu` overlay vs full OSR-into-wgpu?
- History/bookmarks store: SQLite vs sled vs flat TOML?
- Hint mode DOM access: CEF V8 extension vs DevTools Protocol?
- Search providers: hardcode list vs config-only?
- Theme system: tokens (`fg`, `bg`, `accent`) vs full CSS-like?

Resolve each before its phase starts; record decision in `docs/adr/`.
