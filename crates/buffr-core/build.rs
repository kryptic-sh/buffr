//! Build script for `buffr-core`.
//!
//! Resolves the CEF binary distribution location and:
//!
//! 1. Emits `cargo:rustc-link-search` so the linker finds `libcef`.
//! 2. Emits `cargo:rustc-link-lib` for the platform's CEF shared lib.
//! 3. Copies the runtime payload (`Resources/`, `*.pak`, `locales/`,
//!    and the shared library itself) next to the final target binaries
//!    so `cargo run -p buffr` works without `LD_LIBRARY_PATH` gymnastics.
//!
//! Resolution order for the CEF tree:
//!
//! 1. `CEF_PATH` env var (mirrors the upstream `cef-rs` convention).
//! 2. `<workspace>/vendor/cef/<platform>/` (populated by
//!    `cargo xtask fetch-cef`).
//!
//! If neither is present, we emit warnings and skip the copy step —
//! the actual link errors will then come from `cef-dll-sys` itself,
//! which downloads its own copy under `OUT_DIR`. This lets `cargo
//! check` succeed even on a fresh clone.

use std::{
    env,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CEF_PATH");
    if let Some(vendor_cef_path) = vendor_cef_path() {
        println!("cargo:rerun-if-changed={}", vendor_cef_path.display());
    }

    let cef_path = match resolve_cef_path() {
        Some(p) => p,
        None => {
            println!(
                "cargo:warning=CEF_PATH not set and vendor/cef/<platform> missing; \
                 falling back to cef-dll-sys defaults. Run `cargo xtask fetch-cef` to vendor."
            );
            return;
        }
    };

    // Informational only — cargo has no `cargo:info=` directive, so we
    // emit to stderr where it shows under `cargo build -vv` without
    // polluting normal builds with a warning.
    eprintln!("buffr-core: using CEF_PATH = {}", cef_path.display());

    // Spotify minimal distributions ship the shared library under
    // `Release/` on Linux/Windows and under
    // `Release/Chromium Embedded Framework.framework` on macOS.
    let release_dir = cef_path.join("Release");
    let resources_dir = cef_path.join("Resources");

    let lib_dir = if release_dir.exists() {
        release_dir.clone()
    } else {
        cef_path.clone()
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=dylib=cef");
        // Help loaders find libcef.so at runtime when the user runs
        // the binary out of `target/debug` directly.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=dylib=libcef");
    }
    #[cfg(target_os = "macos")]
    {
        // On macOS the framework path discipline is handled by
        // `cef-rs`'s library-loader at runtime; we only need to make
        // sure the framework is staged next to the binary.
    }

    if let Err(err) = stage_runtime(&lib_dir, &resources_dir) {
        println!("cargo:warning=failed to stage CEF runtime: {err}");
    }
}

fn resolve_cef_path() -> Option<PathBuf> {
    if let Ok(p) = env::var("CEF_PATH") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    let candidate = vendor_cef_path()?;
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

fn vendor_cef_path() -> Option<PathBuf> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok()?;
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();
    Some(workspace_root.join("vendor/cef").join(host_platform()))
}

fn host_platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macosarm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macosx64"
    } else if cfg!(target_os = "windows") {
        "windows64"
    } else {
        "unknown"
    }
}

/// Copy CEF resources (`Resources/`, `*.pak`, `locales/`, shared lib)
/// next to the final target binaries.
///
/// `OUT_DIR` is `target/<profile>/build/<crate>-<hash>/out`. Walking
/// `../../..` lands us at `target/<profile>/`, where Cargo deposits
/// the binaries from `apps/buffr` and `apps/buffr-helper`.
fn stage_runtime(lib_dir: &Path, resources_dir: &Path) -> io::Result<()> {
    let out_dir = env::var_os("OUT_DIR").map(PathBuf::from);
    let Some(out_dir) = out_dir else {
        return Ok(());
    };
    let target_dir = out_dir
        .ancestors()
        .nth(3)
        .ok_or_else(|| io::Error::other("OUT_DIR has no grand-grand-parent"))?
        .to_path_buf();

    if !target_dir.exists() {
        return Ok(());
    }

    // Shared library.
    #[cfg(target_os = "linux")]
    {
        let so = lib_dir.join("libcef.so");
        if so.exists() {
            copy_into_dir(&so, &target_dir)?;
        }
    }
    #[cfg(target_os = "windows")]
    {
        let dll = lib_dir.join("libcef.dll");
        if dll.exists() {
            copy_into_dir(&dll, &target_dir)?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let framework = lib_dir.join("Chromium Embedded Framework.framework");
        if framework.exists() {
            // `cef::library_loader::LibraryLoader::new(current_exe, false)`
            // resolves `../Frameworks/Chromium Embedded Framework.framework`
            // from `target/debug/buffr`, i.e. `target/Frameworks/...`.
            // Keep dev-tree staging aligned with the same relative layout as
            // `buffr.app/Contents/MacOS/../Frameworks`.
            let frameworks_dir = target_dir
                .parent()
                .map(|parent| parent.join("Frameworks"))
                .unwrap_or_else(|| target_dir.join("Frameworks"));
            let dest = frameworks_dir.join("Chromium Embedded Framework.framework");
            let _ = fs::remove_dir_all(&dest);
            copy_dir(&framework, &dest)?;

            let libraries_dir = framework.join("Libraries");
            if libraries_dir.exists() {
                for entry in fs::read_dir(libraries_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.ends_with(".dylib") || name.ends_with(".json") {
                            copy_into_dir(&path, &target_dir)?;
                        }
                    }
                }
            }
        }
    }

    // *.pak files + .dat (icudtl, snapshot blob) live next to the lib.
    if lib_dir.exists() {
        for entry in fs::read_dir(lib_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".pak") || name.ends_with(".dat") || name.ends_with(".bin") {
                    copy_into_dir(&path, &target_dir)?;
                }
            }
        }
    }

    // Resources/ tree (en-US.pak, locales/, etc.).
    if resources_dir.exists() {
        // Spotify ships .pak + locales/ at the top of Resources/.
        for entry in fs::read_dir(resources_dir)? {
            let entry = entry?;
            let path = entry.path();
            let dest = target_dir.join(entry.file_name());
            if path.is_dir() {
                let _ = fs::remove_dir_all(&dest);
                copy_dir(&path, &dest)?;
            } else {
                copy_into_dir(&path, &target_dir)?;
            }
        }
    }

    Ok(())
}

fn copy_into_dir(src: &Path, dest_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dest_dir)?;
    let dest = dest_dir.join(
        src.file_name()
            .ok_or_else(|| io::Error::other("copy_into_dir: src has no file name"))?,
    );
    fs::copy(src, dest)?;
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_dir(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        io::copy(&mut File::open(src)?, &mut File::create(dest)?)?;
        Ok(())
    }
}
