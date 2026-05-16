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

    // ── libwpe (wpe-1.0) ───────────────────────────────────────────────────────
    let wpe_lib = pkg_config::probe_library("wpe-1.0")
        .expect("wpe-1.0 not found via pkg-config. Install libwpe-1.0-dev or libwpe.");

    // ── wpebackend-fdo-1.0 ────────────────────────────────────────────────────
    let fdo_lib = pkg_config::probe_library("wpebackend-fdo-1.0").expect(
        "wpebackend-fdo-1.0 not found. Install wpebackend-fdo or libwpebackend-fdo-1.0-dev.",
    );

    // ── wayland-server (for wl_shm_buffer pixel readback) ────────────────────
    let wayland_lib = pkg_config::probe_library("wayland-server")
        .expect("wayland-server not found. Install libwayland-dev.");

    // Collect -I flags for bindgen's clang args (deduplicated).
    let mut include_set: std::collections::HashSet<String> = webkit_lib
        .include_paths
        .iter()
        .chain(wpe_lib.include_paths.iter())
        .chain(fdo_lib.include_paths.iter())
        .chain(wayland_lib.include_paths.iter())
        .map(|p| format!("-I{}", p.display()))
        .collect();
    // Always expose wpe-1.0 headers so fdo can include <wpe/wpe.h>.
    include_set.insert("-I/usr/include/wpe-1.0".to_owned());
    let clang_args: Vec<String> = include_set.into_iter().collect();

    // ── Generate unified FFI bindings ──────────────────────────────────────────
    //
    // Single bindgen run covers all four libraries so cross-type references
    // (e.g. wpe_view_backend* used by both wpe-1.0 and wpebackend-fdo) resolve
    // without duplication.
    let bindings = bindgen::Builder::default()
        // wpe-webkit-2.0 umbrella header.
        .header("/usr/include/wpe-webkit-2.0/wpe/webkit.h")
        // libwpe umbrella header.
        .header("/usr/include/wpe-1.0/wpe/wpe.h")
        // wpebackend-fdo EGL umbrella (SHM + EGL exportable APIs).
        .header("/usr/include/wpe-fdo-1.0/wpe/fdo-egl.h")
        // wpebackend-fdo SHM+base exportable (non-EGL path used for CPU readback).
        .header("/usr/include/wpe-fdo-1.0/wpe/fdo.h")
        // wayland-server for wl_shm_buffer pixel access.
        .header("/usr/include/wayland-server-core.h")
        .clang_args(&clang_args)
        // Required guards to compile the fdo headers outside the library.
        .clang_arg("-DWPE_FDO_COMPILATION")
        .clang_arg("-DWL_HIDE_DEPRECATED")
        // Allowlists — keep compact.
        .allowlist_function("webkit_.*")
        .allowlist_type("WebKit.*")
        .allowlist_var("WEBKIT_.*")
        .allowlist_function("wpe_.*")
        .allowlist_type("wpe_.*")
        .allowlist_var("WPE_.*")
        .allowlist_function("wl_shm_buffer_.*")
        .allowlist_type("wl_shm_buffer")
        .allowlist_type("wl_resource")
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
    println!("cargo:rerun-if-changed=/usr/include/wpe-1.0/wpe/wpe.h");
    println!("cargo:rerun-if-changed=/usr/include/wpe-fdo-1.0/wpe/fdo-egl.h");
    println!("cargo:rerun-if-changed=/usr/include/wpe-fdo-1.0/wpe/fdo.h");
    println!("cargo:rerun-if-changed=/usr/include/wayland-server-core.h");
}
