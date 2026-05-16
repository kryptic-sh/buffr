fn main() {
    // Only generate FFI bindings on Linux where WPE WebKit is available.
    #[cfg(target_os = "linux")]
    {
        build_linux();
    }

    // Rerun when the header or this script changes.
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(target_os = "linux")]
fn build_linux() {
    // Locate wpe-webkit-2.0 via pkg-config.
    let lib = pkg_config::probe_library("wpe-webkit-2.0").expect(
        "wpe-webkit-2.0 not found via pkg-config. \
         Install libwpewebkit-2.0-dev (Debian/Ubuntu) or wpewebkit (Arch).",
    );

    // Collect -I flags for bindgen's clang args.
    let clang_args: Vec<String> = lib
        .include_paths
        .iter()
        .map(|p| format!("-I{}", p.display()))
        .collect();

    // Generate FFI bindings for the WPE WebKit public header.
    let bindings = bindgen::Builder::default()
        .header("/usr/include/wpe-webkit-2.0/wpe/webkit.h")
        .clang_args(&clang_args)
        // Keep bindings compact: only WebKit-namespaced symbols.
        .allowlist_function("webkit_.*")
        .allowlist_type("WebKit.*")
        .allowlist_var("WEBKIT_.*")
        // Suppress common C-binding warnings that don't affect correctness.
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed to generate WPE WebKit bindings");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    bindings
        .write_to_file(std::path::PathBuf::from(out_dir).join("wpe_bindings.rs"))
        .expect("failed to write wpe_bindings.rs");

    println!("cargo:rerun-if-changed=/usr/include/wpe-webkit-2.0/wpe/webkit.h");
}
