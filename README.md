# buffr

Vim-inspired browser. Native, GPU-accelerated. Rust + CEF.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Modal browser built on
[Chromium Embedded Framework](https://bitbucket.org/chromiumembedded/cef) via
the [`cef`](https://crates.io/crates/cef) Rust crate. Vim keybindings powered by
[hjkl-engine](https://crates.io/crates/hjkl-engine).

## Status

`0.1.0` — first tagged release. Multi-tab browsing; popup windows
(`window.open`, OAuth) render in dedicated buffr windows with read-only address
bars and preserve `window.opener`; `target="_blank"` and Ctrl+click open in
tabs; two-finger horizontal swipe navigates browser history; vim modal engine
(`hjkl 0.1.0`) wired for page-mode dispatch and insert-mode text editing;
history / downloads / bookmarks / permissions / zoom data layers wired and
persisted to SQLite. See [CHANGELOG.md](CHANGELOG.md).

## Supported platforms

Each release publishes binary artifacts for:

| OS      | Architecture          | Format                    |
| ------- | --------------------- | ------------------------- |
| Linux   | x86_64, aarch64       | `.deb`, `.rpm`, `.tar.gz` |
| macOS   | arm64 (Apple Silicon) | `.dmg`                    |
| Windows | x64, arm64            | `.msi` (per-user)         |

### Why no Intel Mac (`x86_64-apple-darwin`)?

We dropped Intel Mac builds in `0.1.14`. Apple stopped selling Intel Macs in
2023, and the GitHub Actions `macos-13` runner pool is heavily contended —
release tags routinely queued for 1–2 hours waiting on a slot, blocking the
entire publish pipeline (the crates.io stub publish gates on every binary leg).
The cost wasn't paying for the user count, so we cut it.

If you're on an Intel Mac and want to run buffr, build from source on your own
machine — the workspace builds clean against `x86_64-apple-darwin`, the support
is just absent from the release pipeline, not from the code.

## Apps

| Binary         | Role                                                               |
| -------------- | ------------------------------------------------------------------ |
| `buffr`        | Main browser binary. Owns the winit window, CEF lifecycle, keymap. |
| `buffr-helper` | CEF subprocess helper (renderer / GPU / utility processes).        |

## Crates

| Crate               | Role                                                             |
| ------------------- | ---------------------------------------------------------------- |
| `buffr-core`        | CEF integration, `BrowserHost`, multi-tab host, OSR, IPC.        |
| `buffr-modal`       | Vim page-mode FSM, keymap trie, `hjkl-engine` edit-mode bridge.  |
| `buffr-ui`          | Statusline, tab strip, input bar, permission / confirm prompts.  |
| `buffr-config`      | TOML config loader, validator, hot-reload watcher.               |
| `buffr-history`     | SQLite-backed browsing history (frecency search).                |
| `buffr-bookmarks`   | SQLite-backed bookmark store with tags + Netscape import.        |
| `buffr-downloads`   | SQLite-backed download tracking; CEF handler integration.        |
| `buffr-permissions` | SQLite-backed per-origin permission store (camera, mic, geo, …). |
| `buffr-zoom`        | SQLite-backed per-domain zoom-level persistence.                 |

Not yet published to crates.io — consume via path or git dep.

## Build

```bash
# Vendor the CEF binary distribution (~500 MB extracted).
cargo xtask fetch-cef

# Build the workspace.
cargo build

# Run (the workspace's default-members points at the real binary,
# so bare `cargo run` works; use `-p buffr-bin` if you want to be
# explicit).
cargo run
```

> **Heads-up:** `cargo install buffr` is **not** a supported install path. The
> `buffr` crate on crates.io is a stub that prints download instructions — CEF
> apps need a ~150 MB runtime payload (libcef, paks, locales, sandbox) that
> `cargo install` can't bundle. Grab a prebuilt release from
> [github.com/kryptic-sh/buffr/releases](https://github.com/kryptic-sh/buffr/releases),
> or build from source as shown above.

See [`docs/dev.md`](docs/dev.md) for full prerequisites, platform matrix, and
CEF path overrides.

## Known limitations

- **Image / multimedia clipboard paste is unsupported.** Ctrl+V text paste
  works, but pasting images or other non-text clipboard content into web pages
  is a no-op. This is a CEF off-screen-rendering (OSR) limitation, not specific
  to buffr's implementation. Tracked in
  [#19](https://github.com/kryptic-sh/buffr/issues/19).

## License

MIT. See [LICENSE](LICENSE).
