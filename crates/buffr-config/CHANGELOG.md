# Changelog

All notable changes to `buffr-config` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.2] - 2026-05-15

### Fixed

- **Security:** `resolve_input` now treats `javascript:` and `data:` schemes as
  search queries instead of navigable URLs, closing an XSS vector via the
  omnibar. `is_real_scheme` allow-list updated; 5 regression tests added. Audit
  finding #1.

## [0.4.1] - 2026-05-15

### Added

- `Engines` config section (`[engines]`): `default` engine id (defaults to
  `"cef"`) and `[[engines.rules]]` with `match` (host glob) and `engine` fields.
  Validation: non-empty `default` + non-empty pattern/engine per rule.
- `EngineInstance` struct (`id`, `backend`, optional `data_dir`) and
  `[[engines.instances]]` array. When `instances` is empty a single
  `{ id: "cef", backend: "cef" }` instance is synthesised via
  `effective_instances()` so existing configs need no changes.
- `action_to_string` exhaustive match extended to cover `PageAction::Engine(id)`
  so keybinding round-trip serialization compiles cleanly.
- Round-trip and validation tests for engines TOML: synthesis, default
  reference, rule reference, duplicate-id rejection, and field names.

## [0.4.0] — 2026-05-12

### Added

- `SearchEngine::prefix` (optional). Omnibar input of `<prefix> <query>`
  resolves through that engine instead of `default_engine` (`g rust closures` →
  google, `ddg foo` → duckduckgo). Bare prefix words with no query fall through
  to the default engine. `Config::validate` now scans the engine table for empty
  / duplicate prefixes. Closes kryptic-sh/buffr#47.

### Changed

- CI maintenance: collapsed two-stage CI (ci.yml + release.yml) into a single
  tag-driven `ci.yml`, added Dependabot config (cargo + github-actions, weekly),
  and renamed the workflow to PascalCase.

## [0.3.0] — 2026-05-04

### Added

- `IdleInhibitConfig` section (`[idle_inhibit]`) for issue #22 — controls the
  platform idle-inhibitor (prevent screen lock while video plays). Fields:
  `enabled` (default `true`), `inhibit_audio_only` (default `false`),
  `require_focus` (default `true`).

## [0.2.1] — 2026-05-04

### Changed

- `APPLICATION` constant now resolves to `"buffr-debug"` in debug builds via
  `cfg(debug_assertions)`, so dev runs don't share cache/data directories with
  release installs.

## [0.2.0] — 2026-05-03

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

[Unreleased]: https://github.com/kryptic-sh/buffr-config/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.4.2
[0.4.1]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.4.1
[0.4.0]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.3.0
[0.2.1]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.2.0

## [0.1.1] — 2026-04-30

### Changed

- Extracted from the `kryptic-sh/buffr` umbrella into a standalone repository
  with full git history preserved via `git subtree split`.
- Added per-repo CI (fmt / clippy / test matrix / cargo-deny) and a tag-driven
  release workflow that publishes idempotently to crates.io.

[0.1.1]: https://github.com/kryptic-sh/buffr-config/releases/tag/v0.1.1
