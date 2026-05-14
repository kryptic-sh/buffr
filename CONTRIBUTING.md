# Contributing to `buffr-cef`

Thanks for considering a contribution. `buffr-cef` is pre-1.0 and the public API
is still in motion — please open an issue before starting any non-trivial PR so
the design can be sanity-checked early.

## Development setup

```bash
git clone git@github.com:kryptic-sh/buffr-cef.git
cd buffr-cef
rustup toolchain install stable
cargo test --all-features
```

## MSRV policy

`rust-version` in `Cargo.toml` tracks current stable Rust. Floor, not ceiling —
bumps land freely when new features are useful. Any bump must be logged in
`CHANGELOG.md` under the version that introduces it.

## Pull requests

- Branch from `main`. One logical change per PR.
- Commits: [Conventional Commits](https://www.conventionalcommits.org/) format.
  `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`, `ci`, `build`.
  Scope optional.
- Run before pushing:
  - `cargo fmt --all --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --all-features`
- New public API needs rustdoc and (where applicable) a `///` example.

## Releases

Patch bumps follow the BCTP flow (Bump → Commit → Tag → Push). The release
workflow's publish step is idempotent — re-running after a partial failure is
safe.

To **yank** a broken release:

```bash
cargo yank --version X.Y.Z
```

Yank ≠ delete: consumers pinned to `=X.Y.Z` still resolve. Document the reason
in `CHANGELOG.md` under a `### Yanked` heading for that version.

## Pre-1.0 stability

Pre-1.0, breaking changes may land on minor bumps per Cargo's SemVer rules for
`0.x` versions. Document each breaking change in `CHANGELOG.md`.
