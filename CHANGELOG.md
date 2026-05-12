# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Crash-loop detection** (#61). `apps/buffr-app/src/crash_guard.rs` tracks
  recent startup timestamps in `<data_dir>/launch.json`. Three startups inside a
  60-second window without a clean exit between them is treated as a crash loop:
  the saved `session.json` is moved aside to `session.json.crashed-<unix_ts>`
  and the new launch starts from the homepage instead of restoring the killer
  URL set. Graceful shutdown paths (last-tab-close, `:q`, window close, Ctrl+C)
  clear the tracker. Skipped under `--private`.

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

[Unreleased]: https://github.com/kryptic-sh/buffr/compare/v0.6.1...HEAD
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
