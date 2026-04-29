# buffr (stub)

This is the crates.io entry for [buffr](https://github.com/kryptic-sh/buffr) —
a vim-inspired, CEF-backed browser written in Rust.

**`cargo install buffr` is not supported.** The crate you'd get from this
command is just a stub that prints these install instructions and exits.

## Why

buffr requires the Chromium Embedded Framework (CEF) runtime — `libcef.so`
plus ~150 MB of paks, locales, and a sandbox helper — to live next to the
executable. `cargo install` copies only the bare binary into `~/.cargo/bin`,
so a cargo-installed buffr fails at startup with
`error while loading shared libraries: libcef.so`.

Industry standard for native apps with binary runtimes (Zed, Servo, Tauri
apps) is to skip crates.io as an install path and ship binaries directly.

## How to install

Download a prebuilt release for your platform:

<https://github.com/kryptic-sh/buffr/releases>

Or build from source — `cargo build --release` puts the CEF runtime next
to the binary in `target/release/`:

```bash
git clone https://github.com/kryptic-sh/buffr
cd buffr
cargo build --release
./target/release/buffr
```
