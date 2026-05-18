# buffr-core

CEF integration and browser host for buffr.

Part of the [buffr](https://github.com/kryptic-sh/buffr) ecosystem — a
vim-inspired, CEF-backed browser written in Rust. This crate is pulled into the
umbrella as a git submodule under `crates/buffr-core/`; consumers outside the
umbrella can depend on it directly:

```toml
[dependencies]
buffr-core = "0.1"
```

## Status

Pre-1.0. Public API may break on minor bumps until 1.0.0 ships. See
`CHANGELOG.md` for per-release notes.

## License

MIT — see [`LICENSE`](LICENSE).
