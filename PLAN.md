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
```

- **`buffr-core`** — CEF lifecycle, `CefApp`/`CefClient`, browser host,
  off-screen or windowed rendering, navigation, downloads, profile/cache dirs.
- **`buffr-modal`** — mode FSM (normal/insert/visual/command/hint), keymap
  parser (vim-notation: `<C-w>v`, `gT`, `<leader>...`), pending-key buffer,
  count prefixes, motion grammar.
- **`buffr-ui`** — native window, tab strip, statusline, command palette, hint
  overlay, omnibar. GPU-composited surface hosting CEF view.
- **`buffr-config`** — TOML loader, schema, hot-reload, XDG/`directories`
  resolution, default config baked in.
- **`apps/buffr`** — main entry. Initializes CEF, loads config, wires modal
  engine to UI, opens initial window.
- **`apps/buffr-helper`** — subprocess entry for CEF child processes (renderer,
  GPU, utility). Must be tiny and fast.

## Phases

### Phase 0 — Scaffold ✅

- [x] Workspace `Cargo.toml` with crates + apps.
- [x] Stub `lib.rs` / `main.rs` per crate.
- [x] README, LICENSE, rustfmt.

### Phase 1 — CEF up

Goal: empty native window renders `https://example.com` via CEF.

- [ ] CEF binary distribution: download script (`xtask fetch-cef`) per platform,
      pinned to `cef` crate version (147).
- [ ] `build.rs` resolves `CEF_PATH`, links `libcef`, copies resources/locales
      next to target.
- [ ] `apps/buffr-helper` minimal: forwards argv to `cef::execute_process`,
      exits with returned code.
- [ ] `buffr-core::App`: `CefApp` impl, `on_before_command_line_processing`,
      profile/cache dir via `directories`.
- [ ] `buffr-core::Host`: create browser, attach to native window handle.
- [ ] `apps/buffr` main: init tracing, CEF init, open one tab, run loop.
- [ ] CI: Linux build + smoke test (window opens, page loads, exits clean).

### Phase 2 — Modal engine

Goal: keystrokes routed through modal FSM; basic motions work.

- [ ] `Mode` enum: `Normal | Insert | Visual | Command | Hint | Pending`.
- [ ] Key parser: vim notation → `KeyChord`. Handle `<C-...>`, `<S-...>`,
      `<M-...>`, `<leader>`, `<Space>`, literals.
- [ ] Keymap trie: prefix lookup, ambiguity timeout, count prefix, register
      prefix (`"a`).
- [ ] Action dispatch: `Action` enum mapped to host calls (scroll, tab
      next/prev/close, back/forward, reload, find, yank URL).
- [ ] Default bindings table (documented in `docs/keymap.md`).
- [ ] Unit tests: parser, trie, ambiguity, count, mode transitions.

### Phase 3 — UI chrome

Goal: tab strip + statusline + command line + omnibar, all native.

- [ ] Window backend choice: `winit` for window/event loop; `wgpu` or
      platform-native compositor for chrome layer above CEF surface. Decision
      doc in `docs/ui-stack.md`.
- [ ] Tab strip: render, drag-reorder, close-on-middle-click, overflow.
- [ ] Statusline: mode indicator, URL, progress, cert state, count buffer.
- [ ] Command line (`:`): input, history, completion, async results.
- [ ] Omnibar (`o`/`O`): search-or-URL, suggestions from history.
- [ ] Hint mode (`f`/`F`): DOM query via CEF V8 binding → overlay labels →
      keystroke filter → click/focus dispatch.
- [ ] Find-in-page (`/`, `?`, `n`, `N`): wire to CEF find API.

### Phase 4 — Config

Goal: user TOML config drives keymap, theme, startup, search engines.

- [ ] Schema: `[keymap]`, `[theme]`, `[startup]`, `[search]`, `[privacy]`.
- [ ] Loader: XDG (`$XDG_CONFIG_HOME/buffr/config.toml`), macOS app support dir,
      Windows `%APPDATA%\buffr\config.toml`. Resolved via `directories`.
- [ ] Validation: friendly errors with line/col via `toml` spans.
- [ ] Hot reload: file watcher → re-parse → diff → apply.
- [ ] `buffr --print-config` and `buffr --check-config`.

### Phase 5 — Browser features (1.0 cut)

- [ ] Tabs: create/close/move/pin/duplicate, restore last session.
- [ ] History: SQLite store, dedupe, fuzzy search for omnibar.
- [ ] Bookmarks: tagged, TOML or SQLite, CLI import (Netscape HTML).
- [ ] Downloads: progress, open-on-finish, default dir from config.
- [ ] Cookies/site storage: per-profile, clear-on-exit option.
- [ ] Permissions prompt UI: camera, mic, geolocation, notifications.
- [ ] Private window: ephemeral profile.
- [ ] Zoom per-site, persisted.
- [ ] DevTools toggle (`<C-S-i>`).

### Phase 6 — Polish & ship

- [ ] Crash reporter (opt-in).
- [ ] Update channel: stable + nightly tags. Tauri-style updater or OS package
      managers.
- [ ] Packaging:
  - [ ] Linux: AppImage, `.deb`, AUR PKGBUILD.
  - [ ] macOS: signed/notarized `.app` + `.dmg`. Helper bundle inside.
  - [ ] Windows: signed MSI.
- [ ] Telemetry: none by default; opt-in anonymous usage counters.
- [ ] Accessibility pass: screen reader labels on chrome, focus order.
- [ ] Docs site: install, keymap reference, config reference, recipes.

### Post-1.0

- WebExtensions subset (content scripts, browser_action, storage).
- Tree-style tabs / workspaces.
- Sync (encrypted, self-hostable).
- Reader mode.
- Container tabs (Firefox-style).
- Per-site script blocking, ad/tracker lists.

## Cross-Cutting Concerns

### Build & toolchain

- Pinned Rust via `rust-toolchain.toml` (TBD).
- `cargo xtask` for: fetch-cef, package, sign, run-helper.
- Workspace lints: `clippy::pedantic` opt-in per crate, deny `unwrap` in release
  paths.

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

| Risk                                         | Mitigation                                            |
| -------------------------------------------- | ----------------------------------------------------- |
| `cef` crate API churn at 147                 | Pin minor; vendor patches if needed.                  |
| CEF binary size + distribution friction      | `xtask fetch-cef`; cache in CI; mirror tarballs.      |
| Native chrome over CEF surface compositing   | Prototype early in Phase 3; fall back to OSR if hard. |
| macOS code signing / notarization complexity | Set up Apple cert in CI before Phase 6.               |
| Wayland vs X11 input handling differences    | Test both in CI from Phase 1.                         |
| Modal engine ambiguity / timeout UX          | Vim-parity defaults; configurable via TOML.           |

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
