# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.1] - 2026-05-25

### Fixed

- Mouse wheel vertical scroll direction. The buggy commit `6967500` worked
  around inverted scroll by negating dy in the wayr → CEF helper; the root cause
  was wayr passing Wayland's "positive = scroll down" sign through unchanged.
  Wayr 0.2.1 now normalises to winit's "positive = scroll up" convention at the
  emission site, so the buffr-side negation is reverted and the helper just
  passes the signed delta straight to CEF.

### Changed

- Bumped wayr dep `0.2.0 → 0.2.1` for the scroll-sign fix above.

[0.14.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.1

## [0.14.0] - 2026-05-25

### Added

- Cross-platform support: buffr-app now builds and runs on macOS and Windows in
  addition to Linux. Wayr stays Linux-only (Wayland-only by design); macOS and
  Windows use winit 0.30 behind a `crate::windowing::*` re-export that mirrors
  wayr's API shape. A new `bridge` feature on `buffr-modal` introduces a
  toolkit-agnostic `KeyEvent` / `KeyCode` / `Modifiers` / `ScanCode` shape
  consumed on non-Linux; Linux keeps using `wayr_key_event_to_chord`.
- Hidden `--smoke-test [--smoke-test-timeout-ms <N>]` flag on `buffr-app`.
  Launches the windowing backend, paints once, exits 0 on first paint, exits 3
  if a watchdog timer fires. Wired into CI as a 3-OS event-loop smoke matrix.
- `BUFFR_DISABLE_ZYGOTE=1` env switch appends `--no-zygote` to the CEF command
  line on Linux. Off by default; intended for headless / containerized
  environments where Chromium's zygote subprocess fork can't pass the ICU file
  descriptor.
- Authoritative compositor occlusion via wayr 0.1.9 `Occluded` events: paint
  pacing now stops when the compositor reports the surface fully occluded
  instead of inferring from `present_us` heuristics.
- Damage tracking via wayr 0.1.11: chrome paints attach damage rectangles so the
  compositor can skip recomposite for unchanged regions.
- Focus existing window on `--new-tab` via `xdg_activation`: a second buffr
  invocation now raises the existing window before opening the tab instead of
  spawning a duplicate toplevel.
- Per-platform smoke jobs (Linux, macOS, Windows) cover the full event-loop
  startup path, including CEF init and first paint. Ubuntu smoke is marked
  `continue-on-error` while CEF's subprocess ICU fd-passing remains broken under
  pixman-headless sway on GHA runners (unrelated to the migration — affects real
  Linux desktops only in headless contexts).

### Changed

- Migrated buffr-app from winit to wayr on Linux. Wayr owns wl_display dispatch
  so wayr globals can be bound against the same display without the EAGAIN race
  winit's calloop integration causes.
- Workspace resolver bumped to `"3"` (matches edition 2024).
- Renderer decoupled from `winit::Window`: takes a `Surface` trait so wayr
  toplevels and winit windows go through the same path.
- All wgpu mutating calls run on a worker thread (`render_worker`); UI thread
  only does CPU prep + memcpy + `try_send`. Eliminates Wayland backpressure
  hangs when the compositor queues frames.
- CI smoke timeout bumped 30s → 120s for cold-runner CEF init; smoke success
  signal moved from `RedrawRequested` to first `paint_chrome` completion so
  headless Windows runners (no DWM → no WM_PAINT) still satisfy it.

### Fixed

- buffr-cef: collapse nested `if let` blocks for macOS clippy.
- buffr-app: keep `modifiers` in sync from `KeyEvent.modifiers` on every key
  event, not only on a dedicated `ModifiersChanged` event (wayr's model).
- buffr-app: ASCII-control text now routed through the named-key path so Ctrl-A
  / Ctrl-B etc. produce the right chord instead of garbled bytes.
- buffr-app: leak the renderer on shutdown instead of dropping (wayr 0.1.4
  wgpu-surface path is safe; the leak avoids a tear-down race that fired Vulkan
  validation warnings on exit).
- supervisor: respect an explicit clean-shutdown intent via env-var flag, and
  don't respawn when the child exits with a non-zero code if it was a clean exit
  (e.g., `--smoke-test`).
- Build a child process suspended on Windows via `&Path` not `&PathBuf` (clippy
  `ptr_arg`).
- Windows headless smoke: `request_redraw()` after window creation forces a
  paint cycle when the runner's WM never delivers WM_PAINT.

### Removed

- `[patch.crates-io]` overrides for wayr — published wayr versions now drive
  buffr's wayr surface directly.

[0.14.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.0

## [0.13.11] - 2026-05-19

### Fixed

- Windows packaging job failed in v0.13.10 because the GitHub Actions PowerShell
  wrapper propagates `$LASTEXITCODE` from the last external command — even when
  the script checks it manually and chooses to continue. `buffr-helper.exe`
  exits -1 by design (no `--type=` → cef returns -1), and that bled out as the
  step's overall exit code. Reset `$LASTEXITCODE = 0` after the manual check.
  macOS and Linux smoke (bash) are unaffected because the trailing `echo` resets
  `$?` to 0.

[0.13.11]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.11

## [0.13.10] - 2026-05-19

### Added

- CI smoke now also exercises `buffr-helper` on all three platforms (Linux
  tarball, macOS DMG, Windows zip). Helper has no clap CLI; bare invocation
  loads `libcef` then calls `execute_subprocess`, which returns `-1` for the
  no-`--type=` path (Rust exit 255 on unix, -1 on Windows). Catches framework
  rpath rot on macOS (`Buffr Helper.app/Contents/MacOS/Buffr Helper`), libcef
  RPATH on Linux, and DLL search-path regressions on Windows.

[0.13.10]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.10

## [0.13.9] - 2026-05-19

### Added

- Per-platform smoke tests in CI: every release job now extracts the produced
  artifact (Linux tarball, macOS DMG, Windows zip) and invokes both `buffr` and
  `buffr-app` with `--version` and `--help` before uploading. Clap exits before
  CEF init so no display server is required; catches PE/ELF/Mach-O link
  failures, missing `buffr-app.exe` suffix on Windows, libcef rpath rot on
  Linux, and dyld framework misses on macOS.

[0.13.9]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.9

## [0.13.8] - 2026-05-19

### Fixed

- Supervisor watchdog killed the browser before it could ever connect on
  cold-disk first runs (scoop / MSI install on a fresh Windows machine). Two
  stacking issues:
  - buffr-app called `heartbeat::Heartbeat::try_connect` ~600 lines into `main`,
    after `cef::load_library`, `Cli::parse`, tracing init, and SQLite db opens.
    Hoisted to immediately after the CEF subprocess short-circuit so the
    named-pipe / UDS connect happens before any heavy work.
  - Supervisor `CONNECT_GRACE` was 5 s on both Unix and Windows. Raised to 20 s
    — fits the worst observed cold-cache first run. The 20 s default is
    overridable via `BUFFR_CONNECT_GRACE_MS` so integration tests stay bounded
    (the `heartbeat_no_connect` integration test would otherwise wedge nextest's
    60 s per-test timeout).
- Supervisor watchdog error message pointed users at a non-existent
  `~/.local/share/buffr/crashes/` (or `%APPDATA%\buffr\crashes\`) path — the
  supervisor never wrote anything there. Replaced with a concrete command for
  capturing buffr-app's stderr directly. A proper supervisor-side child-stderr
  redirect remains a TODO.

[0.13.8]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.8

## [0.13.6] - 2026-05-19

### Fixed

- Windows supervisor (`buffr.exe`) couldn't locate `buffr-app.exe` either as a
  sibling or on `PATH` because the resolver joined the bare string `"buffr-app"`
  and `is_file()` is exact-name on Windows. Now joins `buffr-app.exe` when
  `cfg!(windows)`. Scoop / MSI installs launch correctly.

### Changed

- Scoop manifest now exposes only `buffr.exe` as a `bin` / shortcut.
  `buffr-app.exe` and `buffr-helper.exe` were also shimmed at v0.13.5, which let
  users double-click `buffr-app.exe` directly and land on a white screen (the
  supervisor is the only supported entry point — it owns the crash-restart +
  heartbeat watchdog).

[0.13.6]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.6

## [0.13.5] - 2026-05-19

### Added

- Scoop manifest pipeline. Windows packaging now produces a `.zip` payload
  alongside the existing `.msi` (same staged `buffr.exe` + `buffr-app.exe` +
  `buffr-helper.exe` + CEF runtime tree). A new `scoop-bucket` CI job renders
  `pkg/scoop/buffr.json.in` against the freshly-published zip sha256 sidecars
  and pushes the manifest to `kryptic-sh/scoop-bucket` so
  `scoop install kryptic/buffr` resolves. Requires `SCOOP_SSH_KEY` secret
  granted to the repo.

[0.13.5]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.5

## [0.13.4] - 2026-05-19

### Changed

- `no suitable wgpu adapter` error now names the actual fix: install a software
  Vulkan driver. Lists the package names for Arch (`vulkan-swrast`,
  `vulkan-intel`, etc.), Debian/Ubuntu (`mesa-vulkan-drivers`), and the
  `vulkaninfo --summary` verification command. Saves a round-trip to the issue
  tracker for users on un-configured Vulkan stacks.

[0.13.4]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.4

## [0.13.3] - 2026-05-19

### Fixed

- wgpu adapter selection now falls back through a ladder (HighPerformance →
  LowPower → software/llvmpipe) instead of bailing with
  `no suitable wgpu adapter` on the first miss. Machines with broken Vulkan +
  broken DRI2 + no usable GL (older hardware, drivers in transition) now boot
  via the software path rather than refusing to start. Logs the selected
  `AdapterInfo` so triage shows backend + driver at a glance.

[0.13.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.3

## [0.13.2] - 2026-05-19

### Fixed

- Linux packaging now ships every CEF runtime `.so` next to `libcef.so`:
  `libEGL.so`, `libGLESv2.so` (ANGLE), `libvk_swiftshader.so`, `libvulkan.so.1`
  (SwiftShader Vulkan fallback), plus `vk_swiftshader_icd.json`. Previously only
  `libcef.so` was packaged — on hardware without a system GLES library at the
  expected path, CEF emitted
  `Failed to load GLES library: /opt/buffr/libGLESv2.so` and the GPU process
  crashed before wgpu init. App then exited cleanly with no usable renderer.
  Affected: any machine where the CEF binary distribution's bundled
  ANGLE/SwiftShader libs weren't present in the install root. Fix lands in
  `xtask::collect_runtime_payload` + `stage_payload` so `.deb`, `.rpm`,
  `.tar.gz`, AUR all carry them.

[0.13.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.2

## [0.13.1] - 2026-05-19

### Fixed

- CEF cookie persistence on Linux. Login state + cookies + cache now survive
  browser restart. Two stacked fixes:
  - `--password-store=basic` propagated to every CEF subprocess via the new
    `on_before_child_process_launch` hook on `BrowserProcessHandler` (CEF does
    not auto-propagate user switches; each subprocess argv is rebuilt at spawn).
    Required when a Secret Service backend (pass-secret-service, KeePassXC,
    etc.) is detected over D-Bus but its unlock/auth dance fails — Chromium
    policy is to drop cookies rather than persist unencrypted.
  - Per-engine `CefRequestContext` creation dropped in `BrowserHost::new`. CEF
    Alloy runtime collapses any child `cache_path` to `Default/` anyway, and
    creating the per-engine context was silently blocking cookie persistence on
    top of that. Per-engine isolation tracked separately in #158 — needs Chrome
    runtime swap to be real.
- `session.json` URL fix and engine profile-dir relocation now have a fully
  working storage path under them.

### Changed

- `BackendOpenOptions.data_dir` is currently unused by the CEF backend (was
  wired into the deleted per-engine `CefRequestContext`). Will be re-wired when
  per-engine isolation lands.

[0.13.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.1

## [0.13.0] - 2026-05-18

### Removed

- X11 support on Linux. buffr now requires a Wayland session
  (`XDG_SESSION_TYPE=wayland`) and refuses to start otherwise. winit's X11
  backend is no longer compiled in. The X11 idle-inhibit backend
  (`buffr-core::inhibit::linux::x11`, 242 LoC) is gone.
- Browser-engine backends: `buffr-servo`, `buffr-ladybird`, `buffr-blitz`,
  `buffr-webkit-cocoa`, `buffr-webview2`. CEF is now the only user-facing engine
  on every platform. `buffr-webkit` (WPE WebKit) is retained on disk as an
  experimental Linux-only backend but is workspace-excluded — build standalone
  with `cargo build --manifest-path crates/buffr-webkit/Cargo.toml`.
- `--engine` CLI flag now accepts only `cef`.
- `apps/buffr-stub` (the `buffr` crates.io name-holder). All 13 published
  versions on crates.io have been yanked. Binary distribution moves entirely to
  GitHub release artifacts.
- CEF custom `buffr://` scheme registration. The scheme handler factory,
  `register_buffr_scheme`, `register_buffr_handler_factory`, and
  `BuffrSchemeHandlerFactory` are deleted.
- WebKit `data:text/html;base64,…` URL fallback for `buffr://*`.
- `buffr-engine::newtab::translate_internal_url` and `default_newtab_html`
  (data: URL helpers).
- `BrowserEngine::image_rotate` trait method (no use case).
- xvfb-based smoke test in CI (incompatible with Wayland-only Linux).

### Added

- `BrowserEngine::is_using_native_compositing` trait method to surface the
  runtime native-vs-OSR state.
- IME composition support in `buffr-webkit` via `WebKitInputMethodContext`.
- `BackendOpenOptions::internal_server` so backends receive the loopback HTTP
  server at construction. Avoids a race where the initial-tab navigation fires
  before the server can be wired post-hoc.

### Changed

- Repository structure: 12 ex-submodule crates (`buffr-bookmarks`, `buffr-cef`,
  `buffr-config`, `buffr-core`, `buffr-downloads`, `buffr-engine`,
  `buffr-history`, `buffr-modal`, `buffr-permissions`, `buffr-ui`,
  `buffr-view-source`, `buffr-zoom`) absorbed into the monorepo via
  `git subtree add` (history preserved). `.gitmodules` deleted.
  `[patch.crates-io]` dropped. Per-crate scaffolding (CHANGELOG, README,
  LICENSE, dependabot, ci.yml, deny.toml, rust-toolchain.toml, rustfmt.toml,
  .gitignore, .editorconfig) stripped — workspace-level files cover all members.
  `.editorconfig` hoisted to workspace root.
- CEF backend routes `buffr://*` through `InternalServer` (HTTP loopback)
  instead of a custom URI scheme. Mirrors the webkit pattern. CEF
  `on_register_custom_schemes` no longer touches `buffr://`; `buffr-src://`
  (view-source) keeps its custom handler.
- WebKit's `resolve_url` requires an `InternalServer` to translate `buffr://*`.
  Bind failure is now fatal at startup (was a silent data-URL fallback).
- CEF `tabs_summary` reports the display URL (`buffr://new`) instead of the
  engine-loaded `http://127.0.0.1:<port>/<token>/new`, so `session.json` saves a
  stable URL across restarts (the ephemeral loopback port no longer outlives the
  process).
- All workspace members marked `publish.workspace = true` → `publish = false`.
  No member is published to crates.io independently.
- All Wayland render path moved to an async worker thread (canonical Wayland
  backpressure fix): UI thread does CPU + memcpy + try_send, worker owns all
  wgpu mutating calls.
- Supervisor heartbeat decoupled from UI event loop (bg thread + atomic liveness
  counter). Survives sustained input and Wayland event-loop stalls.
- Loading animation now gates on the live pixel pipeline state, not the engine's
  `host_is_loading` flag (which could pin true forever).
- `--engine cef` skips engine selection state (single binary stays CEF-only).
- WebKit `prefer_native` (Wayland subsurface compositing) is now opt-in via
  `BUFFR_WEBKIT_NATIVE=1`; default is the OSR pixel-copy path. Native path
  remains experimental (chrome overlay layout + watchdog hang issues
  unresolved).
- Demoted `buffr::ui_path` per-frame traces from `info!` to `debug!`.
- crates.io: yanked all 14 buffr-\* component crates (58 versions total). Names
  remain reserved.
- GitHub: 12 submodule repos deleted under `kryptic-sh/buffr-*`.

### Fixed

- Wayland-only Linux: `wayland_globals` registry roundtrip gated behind
  native-compositing opt-in (avoided clobbering main-loop event routing on
  Wayland sessions).
- CEF OSR composite alpha: prefer Opaque over PreMultiplied to fix
  semi-transparent viewport on the OSR path.
- WebKit cookie DB path doubling (`engines/<id>/profile/engines/<id>/` →
  `engines/<id>/profile/`).
- WebKit `JSON.stringify` wrapping on `postMessage` UCM bridge
  (`[object Object]` payloads silently dropped before).
- WebKit RawDown handling: printable keys no longer emit duplicate events
  (typing produced `"Hh"`, `"Ee"`).
- WebKit `FocusFirstInput` + `ExitInsertMode` dispatch arms.
- WebKit resize-paint watchdog: `force_repaint_active()` reaches the worker so
  paint can recover after stuck-frame timeouts.
- Same-tab navigation no longer leaves a stale OSR frame: applies
  resize-wiggle + `needs_fresh` like cross-tab open does.
- wgpu OOM at surface acquire under rapid resize: poll the device every frame so
  lazy resource drops actually run (32-bit AMDGPU address-space pressure).
- Ctrl+C now works reliably on Wayland-stuck event loops via a three-layer fix
  (NewEvents check, UserEvent exit, 3s libc::\_exit abort).
- Splash gate no longer reads `host_is_loading` (could pin true forever).
- Heartbeat keeps alive under sustained input.

[0.13.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.0

## [0.12.0] - 2026-05-15

### Security

- **XSS chain closed at three layers.** `javascript:` and `data:` URIs are
  rejected by the omnibar resolver (treated as search queries), the IPC
  single-instance accept thread (dropped before dispatch), and the navigation
  router (`NavigationVerdict::DisallowedScheme`). Previously a crafted paste,
  malicious second-instance invocation, or attacker-controlled bookmark could
  reach `engine.navigate()` and execute arbitrary script.
- **view-source SSRF closed.** `spawn_view_source_fetch` validates the URL
  scheme is `http` or `https` before issuing the ureq fetch, blocking
  `file:///etc/passwd` and `http://169.254.169.254/` exfiltration paths.
- **IPC payload caps.** Forward payloads on the local socket are limited to 100
  URLs of 1024 bytes each before scheme validation runs.

### Fixed

- `WindowEvent::MouseWheel` no longer panics on `active_engine_dyn().unwrap()`
  during startup or after the last tab closes.
- `BlinkCdpEngine::close_all_browsers` uses blocking `send` for `Shutdown` so a
  full channel never silently leaks the worker thread + child process.
- blink-cdp page title now sourced from `Target.targetInfoChanged` (the previous
  `frame.name` read was almost always empty, blanking the tab strip after each
  navigation).
- blink-cdp `Mutex<EngineState>` lock failures recover from poison via a
  `lock_state()` helper instead of cascading panics to the UI thread.
- blink-cdp WebSocket `try_recv_text` returns `Ok(None)` for non-`Plain` (TLS)
  streams instead of blocking the worker forever.
- blink-cdp permission shim injects a `beforeunload` listener that resolves
  pending Promises with `denied` on navigation, eliminating per-navigation
  Promise leaks in SPAs.
- blink-cdp pending CDP commands carry an `Instant`; entries older than 30 s are
  swept and error-replied each iteration to bound memory under hung Chromium.
- CEF `on_dismiss_permission_prompt` removes the dismissed entry from the
  callback registry and neutral queue (slow-leak fix).
- CEF `sanitise_filename` rejects Windows reserved device stems (`CON`, `PRN`,
  `AUX`, `NUL`, `COM[1-9]`, `LPT[1-9]`) on every platform.

### Changed

- `BrowserEngine::can_go_back()` and `can_go_forward()` trait defaults changed
  from `true` to `false`. Both engines override these methods, so concrete
  behavior is unchanged.
- `EngineEvent`, `LoadState`, `CursorKind`, `MediaType` annotated with
  `#[non_exhaustive]` to allow additive variants without breaking downstream
  exhaustive matches.

### Removed

- `tracing::debug!` calls from CEF `view_rect` and `get_screen_info` (per-paint
  hot path, called at 60+ Hz).

### Dependencies

- `buffr-config` 0.4.1 → 0.4.2
- `buffr-engine` 0.1.2 → 0.1.3
- `buffr-blink-cdp` 0.1.4 → 0.1.5
- `buffr-cef` 0.1.1 → 0.1.2

## [0.11.1] - 2026-05-15

### Added

- `tracing::debug!` log when CEF backend silently drops `cache_dir` option —
  diagnostic for multi-engine on-disk layout comparison (buffr-cef 0.1.1).
- `BlinkError::CacheDirCreate` distinguishes pre-spawn `mkdir` failures from
  actual `Command::spawn` failures (buffr-blink-cdp 0.1.4).
- New unit test `migration_skips_when_new_path_already_exists` covers the
  both-old-and-new-exist case in `engine_migrate`, asserting the production
  guard preserves new-layout content and leaves old in place.

### Dependencies

- `buffr-cef` 0.1.0 → 0.1.1
- `buffr-blink-cdp` 0.1.3 → 0.1.4

## [0.11.0] - 2026-05-15

### Fixed

- CEF engine no longer spills its Chromium profile flat into `~/.cache/buffr/`
  (`Default/`, `ShaderCache/`, `chrome_debug.log`, etc. at the root). Each CEF
  engine instance now gets its own namespaced directory under
  `~/.cache/buffr/engines/<engine-id>/`. Phase 11a (#96).
- blink-cdp profile directory moved from `~/.local/share/buffr/blink-cdp/<id>/`
  to `~/.local/share/buffr/engines/<id>/profile/`. Existing profiles are
  migrated automatically on first launch via `fs::rename`. Phase 11a (#96).

### Added

- Startup migration shim (`engine_migrate`) moves blink-cdp profiles from the
  pre-11a layout to the new `engines/<id>/profile/` path. Warns on stale CEF
  flat state without auto-deleting. Migration is skipped in `--private` mode.
- `BackendOpenOptions.cache_dir: Option<&Path>` — ephemeral cache directory for
  backends that support a persistent/ephemeral split. Phase 11b (#96).
- blink-cdp now passes `--disk-cache-dir=<cache_root>/engines/<id>/` to headless
  Chromium. HTTP cache, shader cache, and code cache land in
  `~/.cache/buffr/engines/<id>/` while cookies, IndexedDB, and prefs remain in
  `~/.local/share/buffr/engines/<id>/profile/`. Phase 11b (#96).

### Dependencies

- `buffr-engine` 0.1.1 → 0.1.2
- `buffr-blink-cdp` 0.1.2 → 0.1.3

## [0.10.0] - 2026-05-15

### Fixed

- **blink-cdp audit Phase 10 — 23 bugs fixed** (P0-1 through P2-8). All
  blink-cdp backend behaviors that should match CEF now do. Highlights:
  - Address bar updates after in-page navigation (was frozen)
  - History back/forward, reload, hard reload, stop, and all scroll keybinds
    work (were silent no-ops)
  - Permissions/find/context-menu shims fire on the initial page load (were
    registered too late)
  - Download notices carry the right filename and path
  - `view-source:` no longer freezes the UI thread
  - Permission prompts show readable origin for `data:` URLs
  - `getUserMedia` requesting both audio + video now prompts for both
  - Tab strip loading indicator reflects real loading state
  - `open_tab_at` honors the insert index
  - Graceful engine shutdown via Drop impl (subprocess cleanup)

### Changed

- `BackendOpenOptions.find_sink` field added (buffr-engine 0.1.1).
- `BlinkCdpEngine::open_tab_internal` uses `about:blank` then explicit
  `Page.navigate` so all shims are registered before page content loads.
- Live tab title via `Target.targetInfoChanged` subscription.

### Dependencies

- `buffr-engine` 0.1.0 → 0.1.1
- `buffr-blink-cdp` 0.1.1 → 0.1.2

## [0.9.1] - 2026-05-15

### Changed

- **Promote `buffr-engine`, `buffr-cef`, `buffr-blink-cdp` to standalone repos +
  crates.io** (#72). All three new crates extracted to their own GitHub repos
  (`kryptic-sh/buffr-engine`, `kryptic-sh/buffr-cef`,
  `kryptic-sh/buffr-blink-cdp`) with full sibling boilerplate (README,
  CHANGELOG, LICENSE, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, deny.toml,
  rust-toolchain.toml, rustfmt.toml, .editorconfig, tag-driven CI). Published to
  crates.io as `buffr-engine@0.1.0`, `buffr-cef@0.1.0`, `buffr-blink-cdp@0.1.1`.
  Umbrella now references all three as submodules under `crates/`, mirroring the
  buffr-core / buffr-modal / buffr-config / buffr-ui pattern.
  `[patch.crates-io]` table unchanged — keeps in-tree resolution for umbrella
  builds.

  Cascade BCTPs landed before promotion to unblock standalone CI:
  - `buffr-modal` 0.1.4 → 0.1.5 — `PageAction::Engine(String)` variant
  - `buffr-config` 0.4.0 → 0.4.1 — engines table + instances + per-domain rules
  - `buffr-ui` 0.2.1 → 0.2.2 — tab-strip engine badge + hover outline
  - `buffr-core` 0.6.3 → 0.7.0 — **minor bump**, dropped all CEF integration
    code (~30 public types removed: `BrowserHost`, `Tab`, `OsrFrame`,
    `BuffrApp`, `AudioEvent`, `PopupQueue`, `PermissionsQueue`, etc.).
    buffr-core is now backend-agnostic.

  `buffr-core`'s standalone CI now resolves cleanly (`buffr-engine = "0.1"`
  available on crates.io). Apps layer unchanged — workspace builds via
  `[patch.crates-io]` paths.

## [0.9.0] - 2026-05-15

### Added

- **Phase 8: blink-cdp feature parity sprint** — seven sub-phases bring the
  blink-cdp backend up to the same baseline UX as CEF for everyday browsing.
  After this release, blink-cdp is viable as a default engine for typical
  workloads, not just experimental opt-in.
  - **8a — permissions** (#88). Geolocation, Notifications, Microphone, and
    Camera prompts now route through the same status-line prompt UI as CEF.
    Implemented via a JS shim injected with
    `Page.addScriptToEvaluateOnNewDocument` that wraps
    `navigator.geolocation.getCurrentPosition`,
    `Notification.requestPermission`, `navigator.permissions.query`, and
    `navigator.mediaDevices.getUserMedia`. The shim posts to a
    `Runtime.addBinding` named `__buffrPermissionRequest`; the CDP worker thread
    receives `Runtime.bindingCalled` events and pushes a neutral
    `PendingPermission` onto the shared `PermissionsQueue`. User answers resolve
    back to the page via `Runtime.evaluate` calling
    `__buffrPermissionResolve(<id>, granted|denied)`. The permission types
    (`PendingPermission`, `PermissionsQueue`, `PromptOutcome`, `Capability`)
    moved to `buffr-engine::permissions`; the queue is now a `BrowserEngine`
    trait method `permissions_queue()` so apps drain from both backends
    uniformly.

  - **8b — downloads** (#84).
    `Browser.setDownloadBehavior { behavior: "allow", downloadPath, eventsEnabled: true }`
    is configured per engine at startup. The worker subscribes to
    `Browser.downloadWillBegin` and `Browser.downloadProgress` events, mapping
    `inProgress`/`completed`/ `canceled` states onto the existing
    `buffr-downloads` SQLite store via
    `Downloads::record_started`/`update_progress`/`record_completed`/
    `record_canceled`. Completed downloads push to `DownloadNoticeQueue` for
    status-line surface (same as CEF). Per-engine download directory defaults to
    `<data_root>/blink-cdp/<id>/downloads` so each instance gets isolated
    storage.

  - **8c — find-in-page** (#83). `/`-keymap now works on blink-cdp tabs via an
    injected TreeWalker-based shim that wraps text-node matches in
    `<span class="__buffr-find-match">` with a current-match accent on
    `__buffr-find-current`. `start_find` / `find_next` / `find_prev` /
    `stop_find` on the `BrowserEngine` trait call
    `Runtime.evaluate("__buffrFindNext('query', false)")` etc.; the JS returns
    `{ current, total }` which the worker writes to the existing
    `FindResultSink`. No `eval`, no `innerHTML` — pure DOM mutation.

  - **8d — context-menu hit-test** (#87). Right-click on a blink-cdp tab now
    populates the same context menu as CEF (Copy Link, Save Image, Copy Image
    URL, Open Image In New Tab, etc.). Implemented via a capture-phase
    `contextmenu` event listener injected on every page; the listener walks the
    target ancestor chain for `<a href>`, `<img>`, `<video>`, `<audio>`,
    editable nodes, plus `window.getSelection()`, then posts the result to the
    `__buffrContextMenu` Runtime binding. The worker parses the JSON payload
    into a neutral `ContextMenuRequest` matching CEF's shape so the apps-layer
    menu builder is reused unchanged.

  - **8e — IME composition** (#86). Three new `BrowserEngine` trait methods
    (`ime_set_composition`, `ime_commit`, `ime_cancel`) route winit
    `WindowEvent::Ime(Preedit|Commit|Enabled|Disabled)` events to CDP
    `Input.imeSetComposition { text, selectionStart, selectionEnd }` and
    `Input.insertText { text }`. International input (Japanese, Chinese, Korean
    composition windows; dead-key combining accents) now works on blink-cdp
    tabs.

  - **8f — `buffr://` and `view-source:` schemes** (#81). Chromium's network
    stack rejects unknown schemes before CDP `Fetch` can intercept them, so the
    engine translates internal URLs at the navigation layer: `buffr://new` and
    `buffr://settings` route to `data:text/html;base64,<page_html>`;
    `view-source:<url>` sync-fetches the target via `ureq`, HTML-escapes it into
    a dark-themed `<pre>` envelope, and navigates to a data: URL.
    `EngineState.original_urls` stashes the human-readable URL per-tab so
    `active_tab_live_url` / `tabs_summary` return `buffr://new` instead of the
    opaque base64 blob. The `NewTabHtmlProvider` is now shared between CEF and
    blink-cdp through `BlinkCdpBackend::register_new_tab_handler`.

  - **8g — Picture-in-Picture** (#90, resolves #31). Picture-in-Picture now
    works on blink-cdp tabs via `Runtime.evaluate` invoking
    `HTMLVideoElement.requestPictureInPicture()`. The IIFE finds the most
    relevant video (preferring currently-playing, then unmuted, then first) and
    toggles between enter/exit. This closes #31 (the original CEF limitation)
    for users on the blink-cdp engine — switch a domain via `:engine blink-cdp`
    or a per-domain engine rule to get PiP on sites where CEF can't deliver it.

  Test count: 887/887. Tab-strip badge `BL` (blink-cdp) tabs now have full
  CEF-equivalent UX across permissions, downloads, find, context menu, IME,
  internal schemes, and PiP.

## [0.8.1] - 2026-05-15

### Fixed

- **CEF: per-engine on-disk cache isolation via `RequestContext`** (#79).
  `BrowserHost::new_with_options` now takes `data_dir: Option<&Path>`. When
  `Some`, builds a `cef::RequestContextSettings` with `cache_path = data_dir`,
  calls `cef::request_context_create_context`, and passes the resulting
  `RequestContext` to `browser_host_create_browser_sync` (the 6th arg slot that
  was previously `None`). The host holds the context in a `Mutex` so the
  `&self`-method `create_browser` can lock-borrow `as_mut()`. When `None`, CEF
  falls back to its global default cache. The apps-layer "advisory only in Phase
  3" warning is deleted — `data_dir` is now real. Two CEF engine instances
  configured with distinct `data_dir` no longer share cookies, cache,
  local-storage, or IndexedDB.

- **blink-cdp: default `--user-data-dir` lives under XDG data, not `/tmp`**
  (#89). The fallback when an `[[engines.instances]]` block has no explicit
  `data_dir` was `/tmp/buffr/blink-cdp/<instance-id>`, which was cleared on
  reboot and did not respect `--private` mode. It now resolves to
  `<AppState.data_root>/blink-cdp/<instance-id>`. `data_root` is captured at
  startup from `resolve_paths(cli.private)`, so in normal mode it's the XDG data
  dir and in `--private` mode it's the per-pid `TempDir` that gets dropped at
  exit. Private-mode blink-cdp profiles are now torn down with the rest of the
  temp tree; persistent-mode profiles survive across reboot.

## [0.8.0] - 2026-05-15

### Changed

- **Engine refactor Phase 6 — apps layer fully agnostic** (#95). The
  `cef_engines: HashMap<EngineId, Arc<BrowserHost>>` parallel map at the apps
  layer is deleted. All ~113 reach-through call sites now route through
  `Arc<dyn BrowserEngine>` via `self.engines`. Seven sub-phases shipped:
  - **6a** — `BrowserEngine` trait widened with 14 `popup_*` methods (sinks,
    resize, close, drain_address/title_changes, history_back/forward, OSR
    input). `PopupQueue`, `PopupCreateSink`, `PopupCloseSink`,
    `PendingPopupAlloc`, `PopupCreated` and their helpers move from `buffr-cef`
    to `buffr-engine`. `BlinkCdpEngine` gets no-op stubs (deferred to a future
    CDP popup pass).
  - **6b** — trait widened with 6 `hint_*` methods (`cancel_hint`,
    `backspace_hint`, `feed_hint_key`, `hint_status`, `is_hint_mode`,
    `pump_hint_events`). `HintStatus` moves to `buffr-engine::hint`.
    `HintAction` (formerly in `buffr-core`) re-exports through `buffr-engine`.
  - **6c** — trait widened with 22 frame/edit/media/JS/devtools/downloads
    methods (`frame_copy/cut/paste/paste_plain/redo/select_all/undo`,
    `media_play_pause/picture_in_picture/toggle_controls/toggle_loop/toggle_mute`,
    `image_rotate`, `run_js`, `run_main_frame_js`,
    `run_edit_attach/cycle/detach/focus`, `run_media_probe`, `start_download`,
    `show_dev_tools_at`). Default impls used heavily — void methods log a
    `debug!`, `Result` returners default to `Err(EngineError::Unimplemented)`.
    `BlinkCdpEngine` overrides `run_js` and `run_main_frame_js` via CDP
    `Runtime.evaluate` and `show_dev_tools_at` via the existing CDP inspector
    URL.
  - **6d** — trait widened with 8 clipboard/audio/cursor/favicon/context-menu
    methods. `ClipboardReader` becomes `Arc<dyn ClipboardRead>` (trait moved to
    `buffr-engine::clipboard`); `FaviconUpdate` is a new neutral type in
    `buffr-engine::favicon`. Audio drain fan-out switches from
    `cef_engines.values()` to `engines.values()`.
  - **6e** — `ProfilePaths` moves from `buffr-cef` to `buffr-engine::profile`.
    New-tab HTML constants (`NEW_TAB_HTML_TEMPLATE`, `NEW_TAB_KEYBINDS_MARKER`,
    `NEW_TAB_SPLASH_ART_MARKER`, `NEW_TAB_URL`, `SETTINGS_URL`) move to
    `buffr-engine::newtab`. Apps imports `buffr_engine::TabId` directly instead
    of routing through `buffr-cef`.
  - **6f** — `cef_engines` map and `active_host()` method deleted. 38 call sites
    migrated to `active_engine_dyn()`. New trait methods filled the last gaps
    (`dispatch`, `is_loading`, `can_go_back`, `can_go_forward`,
    `drain_context_menu_requests`).
  - **6g** — `Backend` trait introduced in `buffr-engine::backend`. `CefBackend`
    (owns `BuffrApp`, drives `cef::initialize`, message pump, scheme handlers,
    cookie store, device-scale, accessibility hints) and `BlinkCdpBackend`
    (no-op lifecycle, `open_engine` constructs `BlinkCdpEngine`) provide the two
    backends. Apps holds `Arc<dyn Backend>`; all 10 CEF lifecycle call sites
    (`cef_initialize`/`cef_shutdown`/`do_message_loop_work`/`load_cef_library`/
    `execute_process_for_subprocess`/`take_scheduled_message_pump_delay_ms`/
    `delete_all_cookies`/`set_force_renderer_accessibility`/
    `set_device_scale_factor`) route through the trait.

  Final state: `apps/buffr-app/src/main.rs` references only `CefBackend`,
  `CefEngineSinks`, and the CEF-specific permissions queue types from
  `buffr-cef`. All other engine interaction goes through `buffr-engine` trait
  surfaces. Test count: 820/820.

## [0.7.1] - 2026-05-14

### Fixed

- **macOS package build broken in v0.7.0**: `cef::Settings` binding in
  `buffr_cef::cef_initialize` was `let` not `let mut`, but the
  `#[cfg(target_os = "macos")]` block at `crates/buffr-cef/src/lib.rs:166`
  assigns to `external_message_pump`, `browser_subprocess_path`,
  `framework_dir_path`, and `resources_dir_path`. Linux cfg-gated the block out
  so the issue was invisible locally; macOS package CI caught it.

## [0.7.0] - 2026-05-14

### Added

- **blink-cdp: replace `captureScreenshot` poll with `Page.startScreencast`
  streaming** (#91). The 5 FPS `Page.captureScreenshot` poll loop is removed.
  The worker now sends `Page.startScreencast` (PNG, `everyNthFrame=1`, at
  viewport dimensions) when a session becomes active, and `Page.stopScreencast`
  when it is deactivated or closed. Chromium pushes frames at its native render
  cadence; backpressure is provided by the mandatory `Page.screencastFrameAck`
  reply which the worker sends immediately after decoding each frame. On resize,
  the worker stops and restarts the screencast with the new
  `maxWidth`/`maxHeight` so Chromium re-renders at the correct dimensions. Tab
  switches send stop on the old session and start on the new one. Interactive
  pages now feel as responsive as CEF tabs; idle tabs consume no screenshot
  bandwidth.

- **blink-cdp: `:devtools` opens Chromium inspector in system browser** (#82).
  `BlinkCdpEngine` now implements `open_devtools` on the `BrowserEngine` trait.
  When `:devtools` is invoked with the blink-cdp engine active, buffr builds a
  `http://127.0.0.1:<port>/devtools/inspector.html?ws=…/devtools/page/<target_id>`
  URL and hands it to the OS via the `open` crate so the user's default browser
  hosts the Chromium DevTools inspector. The debug port is tracked on
  `EngineState.debug_port` (set once at `BlinkCdpEngine::new` from the ephemeral
  port chosen by `pick_free_port`). `BrowserEngine::open_devtools` is a new
  trait method with a default no-op body so backends that don't support it
  return `Ok(())` without changes. `buffr-cef` overrides it to call the existing
  `show_dev_tools_at(None, None)` inherent method. The `dispatch_action` arm in
  `apps/buffr-app` routes `PageAction::OpenDevTools` through the active engine
  trait surface (same pattern as the zoom arms added in #85) so both CEF and
  blink-cdp receive the call correctly.

- **tab strip: engine-badge 2-char glyph + hover outline + status-line tooltip**
  (#94). The flat 4-px coloured band on non-default-engine tab pills is replaced
  by a wider badge column that renders a 2-character uppercase label derived
  from the engine id (e.g. `"BL"` for `blink-cdp`, `"WK"` for `webkit`, `"??"`
  for empty ids). Badge width is `font::text_width("WW") + 2 × BADGE_SIDE_PAD`
  so it adapts to the active font automatically. When the cursor hovers over a
  badged tab, a 1-px white outline is drawn around the badge rectangle and the
  engine id is written to the statusline as `"engine: <id>"`. The single-engine
  (CEF-only) path is unchanged — no badges, no tooltip. New
  `EngineRouter::badge_label_for` method and `badge_label_text` helper (6 new
  unit tests). `TabView` gains `engine_label: Option<String>` and
  `hovered: bool` fields; `Statusline` gains `engine_hint: Option<String>`
  rendered on the right-hand cell.

- **blink-cdp: zoom in/out/reset via `Runtime.evaluate` + per-tab tracking**
  (#85). `BlinkCdpEngine` now implements `zoom_in`, `zoom_out`, and `zoom_reset`
  (new default-no-op methods on `BrowserEngine`). Zoom is tracked per tab as a
  linear CSS factor (`1.0` = 100 %, step `0.25`, clamped to `[0.25, 5.0]`) and
  injected via `document.body.style.zoom` through `Runtime.evaluate`. The worker
  thread stores per-session zoom levels and re-applies them on every
  `Page.frameNavigated` event so navigation does not silently reset the scale.
  `active_zoom_level()` now returns the tracked factor (was always `0.0`). The
  step size (0.25) matches the CEF backend's `adjust_zoom` delta.

### Changed

- **blink-cdp: free-port probe replaces hardcoded 9222** (#92).
  `BlinkCdpEngine::new` now calls `pick_free_port()` — a
  `TcpListener::bind("127.0.0.1:0")` probe — instead of the fixed port 9222.
  Multiple engine instances can coexist without port conflicts, and starting
  buffr when 9222 is already in use by another process no longer prevents
  initialisation. New error variant `BlinkError::PortProbe(std::io::Error)` is
  surfaced when the OS cannot allocate an ephemeral port.

### Added

- **Engine refactor — Phase 5 (initial cut): `:engine` command, tab strip engine
  badge, `buffr://settings` scaffold** (#76, parent #54). Three user-visible
  pieces shipped in this pass:
  - **`:engine <id>` command** — rebinds the active tab to a different rendering
    engine (same URL, new engine). Implemented as `Command::Engine(String)` in
    `buffr-core/src/cmdline.rs` (parsed from the `:` command bar), dispatched to
    `PageAction::Engine(String)` in `buffr-modal/src/actions.rs`, and handled in
    `AppState::dispatch_action` in `apps/buffr-app/src/main.rs`. Pattern:
    snapshot URL → `open_tab` on target → `close_active` on source →
    `active_engine` swap. Unknown ids emit a `warn!` and are silently ignored
    (no crash). `"engine"` added to `COMMAND_NAMES` for omnibar completion.
  - **Tab strip engine badge** — a 4-px coloured band at the left edge of each
    unpinned tab pill, visible only when >1 engine is registered. Colour is a
    deterministic DJB2 hash of the engine id string mapped onto an 8-colour
    palette (red / orange / yellow / green / blue / violet / pink / grey). The
    primary `"cef"` engine id is exempt (no badge). Implementation:
    `show_badges()` + `badge_color_for()` methods on `EngineRouter`;
    `engine_badge: Option<u32>` field on `TabView`; badge painted in
    `TabStrip::paint` before the favicon/title. `refresh_tab_strip` populates
    the badge from the router on every tick.
  - **`buffr://settings` scaffold** — new route in the
    `BuffrSchemeHandlerFactory` (`crates/buffr-cef/src/new_tab.rs`). Requests to
    `buffr://settings` are served a minimal HTML page listing registered engines
    and active routing rules. `SettingsHtmlProvider` closure type +
    `settings_html(engines, rules)` helper added so callers can inject live
    router state without a restart.
    `register_buffr_handler_factory_with_settings` is the new preferred entry
    point; the original `register_buffr_handler_factory` retains its signature
    and falls back to a static placeholder. `SETTINGS_URL` constant exported
    from `buffr-cef`.
  - Sub-issues for unimplemented feature-parity work filed against #76 (see
    commit log for issue numbers).
  - Six new unit tests: four in `engine_router` for badge logic, two in
    `buffr-core/cmdline` for the `:engine` parser. 725/725 tests pass.

- **Engine refactor — Phase 4: blink-cdp second backend spike** (#75, parent
  #54). New `buffr-blink-cdp` workspace crate (`crates/buffr-blink-cdp/`)
  implements `BrowserEngine` via headless Chromium driven over Chrome DevTools
  Protocol. No Chromium binary bundled — system `chromium`, `google-chrome`, or
  `chromium-browser` required at runtime. Implemented in Phase 4: tab
  create/close (`Target.createTarget` + `Target.attachToTarget`), navigate
  (`Page.navigate`), OSR paint via `Page.captureScreenshot` polled at ~5 FPS
  (BGRA decode → `SharedOsrFrame`), mouse events (`Input.dispatchMouseEvent`),
  keyboard events (`Input.dispatchKeyEvent`), viewport resize
  (`Page.setDeviceMetricsOverride`), and tab bookkeeping. Stubbed with
  `EngineError::Unimplemented`: `duplicate_active`, `reopen_closed_tab`, and all
  popup/hint/find/zoom/devtools/scheme-handler/audio/video methods.
  `AppState::engines` changed from `HashMap<EngineId, Arc<BrowserHost>>` to
  `HashMap<EngineId, Arc<dyn BrowserEngine>>` (Phase 4 dyn-dispatch map); a
  parallel `cef_engines: HashMap<EngineId, Arc<BrowserHost>>` provides
  CEF-specific reach-through (popup*\*, hint*\*, drain_audio_events, etc.).
  `EngineError::Unimplemented { method: &'static str }` variant added to
  `buffr-engine`. Demo routing config at `examples/blink-cdp-demo-config.toml`.
  Architecture: one `std::thread` per engine instance owns the `tungstenite`
  WebSocket; trait methods send `Command` over an `mpsc::SyncSender` and wait on
  a response channel (sync→async bridge, no tokio).

- **Engine refactor — Phase 3: multi-engine runtime** (#74, parent #54).
  Multiple `BrowserHost` instances now live simultaneously inside one CEF
  process; each is registered under a unique `EngineId` in a
  `HashMap<EngineId, Arc<BrowserHost>>` in `AppState`. New
  `[[engines.instances]]` config table (`id`, `backend`, optional `data_dir`;
  advisory-only in Phase 3 — CEF cache is process-global; Phase 5+ isolation via
  `RequestContext`). When `instances` is empty a single
  `{ id: "cef", backend: "cef" }` instance is synthesised so single-engine
  configs need no changes. Config validation: unique ids, non-empty
  `id`/`backend`, `default` and rule `engine` fields must reference declared
  instances. Engine registry loop in `resumed()` replaces single-host
  construction; each instance gets its own OSR wake callback, frame rate, and
  device scale. Paint multiplexer: `active_host()` reads from
  `engines[active_engine]`; resize/scale fan-out to all engines; audio/video
  activity ORed across all engines. Cross-engine navigation:
  `classify_navigation()` pure helper + `check_cross_engine_nav()` called after
  each `pump_address_changes()` — opens a new tab on the target engine and
  closes the source tab. Tab-count exit checks fan across all engines.
  `active_engine: EngineId` field tracks focused engine; updated on cross-engine
  nav. `buffr-cef/README.md` added documenting `cef::initialize` singleton,
  global cache-path constraint, and multi-instance model. Four
  `classify_navigation` unit tests; eleven `engines.instances` config tests.
  715/715 tests pass. CEF global cache isolation (Phase 5+) and per-engine
  helper subprocess args tracked as follow-ups to #74.

- **Engine refactor — Phase 2: engine router + per-domain rules config** (#73,
  parent #54). New `EngineId` newtype in `buffr-engine` (serde transparent,
  `Display`, `Hash`, `Eq`). New `[engines]` table in `buffr-config`: `default`
  engine id + `[[engines.rules]]` with host-glob `match` and `engine` fields;
  validated at load time (non-empty fields required; registry check deferred to
  router). New `engine_router` module in `apps/buffr-app`: `EngineRouter`
  (backed by `globset` host-glob matching, case-insensitive, `url` host
  extraction; falls back to default for host-less URLs — `about:blank`, `data:`,
  `file://`); `EngineRouterBuilder` (register / default / rule / build);
  `RouterError` (`UnknownEngine`, `InvalidGlob`, `EmptyRegistry`). Router built
  after `BrowserHost` construction in `resumed()`; host wrapped in
  `Arc<dyn BrowserEngine>` and stored alongside `Arc<BrowserHost>` for the tab-
  spawn routing path. Three tab-spawn actions (`TabNew`, `TabNewRight/Left`,
  session + CLI tab restore) route through `routed_open_tab*` helpers; remaining
  sites unchanged (single-backend, behaviour identical). Shutdown:
  `engine_router` dropped before CEF shutdown to preserve `Arc<BrowserHost>`
  drop ordering. Nine `EngineRouter` + builder tests; three `buffr-config`
  engines-section tests. 704/704 tests pass. Groundwork for multi-engine runtime
  (Phase 3, #74).

### Changed

- **Engine refactor — Phase 1: `buffr-engine` trait + `buffr-cef` backend
  extraction** (#72, parent #54). All CEF integration moved out of `buffr-core`
  into a new `buffr-cef` backend crate; a new `buffr-engine` trait crate defines
  the engine-agnostic surface (`trait BrowserEngine`, neutral `TabId` /
  `NeutralKeyEvent` / `MouseButton` / `OsrFrame` / `OsrViewState` types).
  `buffr-cef::BrowserHost` now implements `buffr_engine::BrowserEngine` and
  exposes only neutral types at its public boundary — no `cef::*` leaks through
  `buffr-core`, `apps/buffr-app`, or `apps/buffr-helper`. CEF library load and
  settings construction encapsulated inside `buffr-cef`; helper binary routes
  through `buffr_cef::execute_subprocess()`. No user-visible behaviour change;
  686/686 tests pass. Groundwork for multi-engine routing (Phase 2, #73) and
  alternate backends (Phase 4, #75).

## [0.6.3] - 2026-05-13

### Added

- **Right-click context menu on tab-strip entries** (#37). Right-clicking a tab
  now opens a tab-specific menu — Reload Tab, Duplicate Tab, Pin/Unpin Tab, Copy
  Tab URL, Close Tab, Close Other Tabs, Close Tabs to the Right — instead of the
  page/selection menu. "Close Other Tabs" / "Close Tabs to the Right" are dimmed
  when there's nothing they'd close and skip pinned tabs (Chrome behaviour);
  closing a pinned tab routes through the existing confirmation prompt;
  Duplicate Tab inserts the copy right after the source. Keymap (`x`, `gt`/`gT`,
  pin toggle) stays canonical — the menu mirrors it for discoverability.

### Submodules

- `buffr-core` `0.6.2` → `0.6.3` (adds `ContextMenuTarget`, the tab
  `ContextMenuItem` variants, and `build_tab_context_menu_model`).

## [0.6.2] - 2026-05-13

### Added

- **Crash-loop detection** (#61). `apps/buffr-app/src/crash_guard.rs` tracks
  recent startup timestamps in `<data_dir>/launch.json`. Three startups inside a
  60-second window without a clean exit between them is treated as a crash loop:
  the saved `session.json` is moved aside to `session.json.crashed-<unix_ts>`
  and the new launch starts from the homepage instead of restoring the killer
  URL set. Graceful shutdown paths (last-tab-close, `:q`, window close, Ctrl+C)
  clear the tracker. Skipped under `--private`.

### CI

- Tag-gated the 5 heavy packaging jobs (`linux-package`, `macos-package`,
  `windows-package`, `flatpak`, `snap`) — main-push CI now runs only
  lint+test+smoke+deny+macos-cross (~3min) instead of the full ~17min matrix.
  Tag pushes still get the full pipeline.
- Added `~/.local/share/flatpak` + `flatpak/.flatpak-builder` caches to the
  Flatpak job (GNOME runtime is ~2-3GB per arch; primes at next tag).
- Fanned `bundle-macos-cross` out from behind `needs: [linux]` — runs in
  parallel with the linux build instead of waiting for it.
- Split `fmt` + `clippy` out of the `linux` job into a parallel `lint` job.
- Swapped `cargo test` for `cargo nextest run` (workspace tests run ~3× faster
  locally; doctests covered by a separate `cargo test --doc` pass).

## [0.6.1] - 2026-05-12

### Added

- **Favicon disk cache** (#71). Decoded favicon bitmaps now persist to
  `<data_dir>/favicons.sqlite` keyed by origin (`scheme://host[:port]`).
  Restored tabs paint their cached favicon on the first tick — before CEF's
  asynchronous `download_image` callback fires — and so do new tabs to any
  previously-seen origin (omnibar, hint, popup, middle-click, view-source). A
  per-tick runtime scan compares each tab's current URL against the one last
  cache-checked for that `browser_id`, enqueueing prefills on change without
  per-call-site wiring. CEF-delivered bitmaps unconditionally overwrite the
  cached entry for the tab's current origin (no staleness). Skipped under
  `--private` and when `[general] show_favicons = false`.

### Submodules

- `buffr-core` bumped `0.6.1` → `0.6.2` (adds `FaviconCache`, `CachedFavicon`,
  `origin_of`).

## [0.6.0] - 2026-05-12

### Changed

- **wgpu bumped 22.1.0 → 29.0.3** (#25). API churn across 7 major versions
  required `render.rs` rewrites: `ImageCopyTexture` → `TexelCopyTextureInfo`,
  `ImageDataLayout` → `TexelCopyBufferLayout`, new required fields on
  `RenderPassDescriptor` (`multiview_mask`), `RenderPassColorAttachment`
  (`depth_slice`), and `DeviceDescriptor` (`experimental_features`, `trace`).
  `adapter.request_device` lost its trace-path arg (folded into
  `DeviceDescriptor::trace`). `InstanceDescriptor` lost `Default` —
  `InstanceDescriptor::new_without_display_handle()` instead.
  `surface.get_current_texture()` now returns a `CurrentSurfaceTexture` enum
  with explicit `Occluded` / `Suboptimal` / `Outdated` / `Lost` / `Validation`
  variants — `OutOfMemory` is folded into `Lost`. All wgpu mutating calls remain
  on the async render worker thread.
- **hjkl-bonsai bumped 0.3.0 → 0.6.1** (#69) — picks up predicate/directive
  dispatcher (helix + nvim-treesitter parity over stock tree-sitter) and the new
  `XDG` resolver via `hjkl-xdg`. Consumer code passes `&ManifestMeta` to
  `GrammarLoader::user_default` and `Grammar::load`.
- **sha2 0.10.9 → 0.11.0** (#26).
- **signal-hook 0.3.18 → 0.4.4** (#68).
- **hjkl-config 0.2.0 → 0.2.1; nix 0.31.2 → 0.31.3** (#67 patch group).
- **`actions/download-artifact` 4 → 8** in CI (#66).
- **Workflow names switched to PascalCase.**

### Submodules

- **`buffr-view-source` extracted to its own repo + crates.io publication**
  (`buffr-view-source = "0.1"`). Previously an in-tree workspace member; now
  lives at `crates/buffr-view-source/` as a git submodule, matching the pattern
  of the other nine `buffr-*` crates.
- **All nine pre-existing `buffr-*` submodules bumped** to their latest tags +
  published to crates.io. The umbrella binary's behavior is unchanged from
  v0.5.3 (path-patched the whole time); this catches the published versions up
  to the code that's been shipping. Notable submodule bumps:
  - `buffr-core` 0.5.0 → 0.6.1 (`buffr-src:` CEF scheme handler, splash overlay,
    Chromium autofill / password-manager UI disabled, `hjkl-clipboard` 0.4 → 0.5
    wrapped in `Arc`)
  - `buffr-config` 0.3.0 → 0.4.0 (omnibar prefix dispatch — closes #47)
  - `buffr-ui` 0.2.0 → 0.2.1 (CI maintenance)
  - `buffr-modal` 0.1.2 → 0.1.3 (CI + CHANGELOG backfill)
  - `buffr-{bookmarks,history,permissions,zoom}` → 0.1.2 (CI maintenance)
  - `buffr-downloads` 0.1.1 → 0.1.3 (CI maintenance; 0.1.2 was a botched
    CHANGELOG cut)

### Documentation

- Web install page (`web/index.html`): removed install gaps, unified install
  styles, normalized sibling rails.

## [0.5.3] - 2026-05-06

### Changed

- **CI pipeline collapsed back into a single `ci.yml`.** The 3-stage
  `workflow_run` chain (`ci.yml` → `build.yml` → `release.yml`) had a
  fundamental flaw: `workflow_run`-triggered runs reset `head_branch` to the
  default branch, so a tag-push's ref does not propagate past the first hop.
  v0.5.2's release was skipped three times in a row before being shipped via
  manual `workflow_dispatch`. The new layout uses one workflow with job-level
  `needs:` + `if:` gates: PRs run lint+test only, pushes to `main` add the build
  matrix, and tag pushes add publish/AUR/brew. Cross-workflow
  `dawidd6/action-download-artifact@v6` was swapped for stock
  `actions/download-artifact@v4` since artifacts now live in the same run.

## [0.5.2] - 2026-05-06

### Added

- **Custom search-engine prefix dispatch in the omnibar** (closes #47).
  `[search.engines.<name>]` blocks accept an optional `prefix` shortcut. An
  omnibar input of `<prefix> <query>` routes to that engine instead of
  `default_engine` — `g rust closures` searches Google, `ddg vim folding`
  searches DuckDuckGo, plain `cats` falls through. Bare prefix words with no
  query fall through. Prefix collisions across engines are rejected at config
  validation time.

### Changed

- **CI split into a 3-stage pipeline** (`ci.yml` → `build.yml` → `release.yml`).
  `ci.yml` runs lint + test + smoke on every push/PR. `build.yml` fires via
  `workflow_run` after `ci.yml` succeeds on a push and produces every platform
  artifact. `release.yml` fires via `workflow_run` after `build.yml` succeeds
  and only proceeds when `head_branch` matches a `v*.*.*` semver tag, publishing
  the GitHub Release, AUR-bin, and Homebrew tap. PRs are filtered out of the
  build/release stages so untagged main pushes never publish.

## [0.5.1] - 2026-05-06

### Added

- **Animated splash on the new-tab page and during the loading window** (closes
  #35). Integrates `hjkl-splash` 0.2 to drive a cursor that traces each letter's
  spine of the `buffr` wordmark. The loading-anim path (when CEF hasn't painted
  yet) paints the wordmark into the chrome buffer; the new-tab page
  (`buffr://new`) renders it as a per-cell CSS grid (rectangles via
  `background: currentColor`) and the host pushes the cursor frame's HTML into
  `#buffr-splash` per tick via `execute_javascript`. Animation cadence is driven
  by `hjkl-splash`'s wall clock so paint rate (scrolling, etc.) cannot
  accelerate the wordmark.

### Fixed

- **wtype / Wayland virtual-keyboard typing into web fields** (closes #36).
  `wtype` and similar virtual-keyboard tools synthesize an xkb keymap that
  places characters on arbitrary scancodes (Escape, Backspace, Tab); the apps
  layer was forwarding those scancodes to CEF as `VK_ESCAPE` / `VK_BACK` /
  `VK_TAB`, dropping characters mid-string (a typed email address would lose its
  `.` and the password manager would jump fields). Forward via the _character_
  the text-input layer reports when it disagrees with the physical scancode;
  punctuation (`. , ; / ' [`, etc.) also now maps to the expected `VK_OEM_*`
  codes.

### Changed

- **Workspace `default-members` expanded** to all three app binaries
  (`apps/buffr`, `apps/buffr-app`, `apps/buffr-helper`). A bare `cargo build`
  now produces the full launchable set so the supervisor's sibling-binary lookup
  finds the dev `buffr-app` instead of falling through to `$PATH` and silently
  picking up the installed release binary. `cargo run` requires `-b <bin>` since
  the workspace now has multiple bins.
- `hjkl-splash` 0.1 → 0.2 (wall-clock-owned timing; consumers no longer call
  `Splash::advance` per paint).

## [0.5.0] - 2026-05-05

### Added

- **Crash + hang watchdog** (closes #28). Linux/macOS/Windows: a new `buffr`
  supervisor binary spawns the browser as a child and restarts it on crash or
  UI-thread hang. UI thread sends a 1 Hz heartbeat to the supervisor over UDS
  (Linux/macOS) or named pipe (Windows); supervisor kills + restarts after 8 s
  of no heartbeat (with 1.5 s post-connect grace) or non-zero exit. Backoff
  halts at 3 restarts within 30 s. Linux uses `setsid` + `killpg`; macOS bundles
  the supervisor as `CFBundleExecutable` inside `Buffr.app`; Windows uses Job
  Objects with `KILL_ON_JOB_CLOSE` and `SetConsoleCtrlHandler` for
  Ctrl+C/Break/close. The previous `buffr` binary is now `buffr-app`.
- **`buffr-view-source` renderer crate** (rounds 1–2 of #30) — bonsai-rendered
  `buffr-src:` scheme handler scaffolded in a dedicated crate and wired into the
  CEF resource handler. Tokyonight palette + line numbers (round 3) tracked in
  #30.

### Fixed

- **Hang on cross-app paste after copying from Facebook Messenger / other
  Wayland sites** (closes #34). Picked up `hjkl-clipboard` 0.5.3 with the
  self-paste self-pipe deadlock fix: when the buffr process owns the active
  data_source, `do_get` short-circuits to the cached payload instead of going
  through `offer.receive` + `read_fd_to_end`, which would deadlock the bg
  Wayland thread against its own `data_source.send` event.
- **Windows MSVC build** of the heartbeat client: `HANDLE` is `*mut c_void` (not
  `usize`); `GENERIC_WRITE` moved to `Win32::Foundation`; `hTemplateFile` takes
  `null_mut()`, not `0`.
- **Supervisor binary missing from Windows MSI** — CI was building only
  `buffr-app -p buffr-helper`; xtask `collect_windows_payload` requires all
  three exes. Added `-p buffr` to the Windows package step in `ci.yml` and
  `release.yml`; release.yml also dropped the dead `-p buffr-bin` package id
  left over from the pre-rename split.

### Changed

- **Binary layout: `buffr` is now the supervisor entrypoint, `buffr-app` is the
  browser.** PKGBUILD, deb/rpm, MSI, and macOS bundle install both binaries.
  Linux `.desktop` `Exec=buffr` continues to work (now invokes supervisor).
  Windows MSI shortcuts and `Start Menu` entry target `buffr.exe` (supervisor).
  `cargo run` from the workspace root launches the supervisor by default.
- `hjkl-clipboard` 0.4 → 0.5 (drops `Clone` from `Clipboard`; consumers wrap in
  `Arc` for cross-thread share). Bumped to 0.5.3 with the self-paste fix.
- Supervisor integration tests renamed `buffr-supervisor` → `buffr` to match the
  new binary name.
- CI gates `main` on macOS + Windows package jobs; `continue-on-error: true`
  removed from both, so Windows/macOS regressions now turn the pipeline red.

## [0.4.0] - 2026-05-05

### Added

- **Right-click context menu** (#23). Custom-rendered overlay
  (`buffr-ui::ContextMenuOverlay`) replaces Chromium's native menu while still
  driven by CEF's `ContextMenuHandler`. Items are bucketed by hit-test (page /
  link / image / media / editable / selection), with full action wiring
  including:
  - **Frame edit ops**: undo, redo, cut, copy, paste, paste-plain, delete,
    select-all — dispatched per-frame via CEF's `cef_frame_t` edit commands.
  - **Image ops**: copy URL, save image (off-thread fetch + PNG transcode for
    clipboard), open image in new tab, rotate clockwise / counter-clockwise.
  - **Media ops** (video / audio): play/pause, mute, loop, controls,
    picture-in-picture — JS injection via `document.elementFromPoint` with
    `querySelector` fallback for sites where the click target is a sibling (not
    ancestor) of the `<video>` element. Picture-in-picture currently no-ops on
    YouTube due to a transient-user-activation gap (tracked in #31).
  - **Page ops**: view source (`buffr-src:` prefix), reload, hard-reload, print,
    inspect element with hit-point.
  - **Tab strip click** now closes the omnibar overlay (parity with `gt`/`gT`
    once those gain non-Normal-mode bindings).
- **`buffr-src:` URL prefix** as the user-facing alias for Chromium's
  `view-source:`. Navigations to `buffr-src:<url>` rewrite to
  `view-source:<url>` at the CEF boundary; the omnibar / tab strip / session
  file see the buffr-flavored prefix uniformly.

### Fixed

- **Transparent OSR background on pages without a CSS body bg.**
  `BrowserSettings.background_color` was unset (= 0), which CEF treats as
  fully-transparent painting for windowless browsers; pages like `buffr-src:` /
  `view-source:` showed the wgpu clear colour through the OSR quad. Now opaque
  white, matching standard browser behaviour.
- **DevTools windows** now close on shutdown so the host process exits cleanly
  instead of hanging on the DevTools child window (#27).

### Changed

- `buffr-core` 0.4.0 → 0.5.0; `buffr-ui` 0.1.2 → 0.2.0.

## [0.3.0] - 2026-05-04

### Added

- **OSR sleep** when the buffr window is occluded. CEF's paint scheduler pauses
  via `was_hidden(true)` and the wgpu present pipeline short-circuits,
  eliminating the CPU/GPU spin on hidden workspaces. Driven by
  `WindowEvent::Occluded` plus a `present_us` heuristic that trips after 1
  frame > 500 ms or 3-of-5 frames > 100 ms (covers Hyprland and other
  compositors that don't fire `Occluded` on workspace switch).
- **Idle-inhibit** scaffold for issue #22 — keeps the screen awake while a video
  plays in the focused window. Trait + 4 platform backends:
  - Linux Wayland: `zwp_idle_inhibit_manager_v1`
  - Linux X11: `org.freedesktop.ScreenSaver` D-Bus
  - macOS: `IOPMAssertionCreateWithName` (`NoDisplaySleepAssertion`)
  - Windows: `SetThreadExecutionState(ES_DISPLAY_REQUIRED)` Config section
    `[idle_inhibit]` with `enabled`, `inhibit_audio_only`, `require_focus`.
    Currently inert pending the JS→Rust read-back path for
    `__buffr_video_active`.
- **JS media-activity probe** with five signal sources via the
  patched-constructor pattern:
  1. `navigator.mediaSession.playbackState === 'playing'`
  2. fullscreen `<video>`
  3. silent / muted `<video>` or `<audio>` via patched
     `HTMLMediaElement.prototype.play`
  4. WebRTC: any `RTCPeerConnection` with non-closed `connectionState`
  5. Screen Wake Lock: any un-released `WakeLockSentinel` Init script injected
     once at `on_load_end`; poll script writes `window.__buffr_media_active` and
     `window.__buffr_video_active` each ~2 s tick.
- **Audio detection** via CEF `AudioHandler` — `BrowserHost` exposes
  `any_audio_active()` and `drain_audio_events()` for embedder policies.
- Debug builds use `buffr-debug` as the in-app `APPLICATION` constant (via
  `cfg(debug_assertions)` in `buffr-config`), so `cargo run` and release
  installs no longer share `~/.cache/buffr/` and `~/.local/share/buffr/`.

### Changed

- **Render thread architecture: all wgpu mutating calls run on a dedicated
  `wgpu-render` worker thread.** The UI thread now does only
  `surface.get_current_texture()`, the chrome paint closure (CPU only), and an
  OSR pixel memcpy before sending a `RenderCommand` over a capacity-1 mailbox
  channel. `queue.write_texture`, `queue.submit`, and
  `surface_texture.present()` all happen on the worker. Fixes the multi-second
  UI freeze on Hyprland workspace switches (compositor backpressure used to
  block the UI thread inside `present()`).
- `buffr-core` dep bumped from `"0.3"` to `"0.4"`; `buffr-config` from `"0.2"`
  to `"0.3"`.

### Fixed

- **Ctrl+C now responsive** while the window is occluded — shutdown is
  dispatched via `EventLoopProxy::send_event(BuffrUserEvent::Shutdown)` from the
  `ctrlc` handler, so winit wakes immediately instead of waiting for compositor
  activity.
- **No more `wgpu` panics** on shutdown or resize during occlusion:
  - `frames_in_flight` counter gates `surface.get_current_texture()` — only one
    outstanding acquired SurfaceTexture at a time
    (`desired_maximum_frame_latency = 1`).
  - `Renderer::drop` wraps `surface` and `device` in `ManuallyDrop`; when the
    worker is mid-`present()`, the wgpu state is leaked (process is exiting)
    instead of triggering "Surface cannot be destroyed because is still in use".
  - `resize()` defers `surface.configure()` into a `pending_resize` slot when
    the worker holds an outstanding SurfaceTexture; the next `frame()` applies
    it once the worker drains.
- **`idle_inhibitor` drops before `window`** in `AppState` so the Wayland
  backend's worker doesn't reference a freed `wl_display` during shutdown.
- **Idle-inhibit Drop** uses `recv_timeout(100ms)` instead of an unconditional
  `thread::sleep(100ms)` — shutdown returns the moment the worker exits cleanly.
- **`RTCPeerConnection` patched-constructor** in `media_probe_init.js` aliases
  the `.prototype` property directly so `pc instanceof RTCPeerConnection` keeps
  working on the page.

## [0.2.1] - 2026-05-03

### Changed

- Dropped `publish-stub` job from release workflow. `buffr` on crates.io stays
  at 0.1.28 as a permanent pointer to GitHub releases; no need to bump it on
  every umbrella tag.

## [0.2.0] - 2026-05-03

### Added

- `buffr --help` now renders an ASCII-art banner (figlet "ANSI Regular" font)
  with the package version inline. Banner lives in `apps/buffr/src/art.txt`,
  embedded via `include_str!`. Regenerate with
  `figlet -f "ANSI Regular" buffr > apps/buffr/src/art.txt`.
- CLI smoke tests: `--version` returns `CARGO_PKG_VERSION`, long-form help
  contains the embedded art block and the version string.

### Changed

- **XDG-everywhere paths via `hjkl-config` 0.2 (breaking on macOS/Windows).**
  Bumps `buffr-config` 0.1.1 → 0.2.0 and `buffr-core` 0.2.0 → 0.3.0. Eliminates
  the macOS/Windows split-brain where `buffr-config` already wrote to
  `~/.config/buffr/config.toml` while `buffr-core` resolved cache + data via
  `directories::ProjectDirs` (`sh.kryptic.buffr` Bundle ID). All buffr dirs now
  honor `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` / `$XDG_CACHE_HOME` on every
  platform; Linux paths unchanged. macOS users move from
  `~/Library/Application Support/buffr/` + `~/Library/Caches/sh.kryptic.buffr/`
  to `~/.config/buffr/` + `~/.cache/buffr/`. Windows users move from
  `%APPDATA%\buffr\` + `%LOCALAPPDATA%\kryptic\buffr\cache\` to
  `~/.config/buffr/` + `~/.cache/buffr/`. See `crates/buffr-config/CHANGELOG.md`
  and `crates/buffr-core/CHANGELOG.md` for per-crate detail.

## [0.1.28] - 2026-05-03

### Added

- **Loading animation across all main-frame navigations.** Plays the buffr ASCII
  anim during reload + every navigation gap until the first contentful CEF frame
  commits, driven by a new `BrowserHost::is_loading()` flag set on
  `LoadHandler::on_load_start` and cleared by `OsrPaintHandler::on_paint`.

### Fixed

- **Ctrl+V hang in CEF input fields.** Reading the system clipboard on the main
  thread self-deadlocked: `hjkl-clipboard`'s Wayland `offer.receive` blocks on a
  pipe whose `wl_data_source.send` callback runs on CEF's UI thread (= main
  thread). Ctrl+V intercept now reads on a worker thread and posts the result
  back via `EventLoopProxy` as a `ClipboardPasteText` user event, then injects
  via `execCommand('insertText', ...)`.
- **Letterbox / "two sizes behind" CEF paint after rapid resize.** OSR frames
  now carry a `needs_fresh` flag, set by `osr_resize` and cleared only on
  successful main-frame paint. The freshness gate now requires a post-resize
  paint before re-presenting the OSR buffer, preventing persisted stale dims
  from sticking after a resize burst.
- **Stale-size swapchain texture acquired during rapid resize.** `render.rs`
  drops textures whose dims don't match the current surface, reconfigures, and
  retries up to 2× before skipping the frame.
- **Image clipboard paste no longer spams "MIME type not supported".** Ctrl+V
  intercept falls through to CEF when the clipboard isn't text. Image paste is
  still unsupported in OSR (tracked in #19).

### Changed

- `hjkl-clipboard` 0.3 → 0.4 (Wayland data-control protocol, multi-MIME reads).
  Subsequent bump to 0.4.8 picks up Wayland thread respawn fix.
- Workspace `Cargo.toml` routes all `buffr-*` path overrides through
  `[patch.crates-io]` so consumers without the submodule init get crates.io
  versions automatically — matches the pattern used in hjkl.
- `apps/buffr` no longer depends on `hjkl-clipboard` directly; clipboard reads
  go through the new opaque `buffr_core::ClipboardReader::read_text()`.

## [0.1.27] - 2026-05-03

### Fixed

- Surface-size drift on rapid resize self-heals: the wgpu surface now
  reconfigures when its cached dims diverge from the window's current size.
- `single_instance` test module compiles clean on Windows (`dead_code` allow for
  `socket_path`) and Linux clippy.

## [0.1.26] - 2026-05-03

### Added

- **Single-instance launching with URL forwarding.** A second `buffr <url>`
  invocation forwards the URL to the existing process via Unix socket / Windows
  named pipe and exits. The running window opens the URL in a new tab. Disabled
  by passing `--no-single-instance`.
- **Per-monitor HiDPI scaling.** Live device-scale plumbing: scale changes (drag
  between displays, fractional scaling toggle) propagate to CEF via
  `RenderHandler::screen_info` without restart.
- **ASCII-only loading frames** rendered when the CEF buffer is unusable
  (initial paint, between navigations, surface-drift recovery).
- Bilinear favicon scaling in the tab strip (replaces nearest-neighbour).

### Fixed

- **Resize flicker / stale CEF paints rejected.** Generation-based OSR freshness
  gate; CEF paints at stale dims are dropped instead of presented. Watchdog
  forces a repaint when CEF skips a post-resize paint.
- Resize debounce recomputes target dims at flush so terminal sizes settle to
  the most recent window size, not an intermediate one.
- `resync_cef_rect` now routes through `osr_resize` so the freshness gate fires
  on programmatic resizes (not just user drags).
- Forced chrome repaint when the loading animation deactivates so the modeline /
  tab strip doesn't render against a stale frame.
- Unified `.desktop` file under `pkg/buffr.desktop` (was duplicated).

### Changed

- `/vendor` tree gitignored entirely.

## [0.1.25] - 2026-05-02

### Added

- ASCII loading animation when the CEF buffer is unusable.

## [0.1.24] - 2026-05-01

### Added

- Use the buffr.kryptic.sh website icon in the desktop entry.

## [0.1.23] - 2026-05-01

### Added

- AUR `buffr-bin` runtime tarball now bundles the `.desktop` entry and icon so
  `pacman -S buffr-bin` produces a launcher in the application menu.

## [0.1.22] - 2026-05-01

### Fixed

- AUR auto-publish: ssh options injected via `GIT_SSH_COMMAND` env var
  (previously dropped by the wrapper).

## [0.1.21] - 2026-05-01

### Fixed

- AUR auto-publish: `accept-new` host-key policy so the first push from a fresh
  runner doesn't fail on missing `known_hosts`.

## [0.1.20] - 2026-05-01

### Fixed

- AUR auto-publish: corrected `su` option ordering so `makepkg` actually runs as
  the build user.

## [0.1.19] - 2026-05-01

### Fixed

- AUR auto-publish: provision build user, ship `LICENSE` + `.gitignore`,
  obfuscate maintainer email in PKGBUILD per AUR convention.

## [0.1.18] - 2026-05-01

### Fixed

- **Linux release binaries find their own libcef.** `RUNPATH=$ORIGIN` baked into
  the binary at link time, so the runtime tarball works without setting
  `LD_LIBRARY_PATH`.

## [0.1.17] - 2026-04-30

### Added

- **AUR auto-publish on release.** `buffr-bin` PKGBUILD bumps and pushes to the
  AUR on every `v*` tag.

## [0.1.16] - 2026-04-30

### Fixed

- Release: extract tarball with `--strip-components` for the Flatpak + Snap
  bundle steps.

## [0.1.15] - 2026-04-30

### Added

- **Flatpak + Snap bundles** ship on every release alongside the existing
  `.deb`, `.rpm`, `.tar.gz`, `.dmg`, `.msi` artifacts.

## [0.1.14] - 2026-04-30

### Removed

- **Intel Mac (`x86_64-apple-darwin`) release leg.** GitHub Actions `macos-13`
  runner pool is heavily contended (1–2 hour queue times blocking the publish
  pipeline). Apple stopped selling Intel Macs in 2023; the cost wasn't paying
  for the user count. Source still builds clean against `x86_64-apple-darwin`,
  the support is just absent from the release pipeline.

## [0.1.13] - 2026-04-30

### Added

- Linux release emits a portable `.tar.gz` of the runtime tree alongside the
  `.deb` / `.rpm` packages.

## [0.1.12] - 2026-04-30

### Added

- **Expanded release matrix:** Linux aarch64, Windows arm64, and (briefly,
  reverted in 0.1.14) Intel Mac binaries.

## [0.1.11] - 2026-04-30

### Added

- Linux release now produces a `.rpm` package (Fedora / RHEL / openSUSE).

## [0.1.10] - 2026-04-30

### Removed

- AppImage build. The `.deb` + `.tar.gz` outputs cover the same ground without
  the AppImage runtime overhead.

## [0.1.9] - 2026-04-30

### Changed

- **Windows now uses OSR rendering**, matching Linux and macOS. Brings
  cross-platform parity for the wgpu compositor + overlay UI; the previous
  Windows-only windowed-CEF path is gone.

## [0.1.8] - 2026-04-30

### Added

- **Linux HiDPI scale forwarded to CEF** via env vars at child-process spawn
  (`GDK_SCALE`, etc), so fractional scaling on Wayland renders pages at the
  right DPR instead of CEF's default 1.0.
- Auto-publish the `buffr` crates.io stub from the release pipeline once all
  platform artifacts upload.

### Changed

- `apps/buffr-stub/target` no longer tracked in the build artifact gitignore.

## [0.1.7] - 2026-04-30

### Fixed

- **macOS DMG preserves the executable bit** on `Contents/MacOS/buffr`.
  Previously dropped by the staging step, leaving the `.app` non-runnable.

## [0.1.6] - 2026-04-30

### Fixed

- Windows MSI: suppress ICE64 alongside ICE38/ICE91 in `light.exe` for the
  per-user install.

## [0.1.5] - 2026-04-30

### Fixed

- Windows MSI: suppress ICE38/ICE91 in `light.exe` so the per-user install
  validates.

## [0.1.4] - 2026-04-30

### Fixed

- **Windows MSI bundles the full CEF runtime** (libcef, paks, locales) via
  `heat.exe`. Previously the MSI was missing libcef and refused to launch.

## [0.1.3] - 2026-04-30

### Fixed

- **Windows MSI is now per-user** (no UAC prompt) — `cargo install`-style
  no-admin install on Windows.

## [0.1.2] - 2026-04-30

### Added

- **Per-tab favicons** rendered in the tab strip via CEF `download_image`.
- **Native cursor forwarding.** CEF cursor changes (text, pointer, resize, …)
  now propagate to winit on the main window and popup windows.
- **Live find highlight** with 300ms debounce — search results highlight as you
  type, not just on enter.
- **Per-mode chrome theming** via HSL hue rotation off a single accent colour
  (`--accent`). `[theme] accent = "#7aa2f7"` drives modeline, selection, hint
  chips, and prompt cursor.
- `[general] show_favicons` config toggle (default `on`).

### Fixed

- **Hint mode keeps matched hints bright** while dimming non-matches; the typed
  prefix is struck through on the matching hint chip.
- Favicon list iterates raw `cef_string_list_t` instead of cloning (clone
  produced an empty list on the cef-rs 147 binding).
- macOS CEF init uses `external_message_pump` and a dev keychain workaround for
  codesigning identity discovery.

### Changed

- **Workspace crates extracted into standalone submodule repos** (matches the
  hjkl pattern). `[patch.crates-io]` overrides used for local dev; fresh
  checkouts without submodule init resolve against crates.io.
- Buffr is now an install-instructions stub on crates.io (`apps/buffr-stub`);
  the real binary at `apps/buffr` was renamed to `buffr-bin` to dodge the
  package-name collision. The produced binary is still `target/<profile>/buffr`.
- hjkl-\* deps caret-pinned (no longer exact-version pinned).
- Submodule checkouts in CI are now recursive.

## [0.1.1] - 2026-04-29

### Added

- **Ctrl+V paste** in the omnibar / command line / find overlay (clipboard text
  pushed into the input buffer, CR/LF stripped) and in CEF-focused page inputs
  (JS-injected via `document.execCommand('insertText', ...)`). Needed because
  CEF on Wayland can't read the system clipboard itself even when the keystroke
  reaches it.

### Fixed

- **Clipboard reads on Wayland.** `clipboard_text()` now falls back to
  `wl-paste -n` when arboard returns empty under `WAYLAND_DISPLAY` — symmetric
  to the existing `wl-copy` write fallback. Same root cause: arboard's
  wl-data-source ownership lives in a worker thread that doesn't reliably serve
  other clients.

### CI

- Tag-driven `release.yml` builds Linux AppImage + .deb, macOS .dmg, and Windows
  .msi on `v*` tag pushes and uploads them to the GitHub Release with sha256
  sidecars. crates.io publishing intentionally manual (11 workspace crates would
  hit the new-crate rate limit).
- `ci.yml` now also accepts `workflow_dispatch:` for manual re-runs.

## [0.1.0] - 2026-04-29

First tagged release. Multi-tab browsing with OAuth-capable popups, modal vim
keybindings, GPU-accelerated chrome compositor, and per-origin data layers
(history / bookmarks / downloads / permissions / zoom) all wired and persisted.

### Added

- **Popup windows.** `window.open(...)` and other `NEW_POPUP` / `NEW_WINDOW`
  dispositions now render in a dedicated buffr winit window with a read-only
  address-bar strip at top (no tab strip, no statusline). Preserves CEF's native
  `window.opener` reference so OAuth flows that `postMessage` back to the opener
  work end-to-end. Multiple concurrent popups supported, each with its own
  browser, history, and lifecycle. JS-driven `window.close()` and opener-driven
  `popup.close()` shut the popup window down cleanly.
- **Two-finger horizontal swipe → back / forward.** Touchpad PixelDelta events
  accumulate horizontally; once the gesture crosses 150 px while staying ≥ 2×
  more horizontal than vertical, fire HistoryBack (swipe right) or
  HistoryForward (swipe left). Works in the main window and in popup windows;
  popups navigate their own browser history.
- **`target="_blank"` and Ctrl+click open in new tabs.** Disposition-aware
  `LifeSpanHandler::on_before_popup` plus a new
  `RequestHandler::on_open_urlfrom_tab` route `NEW_FOREGROUND_TAB` /
  `NEW_BACKGROUND_TAB` through our tab queue while leaving popup dispositions to
  CEF's native handling.

### Fixed

- **Wayland top-edge resize artifacts.** Eliminated black bars / bottom-bar gap
  during interactive top-edge drags on Hyprland. CEF is notified on every winit
  `Resized` event (no debounce); the renderer GPU-stretches whatever frame CEF
  most recently emitted to fill the live browser_rect.
- **Popup focus on click.** Wayland doesn't reliably emit `WindowEvent::Focused`
  on click, so we explicitly call `set_focus(true)` on the popup's CEF browser
  when a press lands inside the OSR content area, ensuring DOM caret state and
  keyboard input route correctly.
- **Popup scroll speed.** Popup wheel handler now uses the same
  `winit_wheel_to_cef_delta` helper as the main window (10× scale on PixelDelta)
  so touchpad scrolling feels identical across windows.

### Changed

- **Resize pipeline simplified.** Dropped ~145 LOC of debounce / throttle /
  double-slot logic. Single OSR texture, GPU-stretched on dim mismatch, CEF told
  the size on every Resized event.

### Documentation

- Workspace READMEs polished to match the hjkl reference style: per-crate
  badges, public-API tables, architecture overviews. New READMEs for
  `apps/buffr`, `apps/buffr-helper`, `buffr-config`, `buffr-core`,
  `buffr-modal`, and `buffr-ui`.

### Changed (workspace deps)

- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.25` to
  `=0.0.26`. Pulls in hjkl Phase 5 trait extraction (`spec::*` re-exports,
  optional `ratatui` on `hjkl-engine`, new ratatui-free Editor methods). Buffr
  does not yet depend on `hjkl-editor` and uses no `Rect`-flavoured APIs, so
  this is a transparent pin bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.26` to
  `=0.0.28` — adopts canonical Buffer impl (0.0.27) plus sticky_col + iskeyword
  hoist (0.0.28). Buffr only uses editor-level accessors, so the
  `hjkl_buffer::Buffer` API breaking change in 0.0.28 is transparent here.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.28` to
  `=0.0.29` — picks up Patch B, which wires the `Host` trait through `Editor`.
  The Host surface itself is unchanged and `BuffrHost` already implements all 10
  SPEC methods; the back-compat `Editor::new` shim wraps `DefaultHost`, so no
  Buffr source changes are required. Migration to
  `Editor::with_host(km, BuffrHost::new())` is left for a follow-up.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.29` to
  `=0.0.30` — picks up Patch C-α, which relocates the motion vocabulary out of
  `hjkl_buffer::Buffer` inherent methods into the `hjkl_engine::motions` module.
  Buffr only consumes editor-level APIs, so the consumer-side change is a pin
  bump only — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.31` to
  `=0.0.32` — picks up Patch C-β (partial): deprecated aliases dropped,
  `_xy`/`_xywh` asymmetries resolved (`mouse_click_in_rect`,
  `mouse_extend_drag_in_rect`, `cursor_screen_pos_in_rect`,
  `install_ratatui_syntax_spans`, `intern_ratatui_style`), and the additive
  `FoldProvider` trait shipped. Buffr has no call sites against the renamed or
  removed symbols, so this is a transparent pin bump — no source changes
  required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.32` to
  `=0.0.33` — picks up Patch C-γ (partial). Buffr has no source migration to
  perform, so this is a transparent pin bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.33` to
  `=0.0.34` — picks up Patch C-δ.1, which relocates `Viewport` ownership from
  `hjkl_buffer::Buffer` onto `hjkl_engine::Host`. `BuffrHost` now carries a
  `viewport: Viewport` field and implements the new `Host::viewport` /
  `Host::viewport_mut` accessors. A `set_viewport_size(width, height)` helper is
  exposed for the eventual resize wiring; until edit-mode is plumbed into the
  CEF/winit page lifecycle in `apps/buffr`, the viewport stays at zero-size and
  the engine's scroll math no-ops. No `buffer().viewport*()` reaches in buffr,
  so the migration is contained to `BuffrHost`.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.34` to
  `=0.0.35` — picks up the search FSM migration from `hjkl_buffer::Buffer` onto
  `hjkl_engine::Editor`. Buffr does not drive search through the Buffer API per
  the consumer audit, so this is a transparent pin bump — no source changes
  required. First of a 5-patch path toward hjkl 0.1.0.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.35` to
  `=0.0.36` — picks up the named-marks consolidation, relocating mark storage
  and operations from `hjkl_buffer::Buffer` onto `hjkl_engine::Editor`. Buffr
  does not interact with the marks API directly, so this is a transparent pin
  bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.36` to
  `=0.0.37` — relocates `spans` and `search_pattern` out of
  `hjkl_buffer::Buffer` onto `hjkl_engine::BufferView`, which now carries the
  `spans` and `search_pattern` fields. Buffr does not consume these fields
  directly per the consumer audit, so this is a transparent pin bump — no source
  changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.37` to
  `=0.0.38` — introduces the `FoldOp` / `FoldProvider::apply` pipeline on
  `hjkl_engine`, threading fold operations through the editor host. Buffr does
  not implement a fold provider and consumes only editor-level APIs, so this is
  a transparent pin bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.38` to
  `=0.0.39` — adds `Query::dirty_gen` for cache invalidation on the syntax query
  layer. Buffr consumes only editor-level APIs, so this is a transparent pin
  bump — no source changes required.

[Unreleased]: https://github.com/kryptic-sh/buffr/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.12.0
[0.11.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.11.1
[0.11.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.11.0
[0.10.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.10.0
[0.9.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.9.1
[0.9.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.9.0
[0.8.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.8.1
[0.8.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.8.0
[0.7.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.7.1
[0.7.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.7.0
[0.6.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.3
[0.6.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.2
[0.6.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.1
[0.6.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.0
[0.5.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.3
[0.5.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.2
[0.5.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.1
[0.5.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.0
[0.4.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.3.0
[0.2.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.2.0
[0.1.28]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.28
[0.1.27]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.27
[0.1.26]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.26
[0.1.25]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.25
[0.1.24]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.24
[0.1.23]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.23
[0.1.22]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.22
[0.1.21]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.21
[0.1.20]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.20
[0.1.19]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.19
[0.1.18]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.18
[0.1.17]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.17
[0.1.16]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.16
[0.1.15]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.15
[0.1.14]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.14
[0.1.13]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.13
[0.1.12]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.12
[0.1.11]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.11
[0.1.10]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.10
[0.1.9]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.9
[0.1.8]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.8
[0.1.7]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.7
[0.1.6]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.6
[0.1.5]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.5
[0.1.4]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.4
[0.1.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.3
[0.1.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.0
