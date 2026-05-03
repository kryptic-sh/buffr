# Changelog

All notable changes to `buffr-config` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Path resolution migrated to `hjkl-config` 0.2 (XDG-everywhere).**
  `default_config_path()` now delegates to
  `hjkl_config::config_path::<Config>()` instead of `directories::ProjectDirs`.
  Same `~/.config/buffr/config.toml` on Linux, macOS, and Windows;
  `$XDG_CONFIG_HOME` is honored on every platform.
- **macOS/Windows path migration (one-time).** macOS users move from
  `~/Library/Application Support/buffr/` to `~/.config/buffr/`. Windows users
  move from `%APPDATA%\buffr\` to `~/.config/buffr/`. Linux paths are unchanged.
- `Config` impls `hjkl_config::AppConfig` (`APPLICATION = "buffr"`).
  `ConfigError`, `validate()`, watcher, and `locate()` stay buffr-local (the
  `Validate` variant has no equivalent in hjkl-config's flatter error enum).
- Replaced `directories` dep with `dirs` (transitive via hjkl-config anyway).
  `resolve_default_dir()` now uses `dirs::download_dir()` instead of
  `directories::UserDirs::download_dir()` — same XDG resolution, fewer dep
  variants in the lock.

[Unreleased]: https://github.com/kryptic-sh/buffr-config/compare/v0.1.1...HEAD

## [0.1.1] — 2026-04-30

### Changed

- Extracted from the `kryptic-sh/buffr` umbrella into a standalone repository
  with full git history preserved via `git subtree split`.
- Added per-repo CI (fmt / clippy / test matrix / cargo-deny) and a tag-driven
  release workflow that publishes idempotently to crates.io.

[0.1.1]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.1.1
