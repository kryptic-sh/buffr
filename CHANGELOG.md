# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
