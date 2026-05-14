# Changelog

All notable changes to `buffr-ui` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.2] - 2026-05-15

### Added

- `TabView::engine_badge: Option<u32>` field — when `Some`, the tab strip
  renders a badge column at the left edge of each unpinned tab showing a
  2-character uppercase engine label (e.g. `"BL"` for blink-cdp, `"WK"` for
  webkit). Width adapts to the active font via `font::text_width`. Single-engine
  configs pass `None`; no visual change.
- Hover outline: when `TabView::hovered` is `true`, a 1-px white outline is
  drawn around the badge rectangle.
- `Statusline::engine_hint: Option<String>` — rendered on the right side as
  `"engine: <id>"` when populated by the app on tab hover.

## [0.2.1] — 2026-05-12

### Changed

- CI maintenance: collapsed two-stage CI (ci.yml + release.yml) into a single
  tag-driven `ci.yml`, added Dependabot config (cargo + github-actions, weekly),
  and renamed the workflow to PascalCase. No code changes.

## [0.2.0] — 2026-05-05

### Added

- **`context_menu` module: `ContextMenuOverlay` widget.** Floating panel
  rendered at click coords (clamped to viewport) with per-row highlight,
  separator hairlines, and disabled-item dimming. Used by `apps/buffr` to render
  the right-click menu surfaced by `buffr-core`'s `ContextMenuHandler`.
- **`ContextMenuOverlay` hit-test helpers: `panel_rect`, `contains`, `row_at`.**
  Mirror the clamp logic in `paint`, so callers can hit-test the same pixels
  that render. `row_at` returns `None` only for separators / outside the panel —
  disabled rows still resolve so callers can highlight them on hover; gate
  activation at the call site.

### Changed

- **Disabled menu rows keep their dimmed text colour even when highlighted.**
  Hover now flips the row background to the selected colour for visual
  continuity, but the text stays `FG_DISABLED` to signal "not interactive".

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

[Unreleased]: https://github.com/kryptic-sh/buffr-ui/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.2.2
[0.2.1]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.2.0
[0.1.2]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr-ui/releases/tag/v0.1.1
