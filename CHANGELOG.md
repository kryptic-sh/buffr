# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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
