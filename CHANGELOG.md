# Changelog

All notable changes to `buffr-ui` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] — 2026-05-03

### Changed

- **Tab strip favicon scaling switched from nearest-neighbour to bilinear.**
  Smoother edges on HiDPI displays where the source 16×16 favicons were upscaled
  to 24×24+.

## [0.1.1] — 2026-04-30

### Changed

- Extracted from the `kryptic-sh/buffr` umbrella into a standalone repository
  with full git history preserved via `git subtree split`.
- Added per-repo CI (fmt / clippy / test matrix / cargo-deny) and a tag-driven
  release workflow that publishes idempotently to crates.io.

[Unreleased]: https://github.com/kryptic-sh/buffr-ui/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.1.1
