# Changelog

All notable changes to `buffr-engine` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-05-18

### Removed

- `BrowserEngine::image_rotate` trait method and the
  `media_js::image_rotate` JS helper. The right-click "Rotate image"
  context-menu action provided no real value (CSS-transform only, not
  persisted, modern images carry correct EXIF) and was retired across
  all backends rather than maintained for parity.

### Changed

- Doc comments on `BrowserEngine`, `BackendOpenOptions`, `EngineId`,
  `PermissionsQueue`, and `EngineError::Unimplemented` reworked to drop
  references to the retired `buffr-blink-cdp`, `buffr-firefox-cdp`, and
  `buffr-webkitgtk` backends. The umbrella drops these three engines in favour
  of CEF, WPE WebKit, WKWebView, WebView2, Blitz, and Ladybird. The minor
  bump reflects the trait-method removal and the umbrella-wide engine
  roster change so consumers pinning specific engine names in routing
  config can spot the drop in one place.

## [0.1.7] - 2026-05-18

### Added

- `BackendOpenOptions.wayland_handles: Option<WaylandNativeHandles>` — threads
  raw Wayland handles from `buffr-app` directly into engine construction so the
  `BuffrDisplayWayland` C subclass (#152) can consume them before the GLib
  worker thread's first `WpeRuntime::new` call. Eliminates the post-construction
  `set_native_wayland_handles` setter race.

## [0.1.6] - 2026-05-17

### Added

- `WaylandNativeHandles` struct in `types` module — carries the raw Wayland
  (`wl_display`, `parent_wl_surface`, `wl_compositor`, `wl_subcompositor`) and
  EGL (`egl_display`) pointers extracted from the host winit window on Wayland
  sessions. Re-exported at crate root. Consumed by `buffr-webkit` (#151) to wire
  platform handles to `WebKitEngine` so the upcoming `BuffrDisplayWayland` C
  subclass (#152) can read them without passing raw pointers through the trait.

## [0.1.5] - 2026-05-17

### Added

- `BackendOpenOptions.prefer_native: bool` — opt-in flag for backends that
  support native compositing. Defaults to `false`; existing callers remain on
  the OSR path. Consumed by `buffr-webkit` (#144) to switch to
  `WPEDisplayWayland` on Wayland sessions.

## [0.1.3] - 2026-05-15

### Changed

- `#[non_exhaustive]` annotation added to public enums `EngineEvent`,
  `LoadState`, `CursorKind`, `MediaType` so downstream crates cannot
  exhaustively match on variants we may add in future minor releases. Audit
  finding #12.
- `BrowserEngine::can_go_back()` and `can_go_forward()` trait defaults changed
  from `true` to `false`. A freshly opened tab has no history, so the previous
  defaults misled UIs into showing the back button as enabled. Both CEF and
  blink-cdp override these methods, so concrete behavior is unchanged. Audit
  finding #17.

## [0.1.2] - 2026-05-15

### Added

- `BackendOpenOptions.cache_dir: Option<&Path>` — optional ephemeral cache
  directory for backends that support a persistent/ephemeral split. Backends
  that do not support a split (e.g. CEF) ignore this field. Phase 11b (#96).

## [0.1.1] - 2026-05-15

### Added

- `BackendOpenOptions.find_sink: Option<Arc<dyn Any + Send + Sync>>` field to
  thread the apps-layer find-result sink through the Backend trait path. Lets
  backend impls populate the same `FindResultSink` the apps layer drains,
  eliminating the broken "find_sink: None" path through
  `BlinkCdpBackend::open_engine`. Audit fix #P1-1.

## [0.1.0] - 2026-04-01

_Initial release._

[Unreleased]: https://github.com/kryptic-sh/buffr-engine/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.2.0
[0.1.7]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.7
[0.1.6]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.6
[0.1.5]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.5
[0.1.4]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.4
[0.1.3]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.3
[0.1.2]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.0
