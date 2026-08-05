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
cargo build
cargo run --bin buffr-app
```

`cargo xtask fetch-cef` downloads the host CEF binary distribution and extracts
it under `vendor/cef/macosarm64` on Apple Silicon or `vendor/cef/macosx64` on
Intel Macs. `vendor/cef/` is intentionally gitignored.

Bare `cargo run` does not work — the workspace has three binaries (`buffr`,
`buffr-app`, `buffr-helper`) and cargo cannot pick one.
`cargo run --bin buffr-app` runs the browser directly; `cargo run --bin buffr`
runs it under the crash-restart supervisor (build first so `buffr-app` sits next
to it). The build stages the CEF framework under `target/Frameworks/` and the
CEF GPU support dylibs next to the binary in `target/debug/`. The macOS runtime
uses CEF off-screen rendering (OSR), so page content and buffr's
tabbar/statusbar are composited into the same `winit` window.

## Runtime paths

buffr is XDG-everywhere, so the dev run writes profile state to the same
directories it uses on Linux (a debug build adds the `-debug` suffix):

```text
~/.local/share/buffr-debug/
```

Everything lives there: the SQLite stores (history, bookmarks, downloads,
permissions, zoom, favicons) **and** CEF's own profile tree — cookies,
`Local Storage`, and the HTTP cache — because `buffr-app` passes the data dir as
CEF's `root_cache_path`. `~/.cache/buffr-debug/` is created too, but CEF writes
nothing there. See [`config.md`](./config.md).

Use `--private` for an in-memory/private data session:

```sh
cargo run --bin buffr-app -- --private
```

## Useful commands

```sh
# More startup detail.
RUST_LOG=buffr_app=debug,buffr_core=debug cargo run --bin buffr-app

# Validate config without starting CEF.
cargo run --bin buffr-app -- --check-config

# Build the macOS app bundle under target/release/Buffr.app.
cargo xtask bundle-macos --release
```

The `.app` bundle path is still the right shape for packaging and signing. The
loose `cargo run --bin …` path is for local development and uses explicit CEF
settings so the loose binary can find the staged framework, resources, and
subprocess path.
