# buffr

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Vim-inspired browser. Native, GPU-accelerated. Rust + CEF.

## Status

Early scaffold. Modal engine wired on hjkl 0.1.0; CEF integration in progress.

## Goals

- Modal keybindings (normal / insert / visual / command / hint).
- Native window, GPU-accelerated compositing for snappy feel and good battery
  life.
- Cross-platform: Linux, macOS, Windows.
- Built on Chromium Embedded Framework via the
  [`cef`](https://crates.io/crates/cef) Rust crate.

## Layout

```
buffr/
├── apps/
│   ├── buffr/         # main binary
│   └── buffr-helper/  # CEF subprocess helper
├── crates/
│   ├── buffr-core/    # CEF integration + browser host
│   ├── buffr-modal/   # vim-style mode + keybind engine
│   ├── buffr-ui/      # chrome, command palette, hint overlay
│   └── buffr-config/  # config loading (TOML)
└── Cargo.toml         # workspace root
```

## Build

```bash
cargo xtask fetch-cef   # vendor CEF (~500 MB extracted)
cargo build
cargo run -p buffr
```

See [`docs/dev.md`](docs/dev.md) for full prerequisites, the platform matrix,
and where things live.

## License

MIT. See [LICENSE](LICENSE).
