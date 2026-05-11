# Changelog

All notable changes to `buffr-core` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.1] — 2026-05-12

### Changed

- `buffr-config` dep bumped from `"0.3"` to `"0.4"` to pick up
  `SearchEngine::prefix` (omnibar prefix dispatch). No public API change in
  `buffr-core` itself.

## [0.6.0] — 2026-05-12

### Added

- **`buffr-src:` is now a real CEF custom scheme** (round 2/3 of #30).
  `BuffrSrcSchemeHandlerFactory` fetches the underlying URL on a worker thread,
  renders via `buffr_view_source::render`, and serves the bonsai-highlighted
  HTML back to CEF. The old navigation-boundary `view-source:` rewrite is
  dropped; `merge_navigation_url` and `to_cef_navigation_url` are now
  pass-through. New module `view_source_scheme`.
- **New-tab splash overlay.** Host pushes the splash frame's HTML into
  `#buffr-splash` via `execute_javascript` every splash tick instead of painting
  an overlay into the chrome buffer. `assets/new_tab.html` reserves vertical
  space (`min-height: 5em`) so the page doesn't reflow on the first push; `.hl`
  styles cursor/trail spans with the accent.

### Changed

- **Chromium autofill server + password-manager UI disabled.** buffr will ship
  its own autofill surface on top; Chromium's built-in mid-typing field rewrites
  and password-manager onboarding popups are silenced.
- **`hjkl-clipboard` bumped 0.4 → 0.5; `Clipboard` wrapped in `Arc`.** Aligns
  with the upstream API and lets multiple subsystems share one clipboard handle.
- CI maintenance: collapsed two-stage CI (ci.yml + release.yml) into a single
  tag-driven `ci.yml`, added Dependabot config (cargo + github-actions, weekly),
  and renamed the workflow to PascalCase.

### Fixed

- **New-tab splash now renders with consistent monospace metrics.** Stack two
  `<pre>` layers so cursor and trail glyphs line up; pin the splash font to one
  with consistent `█` metrics; opt out of all shaping with bare `monospace`;
  render via a per-cell CSS grid instead of a single `<pre>` to eliminate
  sub-pixel drift between rows.

### Documentation

- macOS comment: rename `buffr.app` → `Buffr.app` to match the actual bundle
  name.

## [0.5.0] — 2026-05-05

### Added

- **Per-frame edit ops** (`BrowserHost::frame_undo`, `frame_redo`, `frame_cut`,
  `frame_copy`, `frame_paste`, `frame_paste_plain`, `frame_del`,
  `frame_select_all`) — dispatch CEF `cef_frame_t` edit commands to the active
  tab's focused frame, falling back to `main_frame()`.
- **`BrowserHost::reload_ignore_cache_active`** — hard-reload the active tab
  without the cache.
- **`BrowserHost::print_active`** — open the system print dialog for the active
  tab via `CefBrowserHost::Print`.
- **`BrowserHost::start_download`** — trigger a file download for an arbitrary
  URL via `CefBrowserHost::StartDownload`.
- **`BrowserHost::show_dev_tools_at`** — open DevTools for the active tab with
  an optional element-inspect hit-point (`cef::Point`). Wraps
  `CefBrowserHost::ShowDevTools`.
- **`image_copy` module + `BrowserHost::copy_image_url_to_clipboard`** — fetch
  an image URL off-thread (or decode a `data:` URL inline), transcode to PNG,
  and write to the system clipboard via `hjkl-clipboard` `MimeType::Png`. Falls
  back to copying the URL as text on backends that don't carry image MIME (OSC52
  over SSH). Adds `image` (PNG/JPEG/WebP/GIF only) and `base64` deps.
- **Media JS injection helpers** (`BrowserHost::media_play_pause`,
  `media_toggle_mute`, `media_toggle_loop`, `media_toggle_controls`,
  `media_picture_in_picture`) — fire-and-forget JS snippets that resolve the
  `<video>`/`<audio>` element under the right-click coordinates via
  `document.elementFromPoint` and toggle the corresponding DOM property.
  `media_picture_in_picture` targets `<video>` only and wraps the PiP API in a
  `try/catch` for hosts that disable PiP.
- **Image rotate helper** (`BrowserHost::image_rotate`) — resolves the `<img>`
  element under click coordinates and increments `el.dataset.buffrRotate` by
  `delta_deg`, then sets `el.style.transform = rotate(Ndeg)`. Rotations compose
  across successive calls.
- **`ContextMenuItem::RotateClockwise` / `RotateCounterclockwise`** — new
  variants added to the image bucket (`build_model` emits them for
  `TYPEFLAG_MEDIA + MEDIATYPE_IMAGE` right-clicks).
