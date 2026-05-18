# Changelog

All notable changes to `buffr-permissions` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] — 2026-05-12

### Changed

- CI maintenance: collapsed two-stage CI (ci.yml + release.yml) into a single
  tag-driven `ci.yml`, added Dependabot config (cargo + github-actions, weekly),
  and renamed the workflow to PascalCase. No code changes.

## [0.1.1] — 2026-04-30

### Changed

- Extracted from the `kryptic-sh/buffr` umbrella into a standalone repository
  with full git history preserved via `git subtree split`.
- Added per-repo CI (fmt / clippy / test matrix / cargo-deny) and a tag-driven
  release workflow that publishes idempotently to crates.io.

[Unreleased]:
  https://github.com/kryptic-sh/buffr-permissions/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/kryptic-sh/buffr-permissions/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-permissions/releases/tag/v0.1.1
