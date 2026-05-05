// `cargo:rustc-link-arg` only takes effect for the package that owns
// the build script, so the matching call in `crates/buffr-core/build.rs`
// (which is a lib crate) goes nowhere as far as this binary's link line
// is concerned. We mirror it here so that `target/release/buffr` carries
// `RUNPATH=$ORIGIN`, letting `libcef.so` resolve relative to the binary
// regardless of `LD_LIBRARY_PATH`. Without this, packaged installs at
// `/opt/buffr/buffr` (deb/rpm/AUR) fail at startup with
// "libcef.so: cannot open shared object file".
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}