- **`buffr-src:` URL prefix as the user-facing alias for Chromium's
  `view-source:`.** Navigations to `buffr-src:<url>` rewrite to
  `view-source:<url>` at the CEF boundary; the omnibar / tab strip / session
  file see the buffr-flavored prefix uniformly. Incoming address-change events
  that strip the prefix (Chromium peels it for tracking) are detected and the
  prefixed form preserved on `Tab.url`. Public const `BUFFR_SRC_PREFIX`.

### Fixed

- **Media context-menu actions no-op on overlay-heavy sites (YouTube, etc.).**
  `media_play_pause`, `media_toggle_mute`, `media_toggle_loop`,
  `media_toggle_controls`, and `media_picture_in_picture` now fall back to
  `document.querySelector('video, audio')` (or `'video'` for PiP) when the
  `elementFromPoint` → `parentElement` walk fails to find the media element. CEF
  only emits these menu items when it already knows the right-click target is a
  video/audio, so picking the document's first match is the correct fallback.
- **`view-source:` prefix lost from `Tab.url` on first address-change.** Was
  causing the omnibar to pre-fill with the underlying URL when the user opened
  it on a view-source page; now subsumed into the `buffr-src:` rename above via
  `merge_navigation_url`.
- **Transparent OSR background on pages without a CSS body bg.**
  `BrowserSettings.background_color` was unset (= 0), which CEF treats as
  fully-transparent painting for windowless browsers; pages like `buffr-src:` /
  `view-source:` then showed the wgpu clear colour (`0x1a1b26`) through the OSR
  quad. Now set to opaque white (`0xFFFFFFFF`) so CEF fills un-painted regions,
  matching standard browser behaviour.

## [0.4.0] — 2026-05-04

### Added

- **OSR sleep API** (`BrowserHost::osr_sleep`, `osr_invalidate_view`,
  `force_repaint_active`) — pauses CEF's paint scheduler via `was_hidden(true)`
  when the embedder decides the surface is occluded.
- **Audio detection** — new `audio` module with `BuffrAudioHandler` wired into
  the CEF client, plus `BrowserHost::any_audio_active()` and
  `drain_audio_events()` for embedder polling.
- **JS media-activity probe with patched-constructor signals** for silent video,
  WebRTC, and Screen Wake Lock. New asset files `media_probe_init.js` (page-load
  patches) and `media_probe_poll.js` (per-tick read).
  `BrowserHost::run_media_probe()` fires the poll; `any_video_active()` is wired
  but currently a stub pending the console-log sentinel reader.
- **`inhibit` module** — `IdleInhibitor` trait + `new_inhibitor()` factory +
  four platform backends:
  - Linux Wayland: `zwp_idle_inhibit_manager_v1` via raw `wayland-client` on a
    guest backend that shares winit's `wl_display`.
  - Linux X11: `org.freedesktop.ScreenSaver.Inhibit` D-Bus via `zbus::blocking`.
  - macOS: `IOPMAssertionCreateWithName`/`IOPMAssertionRelease` via direct IOKit
    FFI.
  - Windows: `SetThreadExecutionState(ES_DISPLAY_REQUIRED|ES_CONTINUOUS)` on a
    dedicated worker thread to preserve thread affinity.
- `winit = "0.30"` added as a dep — `new_inhibitor` takes
  `Arc<winit::window::Window>`.

### Changed

- `profile_paths()` now reads `<buffr_config::Config as AppConfig>::APPLICATION`
  instead of hard-coding `"buffr"`, so debug builds get a separate
  `~/.cache/buffr-debug/` and `~/.local/share/buffr-debug/`.
- `buffr-config` dep bumped from `"0.2"` to `"0.3"` for `IdleInhibitConfig`.

### Fixed

- `loading_busy` now cleared on `LoadHandler::on_loading_state_change` so the
  loading animation deactivates promptly when CEF reports done.
- `RTCPeerConnection` patched-constructor in `media_probe_init.js` aliases
  `.prototype` directly so `pc instanceof RTCPeerConnection` still works on the
  page (was breaking due to a `setPrototypeOf` mismatch).
- Idle-inhibit Drop uses `recv_timeout(100ms)` instead of an unconditional
  `thread::sleep(100ms)`, so shutdown returns the moment the worker exits
  cleanly.

## [0.3.1] — 2026-05-03

### Fixed

- `buffr-config` dep constraint bumped from `"0.1"` to `"0.2"`. 0.3.0 was
  published with the stale pin, which prevented it from resolving alongside the
  new `buffr-config 0.2.0` on crates.io.

