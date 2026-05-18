# Changelog

All notable changes to `buffr-view-source` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-12

### Added

- Initial release. `render(url: &str, source: &[u8]) -> String` returns
  a `<pre><code>`-wrapped, syntax-highlighted HTML fragment for the source
  bytes, with the language detected from `url`'s extension via the
  embedded `hjkl-bonsai` grammar registry. Falls back to a plain
  HTML-escaped `<pre>` when no grammar matches or any rendering step
  fails.
- Extracted from the `kryptic-sh/buffr` umbrella into a standalone
  repository with full git history preserved via `git subtree split`.

[Unreleased]: https://github.com/kryptic-sh/buffr-view-source/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kryptic-sh/buffr-view-source/releases/tag/v0.1.0
