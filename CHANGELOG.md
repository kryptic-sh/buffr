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