## [0.3.0] — 2026-05-03

### Changed

- **`profile_paths()` migrated to `hjkl-config` 0.2 (XDG-everywhere).** Cache +
  data dirs now come from `hjkl_config::cache_dir("buffr")` /
  `hjkl_config::data_dir("buffr")` instead of `directories::ProjectDirs`. Fixes
  a split-brain on macOS/Windows where `buffr-config` already routed through
  `hjkl-config` (writing `~/.config/buffr/config.toml`) but `buffr-core` was
  still resolving cache + data via the old `sh.kryptic.buffr` Bundle ID layout.
  Now every dir buffr touches is XDG-everywhere.
- macOS users: cache moves from `~/Library/Caches/sh.kryptic.buffr/` to
  `~/.cache/buffr/`; data moves from
  `~/Library/Application Support/sh.kryptic.buffr/` to `~/.local/share/buffr/`.
- Windows users: cache moves from `%LOCALAPPDATA%\kryptic\buffr\cache\` to
  `~/.cache/buffr/`; data moves from `%APPDATA%\kryptic\buffr\data\` to
  `~/.local/share/buffr/`.
- Linux users: paths unchanged (`~/.cache/buffr/`, `~/.local/share/buffr/`).
- Replaced `directories` dep with `hjkl-config = "0.2"`.

`CoreError::NoProjectDirs` variant name preserved for back-compat; semantics
widen slightly to "no XDG home dir resolvable" (only fires in sandboxed envs
without `$HOME`).

## [0.2.0] — 2026-05-03

### Added

- **`ClipboardReader`** opaque newtype + `BrowserHost::clipboard_handle()` so
  embedders can read the system clipboard from a worker thread without depending
  on `hjkl-clipboard` directly. `read_text()` performs the blocking Wayland read
  off the CEF UI thread to avoid the self-deadlock when Chromium owns the
  selection.
- **`BrowserHost::is_loading()`** flag, set by `BuffrLoadHandler::on_load_start`
  on main-frame loads and cleared by the next successful
  `OsrPaintHandler::on_paint`. Lets the embedder keep a loading animation
  playing across the navigation gap until the first contentful frame.
- **`BrowserHost::force_repaint_active`** atomic flag for embedder watchdogs to
  nudge a stuck CEF renderer via a `was_hidden` cycle.
- **`OsrFrame::needs_fresh`** flag set by `osr_resize` and cleared by the next
  successful main-frame paint. Lets the embedder's freshness gate reject
  persisted-but-stale paints after a resize burst.
- `RenderHandler::screen_info` plumbing for live device-scale changes
  (per-monitor HiDPI, fractional scaling toggle).

### Changed

- **`hjkl-clipboard` 0.3 → 0.4.** `Clipboard` becomes `Clone + Send + Sync`,
  enabling the worker-thread read pattern. New `Selection` / `MimeType` API.
- All paint / load handlers now plumb `loading_busy: Arc<AtomicBool>` through
  the factory functions.
- `OsrPaintHandler::on_paint` clears `needs_fresh` and `loading_busy` on
  successful main-frame paints.
- `osr_resize` invalidates the OSR view (`invalidate(VIEW)`) after tab
  activation so newly-fronted tabs commit a fresh paint.

### Fixed

- **Persistent letterbox / "two sizes behind" paint after rapid resize.**
  Before: the freshness gate accepted any paint at the right dims even if it was
  buffered from before the resize. After: `needs_fresh` requires a post-resize
  paint before re-presenting.

## [0.1.3] — 2026-04-30

### Fixed

- `build.rs` stages all CEF `Release/` DLLs and JSONs on Windows. Previously the
  build script missed runtime files needed by `cargo run` from a fresh checkout.

## [0.1.2] — 2026-04-30

### Changed

- `hjkl-clipboard` dep relaxed from exact-pin to caret `0.3` so consumers can
  pick up patch fixes without a buffr-core re-publish.

## [0.1.1] — 2026-04-30

### Changed

- Extracted from the `kryptic-sh/buffr` umbrella into a standalone repository
  with full git history preserved via `git subtree split`.
- Added per-repo CI (fmt / clippy / test matrix / cargo-deny) and a tag-driven
  release workflow that publishes idempotently to crates.io.

[Unreleased]: https://github.com/kryptic-sh/buffr-core/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.6.1
[0.6.0]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.6.0
[0.5.0]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.5.0
[0.4.0]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.4.0
[0.3.1]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.3.1
[0.3.0]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.3.0
[0.2.0]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.2.0
[0.1.3]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.1.3
[0.1.2]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-core/releases/tag/v0.1.1
