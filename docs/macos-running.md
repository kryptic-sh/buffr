# Running on macOS

This is the shortest path for running the development build from a fresh clone
on macOS.

## Prerequisites

- macOS 12 or newer.
- Xcode command-line tools:

```sh
xcode-select --install
```

- Rust from `rustup`. The repo pins the toolchain in `rust-toolchain.toml`, so
  Cargo installs the required Rust version on first use.
- CMake and Ninja, used by the CEF build wrapper:

```sh
brew install cmake ninja
```

## First run

From the workspace root:

```sh
cargo xtask fetch-cef
cargo run
```

`cargo xtask fetch-cef` downloads the host CEF binary distribution and extracts
it under `vendor/cef/macosarm64` on Apple Silicon or `vendor/cef/macosx64` on
Intel Macs. `vendor/cef/` is intentionally gitignored.

`cargo run` builds the default `buffr` binary, stages the CEF framework under
`target/Frameworks/`, stages the CEF GPU support dylibs next to
`target/debug/buffr`, and starts the browser. The macOS runtime uses CEF
off-screen rendering (OSR), so page content and buffr's tabbar/statusbar are
composited into the same `winit` window.

## Runtime paths

The normal dev run writes profile state to the standard macOS app directories:

```text
~/Library/Caches/sh.kryptic.buffr/
~/Library/Application Support/sh.kryptic.buffr/
```

That includes CEF cache data plus SQLite stores for history, bookmarks,
downloads, permissions, and zoom. Use `--private` for an in-memory/private data
session:

```sh
cargo run -- --private
```

## Useful commands

```sh
# More startup detail.
RUST_LOG=buffr=debug,buffr_core=debug cargo run

# Validate config without starting CEF.
cargo run -- --check-config

# Build the macOS app bundle under target/release/buffr.app.
cargo xtask bundle-macos --release
```

The `.app` bundle path is still the right shape for packaging and signing. The
plain `cargo run` path is for local development and uses explicit CEF settings
so the loose binary can find the staged framework, resources, and subprocess
path.
