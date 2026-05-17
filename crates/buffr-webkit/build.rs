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

    // ── wpe-platform-wayland-2.0 (WPEDisplayWayland / WPEViewWayland) ────────
    //
    // Separate .pc on Arch. Provides the Wayland-specific WPEDisplay subclass
    // that renders into a real wl_surface instead of the OSR pixel-copy path.
    // Required for #144 (WPEDisplayWayland switch on Wayland sessions).
    let wayland_lib = pkg_config::probe_library("wpe-platform-wayland-2.0").expect(
        "wpe-platform-wayland-2.0 not found via pkg-config. \
         Required for WPEDisplayWayland (#144). \
         Install wpewebkit (Arch) or libwpewebkit-2.0-dev with Wayland support.",
    );

    // Collect -I flags for bindgen's clang args (deduplicated).
    let include_set: std::collections::HashSet<String> = webkit_lib
        .include_paths
        .iter()
        .chain(platform_lib.include_paths.iter())
        .chain(wayland_lib.include_paths.iter())
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
        // wpe-wayland umbrella — must come after wpe-platform.h since the
        // Wayland subclasses inherit from the platform base types.
        // Provides WPEDisplayWayland, WPEViewWayland, WPEToplevelWayland, etc.
        .header("/usr/include/wpe-webkit-2.0/wpe-platform/wpe/wayland/wpe-wayland.h")
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
    println!(
        "cargo:rerun-if-changed=/usr/include/wpe-webkit-2.0/wpe-platform/wpe/wayland/wpe-wayland.h"
    );

    // ── Generate zwp_linux_dmabuf_v1 C bindings via wayland-scanner ────────────
    //
    // BuffrViewWayland's render_buffer vmethod wraps DMA-BUF fds into a
    // wl_buffer using zwp_linux_dmabuf_v1. We generate both the client-header
    // and the private-code C source from the stable protocol XML, then compile
    // the generated code as a separate archive.
    //
    // This must happen BEFORE compiling wpe_subclasses.c so the generated
    // header is available (included by BuffrViewWayland code in that file).
    let xml_candidates = [
        "/usr/share/wayland-protocols/stable/linux-dmabuf/linux-dmabuf-v1.xml",
        "/usr/share/wayland-protocols/unstable/linux-dmabuf/linux-dmabuf-unstable-v1.xml",
    ];
    let dmabuf_xml = xml_candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .copied()
        .expect(
            "linux-dmabuf protocol XML not found. \
             Install wayland-protocols (Arch: pacman -S wayland-protocols).",
        );

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dmabuf_hdr = std::path::PathBuf::from(&out_dir).join("linux-dmabuf-client.h");
    let dmabuf_src = std::path::PathBuf::from(&out_dir).join("linux-dmabuf-client.c");

    let scanner = std::process::Command::new("wayland-scanner")
        .args(["client-header", dmabuf_xml, dmabuf_hdr.to_str().unwrap()])
        .status()
        .expect("wayland-scanner not found — install wayland (Arch: pacman -S wayland)");
    assert!(scanner.success(), "wayland-scanner client-header failed");

    let scanner2 = std::process::Command::new("wayland-scanner")
        .args(["private-code", dmabuf_xml, dmabuf_src.to_str().unwrap()])
        .status()
        .expect("wayland-scanner failed to run for private-code generation");
    assert!(scanner2.success(), "wayland-scanner private-code failed");

    println!("cargo:rerun-if-changed={dmabuf_xml}");

    // Compile linux-dmabuf generated C source.
    let mut dmabuf_build = cc::Build::new();
    dmabuf_build
        .file(&dmabuf_src)
        .flag("-Wno-unused-parameter")
        .flag("-Wno-cast-function-type")
        .include("/usr/include");
    for path in platform_lib
        .include_paths
        .iter()
        .chain(webkit_lib.include_paths.iter())
        .chain(wayland_lib.include_paths.iter())
    {
        dmabuf_build.include(path);
    }
    dmabuf_build.compile("buffr_wpe_dmabuf_protocol");

    // ── Compile the C bridge ────────────────────────────────────────────────
    //
    // wpe_subclasses.c defines BuffrDisplay/View/Toplevel/Screen as final
    // GObject subclasses using upstream's G_DEFINE_FINAL_TYPE machinery. We
    // do this in C because bindgen emits the *Class structs as opaque
    // (`_address: u8`) and rolling the layouts by hand in Rust is brittle.
    // ── EGL (for BuffrDisplayWayland — eglGetPlatformDisplay, eglInitialize) ───
    //
    // BuffrDisplayWayland (#152) calls eglGetPlatformDisplay / eglGetDisplay /
    // eglInitialize in buffr_display_wayland_new. We link directly against
    // libEGL here so the C file resolves those symbols at link time.
    println!("cargo:rustc-link-lib=EGL");

    // Link wayland-client for wl_surface_*, wl_subsurface_*, wl_buffer_* calls.
    println!("cargo:rustc-link-lib=wayland-client");

    let mut build = cc::Build::new();
    build
        .file("csrc/wpe_subclasses.c")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-cast-function-type")
        // wpe_subclasses.c includes the generated linux-dmabuf-client.h from OUT_DIR.
        .include(&out_dir);
    for path in platform_lib
        .include_paths
        .iter()
        .chain(webkit_lib.include_paths.iter())
        .chain(wayland_lib.include_paths.iter())
    {
        build.include(path);
    }
    build.compile("buffr_wpe_subclasses");

    println!("cargo:rerun-if-changed=csrc/wpe_subclasses.c");
}
