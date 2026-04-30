// See apps/buffr/build.rs — same rationale. The CEF subprocess helper
// dlopen's libcef.so just like the main binary, so it needs RUNPATH
// to find libcef.so next to itself when launched from /opt/buffr/.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}
