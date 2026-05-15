# Changelog

All notable changes to `buffr-cef` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] - 2026-05-15

### Fixed

- `on_dismiss_permission_prompt` now removes the dismissed entry from the
  callback registry and neutral queue, eliminating slow memory growth from
  dangling prompt references. Audit finding #8.
- `sanitise_filename` filters Windows reserved device stems (`CON`, `PRN`,
  `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`) by prepending an underscore.
  Applied on all platforms for cross-platform profile portability. Audit finding
  #10.

### Removed

- `tracing::debug!` calls from `view_rect` and `get_screen_info`; CEF invokes
  these per-paint callback (60+ Hz), so the tag-and-format overhead measured
  even with the subscriber disabled. Audit finding #15.

## [0.1.1] - 2026-05-15

### Added

- `tracing::debug!` log when `BackendOpenOptions.cache_dir` is `Some` — surfaces
  the silent drop so multi-engine setups can confirm the difference between CEF
  and blink-cdp on-disk layouts. CEF has no API to split persistent vs ephemeral
  state; both live under `cache_path`. Phase 11 polish (#96).

## [0.1.0] - 2026-05-15

### Added

- Initial extraction from the `kryptic-sh/buffr` umbrella into a standalone
  repository via `git subtree split`. CEF browser host, OSR paint handler, audio
  handler, permissions handler, new-tab scheme, view-source scheme, and all
  other CEF-specific modules that were extracted from `buffr-core` 0.7.
- `buffr-core = "0.7"` dep — picks up the engine-agnostic shell.

[Unreleased]: https://github.com/kryptic-sh/buffr-cef/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/kryptic-sh/buffr-cef/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-cef/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/buffr-cef/releases/tag/v0.1.0
