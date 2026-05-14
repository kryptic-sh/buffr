# Changelog

All notable changes to `buffr-engine` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-05-15

### Added

- `BackendOpenOptions.find_sink: Option<Arc<dyn Any + Send + Sync>>` field to
  thread the apps-layer find-result sink through the Backend trait path. Lets
  backend impls populate the same `FindResultSink` the apps layer drains,
  eliminating the broken "find_sink: None" path through
  `BlinkCdpBackend::open_engine`. Audit fix #P1-1.

## [0.1.0] - 2026-04-01

_Initial release._

[Unreleased]: https://github.com/kryptic-sh/buffr-engine/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/buffr-engine/releases/tag/v0.1.0
