//! Stub binary for `cargo install buffr`.
//!
//! buffr is a CEF-backed browser; the runnable binary needs ~150 MB of
//! libcef + paks + locales sitting next to the executable. `cargo install`
//! only copies the bare binary into `~/.cargo/bin`, so a cargo-installed
//! buffr can never load.
//!
//! Rather than fail at `exec` time with a confusing
//! `error while loading shared libraries: libcef.so` message, this stub
//! is what we publish to crates.io. It prints a pointer to the real
//! distribution channel and exits non-zero so scripts notice.
//!
//! The real browser is built from `apps/buffr/` (package `buffr-bin`)
//! and shipped as platform tarballs on GitHub releases.

use std::process::ExitCode;

const REPO: &str = env!("CARGO_PKG_REPOSITORY");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let releases_url = if REPO.is_empty() {
        "https://github.com/kryptic-sh/buffr/releases".to_string()
    } else {
        format!("{}/releases", REPO.trim_end_matches('/'))
    };

    eprintln!("buffr {VERSION}");
    eprintln!();
    eprintln!("`cargo install buffr` is not a supported install path.");
    eprintln!();
    eprintln!("buffr is a CEF-backed browser and requires a ~150 MB runtime");
    eprintln!("payload (libcef, paks, locales, sandbox) that cargo install");
    eprintln!("cannot bundle. Download a prebuilt release for your platform:");
    eprintln!();
    eprintln!("    {releases_url}");
    eprintln!();
    eprintln!("Or build from source with the CEF runtime alongside:");
    eprintln!("    git clone {REPO}");
    eprintln!("    cd buffr && cargo build --release");
    eprintln!("    ./target/release/buffr");

    ExitCode::from(1)
}
