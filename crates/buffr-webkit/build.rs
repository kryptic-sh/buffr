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
    // ── WPE WebKit ─────────────────────────────────────────────────────────────
    let webkit_lib = pkg_config::probe_library("wpe-webkit-2.0").expect(
        "wpe-webkit-2.0 not found via pkg-config. \
         Install libwpewebkit-2.0-dev (Debian/Ubuntu) or wpewebkit (Arch).",
    );

    // ── wpe-platform-2.0 (the new WPEDisplay/WPEView API) ────────────────────
    //
    // Ships inside the wpewebkit package on Arch — separate .pc file. This is
    // the OSR seam we hook into: a custom WPEDisplay subclass that hands
    // WebKit our EGL display + a WPEView whose `render_buffer` vmethod copies
    // pixels into our shared OsrFrame.
    let platform_lib = pkg_config::probe_library("wpe-platform-2.0").expect(
        "wpe-platform-2.0 not found via pkg-config. Requires WPE WebKit ≥ 2.50 \
         with ENABLE_WPE_PLATFORM=ON.",
    );

    // Collect -I flags for bindgen's clang args (deduplicated).
    let include_set: std::collections::HashSet<String> = webkit_lib
        .include_paths
        .iter()
        .chain(platform_lib.include_paths.iter())
        .map(|p| format!("-I{}", p.display()))
        .collect();
    let clang_args: Vec<String> = include_set.into_iter().collect();

    // ── Generate unified FFI bindings ──────────────────────────────────────────
    //
    // One bindgen run covers wpe-webkit-2.0 and wpe-platform-2.0 so the
    // cross-type references (WebKitWebView's `display` property is a
    // WPEDisplay*) resolve without duplication.
    let bindings = bindgen::Builder::default()
        // wpe-webkit-2.0 umbrella.
        .header("/usr/include/wpe-webkit-2.0/wpe/webkit.h")
        // wpe-platform-2.0 umbrella — pulls in WPEDisplay/WPEView/WPEBuffer
        // and the rest of the platform surface.
        .header("/usr/include/wpe-webkit-2.0/wpe-platform/wpe/wpe-platform.h")
        .clang_args(&clang_args)
        // Allowlists — keep compact.
        .allowlist_function("webkit_.*")
        .allowlist_type("WebKit.*")
        .allowlist_var("WEBKIT_.*")
        .allowlist_function("wpe_.*")
        .allowlist_type("WPE.*")
        .allowlist_type("_WPE.*")
        .allowlist_type("wpe_.*")
        .allowlist_var("WPE_.*")
        // GObject helpers we'll need for raw type registration + property
        // construction.
        .allowlist_function("g_object_new")
        .allowlist_function("g_object_ref")
        .allowlist_function("g_object_unref")
        .allowlist_function("g_signal_connect_data")
        .allowlist_function("g_signal_handler_disconnect")
        .allowlist_function("g_type_register_static_simple")
        .allowlist_function("g_type_check_instance_cast")
        .allowlist_function("g_type_class_peek_parent")
        .allowlist_function("g_type_class_ref")
        .allowlist_function("g_type_class_unref")
        .allowlist_function("g_param_spec_object")
        .allowlist_function("g_bytes_.*")
        .allowlist_function("g_error_free")
        .allowlist_type("GTypeInfo")
        .allowlist_type("GTypeFlags")
        .allowlist_type("GError")
        .allowlist_type("GBytes")
        .allowlist_var("G_TYPE_OBJECT")
        // Disable layout assertions — we use the FFI types as opaque pointers
        // only. The glib crate provides real GLib types; our bindgen output just
        // needs the function signatures.
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed to generate WPE/WebKit FFI bindings");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    bindings
        .write_to_file(std::path::PathBuf::from(&out_dir).join("wpe_bindings.rs"))
        .expect("failed to write wpe_bindings.rs");

    println!("cargo:rerun-if-changed=/usr/include/wpe-webkit-2.0/wpe/webkit.h");
    println!("cargo:rerun-if-changed=/usr/include/wpe-webkit-2.0/wpe-platform/wpe/wpe-platform.h");

    // ── Compile the C bridge ────────────────────────────────────────────────
    //
    // wpe_subclasses.c defines BuffrDisplay/View/Toplevel/Screen as final
    // GObject subclasses using upstream's G_DEFINE_FINAL_TYPE machinery. We
    // do this in C because bindgen emits the *Class structs as opaque
    // (`_address: u8`) and rolling the layouts by hand in Rust is brittle.
    let mut build = cc::Build::new();
    build
        .file("csrc/wpe_subclasses.c")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-cast-function-type");
    for path in platform_lib
        .include_paths
        .iter()
        .chain(webkit_lib.include_paths.iter())
    {
        build.include(path);
    }
    build.compile("buffr_wpe_subclasses");

    println!("cargo:rerun-if-changed=csrc/wpe_subclasses.c");
}
