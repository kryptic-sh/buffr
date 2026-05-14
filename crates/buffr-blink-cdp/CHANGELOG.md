# Changelog

All notable changes to `buffr-blink-cdp` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-05-15

### Changed

- `buffr-core` dep bumped from `"0.6"` to `"0.7"` to align with the v0.7.0
  breaking API split (CEF code moved to `buffr-cef`; engine-agnostic shell
  retained in `buffr-core`).

## [0.1.0] - 2026-05-14

### Added

- Initial extraction from the `kryptic-sh/buffr` umbrella into a standalone
  repository via `git subtree split`. Headless Chromium CDP backend stub
  implementing the `buffr-engine` `BrowserEngine` trait.

[Unreleased]:
  https://github.com/kryptic-sh/buffr-blink-cdp/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/kryptic-sh/buffr-blink-cdp/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/buffr-blink-cdp/releases/tag/v0.1.0
