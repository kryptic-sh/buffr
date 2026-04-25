# buffr

Vim-inspired browser. Native, GPU-accelerated. Rust + CEF.

## Status

Early scaffold. Not usable yet.

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
cargo build
```

CEF binary distribution is not yet wired in — full build will require fetching
`libcef` and resources matching the `cef` crate version.

## License

MIT. See [LICENSE](LICENSE).
