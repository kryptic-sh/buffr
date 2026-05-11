# Changelog

All notable changes to `buffr-modal` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3] — 2026-05-12

### Changed

- CI maintenance: collapsed two-stage CI (ci.yml + release.yml) into a single
  tag-driven `ci.yml`, added Dependabot config (cargo + github-actions, weekly),
  and renamed the workflow to PascalCase. No code changes.

### Documentation

- Backfilled missing CHANGELOG entries for prior releases.

## [0.1.2] — 2026-04-30

### Changed

- `hjkl-engine` and `hjkl-buffer` deps relaxed from exact-pin to caret `0.1` so
  consumers pick up patch fixes without a buffr-modal re-publish.

## [0.1.1] — 2026-04-30

### Changed

- Extracted from the `kryptic-sh/buffr` umbrella into a standalone repository
  with full git history preserved via `git subtree split`.
- Added per-repo CI (fmt / clippy / test matrix / cargo-deny) and a tag-driven
  release workflow that publishes idempotently to crates.io.

[Unreleased]: https://github.com/kryptic-sh/buffr-modal/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/kryptic-sh/buffr-modal/releases/tag/v0.1.3
[0.1.2]: https://github.com/kryptic-sh/buffr-modal/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-modal/releases/tag/v0.1.1
