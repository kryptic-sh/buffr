//! `cargo xtask` — build automation tasks for buffr.
//!
//! Subcommands:
//!
//! - `fetch-cef [--platform <linux64|macosarm64|macosx64|windows64>] [--version <X.Y.Z>]`
//!   downloads the CEF Spotify minimal binary distribution matching the `cef`
//!   crate version (147 by default) and extracts it into
//!   `vendor/cef/<platform>/`.
//! - `bundle-macos [--release] [--target <triple>]` assembles a macOS `.app`
//!   bundle (with a nested `buffr Helper.app`) under `target/<profile>/`. Runs
//!   on Linux too; the actual runtime needs macOS, but bundle assembly is
//!   purely filesystem work and is exercised by CI on a Linux runner.
//! - `package-linux [--release] [--variant {appimage,deb,aur,all}]` produces
//!   Linux distribution artifacts under `target/dist/linux/`. Cross-builds
//!   from any Linux dev box; `appimagetool` and `dpkg-deb` are auto-detected
//!   and gracefully degraded if absent.
//! - `package-macos-dmg [--release]` wraps the bundle from `bundle-macos`
//!   into a `.dmg` under `target/dist/macos/`. Requires `hdiutil` (macOS) or
//!   `genisoimage` (Linux fallback); falls through to a staging tree if
//!   neither tool is available.
//! - `package-windows-msi [--release]` produces a `.msi` installer (and / or
//!   the staging payload + WiX source) under `target/dist/windows/`.
//!   `candle.exe` + `light.exe` from the WiX 3 toolset are auto-detected;
//!   absent tools leave the payload + `buffr.wxs` for a Windows runner to
//!   pick up.
//!
//! Run from the workspace root: `cargo xtask fetch-cef`.

use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

const DEFAULT_CDN: &str = "https://cef-builds.spotifycdn.com";
/// CEF major version we pin against the `cef` crate (147.x).
///
/// Spotify CDN entries look like `cef_binary_147.0.10+gXXXXX+chromium-...`;
/// we pick the newest entry whose version starts with this prefix.
const CEF_VERSION_PREFIX: &str = "147.";

/// Embedded `Info.plist` template for the main `buffr.app` bundle.
const MAIN_PLIST_TEMPLATE: &str = include_str!("../templates/main.plist");
/// Embedded `Info.plist` template for the nested `buffr Helper.app` bundle
/// (catch-all / unbranded helper used by cef-rs 147 today).
const HELPER_PLIST_TEMPLATE: &str = include_str!("../templates/helper.plist");
/// Per-flavor helper plists. Apple's signing model wants each subprocess
/// type in its own `.app` bundle so entitlements can differ per flavor;
/// cef-rs 147 only resolves a single `browser_subprocess_path` so we ship
/// the four bundles but every executable points back at the same
/// `buffr-helper` binary (renamed per Apple's distinct-executable rule).
const HELPER_GPU_PLIST_TEMPLATE: &str = include_str!("../templates/helper-gpu.plist");
const HELPER_RENDERER_PLIST_TEMPLATE: &str = include_str!("../templates/helper-renderer.plist");
const HELPER_PLUGIN_PLIST_TEMPLATE: &str = include_str!("../templates/helper-plugin.plist");

/// Embedded WiX 3 source for the Windows MSI installer. Substituted at
/// runtime via `str::replace`.
const WIX_TEMPLATE: &str = include_str!("../templates/buffr.wxs");

/// Bundle identifiers + display name used by the macOS bundle templates.
const NAME: &str = "buffr";
const BUNDLE_ID_MAIN: &str = "sh.kryptic.buffr";
const BUNDLE_ID_HELPER: &str = "sh.kryptic.buffr.helper";
const COPYRIGHT: &str = "MIT — kryptic.sh";

/// Env var override for the macOS CEF framework directory.
///
/// Bundle scripts (and CI on Linux) may not have a real macOS CEF tarball
/// available; pointing this at any directory lets `bundle-macos` finish the
/// assembly step end-to-end so we can catch script regressions per-PR.
const FRAMEWORK_OVERRIDE_ENV: &str = "BUFFR_BUNDLE_FRAMEWORK_DIR";

#[derive(Debug, Deserialize)]
struct CefIndex {
    macosarm64: CefPlatform,
    macosx64: CefPlatform,
    windows64: CefPlatform,
    linux64: CefPlatform,
    #[serde(default)]
    #[allow(dead_code)]
    linuxarm64: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CefPlatform {
    versions: Vec<CefVersion>,
}

#[derive(Debug, Deserialize)]
struct CefVersion {
    cef_version: String,
    #[serde(default)]
    channel: String,
    files: Vec<CefFile>,
}

#[derive(Debug, Deserialize)]
struct CefFile {
    #[serde(rename = "type")]
    file_type: String,
    name: String,
    #[allow(dead_code)]
    sha1: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args
        .next()
        .context("missing subcommand (try `fetch-cef`)")?;
    match cmd.as_str() {
        "fetch-cef" => fetch_cef(args.collect()),
        "bundle-macos" => bundle_macos(args.collect()),
        "package-linux" => package_linux(args.collect()),
        "package-macos-dmg" => package_macos_dmg(args.collect()),
        "package-windows-msi" => package_windows_msi(args.collect()),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => bail!("unknown subcommand `{other}` (try `fetch-cef`)"),
    }
}

fn print_help() {
    println!("buffr xtask");
    println!();
    println!("USAGE:");
    println!("    cargo xtask <COMMAND>");
    println!();
    println!("COMMANDS:");
    println!("    fetch-cef [--platform|--target PLATFORM] [--version PREFIX]");
    println!("        Download + extract CEF minimal binary distribution.");
    println!("        PLATFORM: linux64 (default on Linux), macosarm64, macosx64, windows64.");
    println!("        PREFIX:   version prefix to match (default: {CEF_VERSION_PREFIX}).");
    println!();
    println!("    bundle-macos [--release] [--target TRIPLE]");
    println!("        Assemble buffr.app (with nested buffr Helper.app) under");
    println!("        target/<profile>/. Runs on Linux too (cross-bundle).");
    println!();
    println!("    package-linux [--release] [--variant VARIANT]");
    println!("        Produce Linux distribution artifacts under target/dist/linux/.");
    println!("        VARIANT: appimage | deb | aur | all (default: all).");
    println!();
    println!("    package-macos-dmg [--release]");
    println!(
        "        Wrap target/<profile>/buffr.app into target/dist/macos/buffr-<ver>-<arch>.dmg."
    );
    println!("        Requires hdiutil (macOS) or genisoimage (Linux fallback).");
    println!();
    println!("    package-windows-msi [--release]");
    println!("        Stage Windows payload + WiX source under target/dist/windows/.");
    println!("        Builds the .msi if candle/light from the WiX 3 toolset are on PATH.");
}

fn fetch_cef(args: Vec<String>) -> Result<()> {
    let mut platform: Option<String> = None;
    let mut version_prefix = CEF_VERSION_PREFIX.to_string();

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // `--target` is the same idea as `--platform` — kept as an
            // alias so cross-host CI jobs (`--target windows64` /
            // `--target macosarm64`) read naturally without conflicting
            // with the historical `--platform` flag.
            "--platform" | "--target" => {
                platform = Some(
                    iter.next()
                        .context("--platform/--target requires a value")?,
                );
            }
            "--version" => {
                version_prefix = iter.next().context("--version requires a value")?;
            }
            other => bail!("unknown fetch-cef arg `{other}`"),
        }
    }

    let platform = platform.unwrap_or_else(|| host_platform().to_string());
    let workspace_root = workspace_root()?;
    let vendor_dir = workspace_root.join("vendor/cef").join(&platform);

    eprintln!("xtask: target platform = {platform}");
    eprintln!("xtask: vendor dir      = {}", vendor_dir.display());

    if vendor_dir.join("Release").exists() || vendor_dir.join("libcef.so").exists() {
        eprintln!("xtask: vendor dir already populated; skipping download");
        eprintln!("       (delete {} to re-fetch)", vendor_dir.display());
        return Ok(());
    }

    let index_url = format!("{DEFAULT_CDN}/index.json");
    eprintln!("xtask: fetching index from {index_url}");
    let index: CefIndex = ureq::get(&index_url)
        .call()
        .context("fetching CEF index.json")?
        .body_mut()
        .read_json()
        .context("parsing CEF index.json")?;

    let plat = match platform.as_str() {
        "linux64" => &index.linux64,
        "macosarm64" => &index.macosarm64,
        "macosx64" => &index.macosx64,
        "windows64" => &index.windows64,
        other => bail!("unsupported platform `{other}`"),
    };

    let version = plat
        .versions
        .iter()
        .filter(|v| v.cef_version.starts_with(&version_prefix))
        .find(|v| v.channel.eq_ignore_ascii_case("stable"))
        .or_else(|| {
            plat.versions
                .iter()
                .find(|v| v.cef_version.starts_with(&version_prefix))
        })
        .ok_or_else(|| {
            anyhow!("no CEF version matching prefix `{version_prefix}` for platform `{platform}`")
        })?;

    let file = version
        .files
        .iter()
        .find(|f| f.file_type == "minimal")
        .ok_or_else(|| anyhow!("no minimal distribution for {}", version.cef_version))?;

    eprintln!(
        "xtask: matched cef {} ({}); minimal file {}",
        version.cef_version, version.channel, file.name
    );

    fs::create_dir_all(&vendor_dir)
        .with_context(|| format!("creating {}", vendor_dir.display()))?;

    let archive_url = format!("{DEFAULT_CDN}/{}", file.name);
    let archive_path = vendor_dir.join(&file.name);
    download(&archive_url, &archive_path)?;

    eprintln!(
        "xtask: extracting {} -> {}",
        file.name,
        vendor_dir.display()
    );
    extract_tar_bz2(&archive_path, &vendor_dir)
        .with_context(|| format!("extracting {}", file.name))?;

    flatten_top_level(&vendor_dir)
        .with_context(|| format!("flattening {}", vendor_dir.display()))?;

    eprintln!("xtask: done. CEF extracted at {}", vendor_dir.display());
    eprintln!("       set CEF_PATH={} to override", vendor_dir.display());
    Ok(())
}

/// Build the CDN URL for a `(platform, cef_version)` pair.
///
/// Spotify ships minimal builds at
/// `<cdn>/cef_binary_<version>_<platform>_minimal.tar.bz2`. The full
/// filename is normally read out of `index.json`; we expose the
/// generator separately so unit tests can lock the URL pattern down
/// without hitting the network.
#[cfg_attr(not(test), allow(dead_code))]
fn cef_minimal_url(cdn: &str, platform: &str, cef_version: &str) -> String {
    format!("{cdn}/cef_binary_{cef_version}_{platform}_minimal.tar.bz2")
}

fn host_platform() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "macosarm64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "macosx64"
    }
    #[cfg(target_os = "windows")]
    {
        "windows64"
    }
}

fn workspace_root() -> Result<PathBuf> {
    // CARGO_MANIFEST_DIR for xtask is .../buffr/xtask. Workspace root is parent.
    let manifest = env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?;
    let path = PathBuf::from(manifest);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("xtask manifest has no parent"))?;
    Ok(parent.to_path_buf())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    eprintln!("xtask: downloading {url}");
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = resp.into_body().into_reader();
    let mut file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        total += n as u64;
        if total.is_multiple_of(8 * 1024 * 1024) {
            eprintln!("       {} MiB", total / (1024 * 1024));
        }
    }
    file.flush()?;
    eprintln!(
        "xtask: downloaded {} ({:.1} MiB)",
        dest.display(),
        total as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

fn extract_tar_bz2(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let bz2 = bzip2::read::BzDecoder::new(file);
    let mut tar = tar::Archive::new(bz2);
    tar.unpack(dest)?;
    Ok(())
}

/// Spotify archives contain a single top-level `cef_binary_<ver>_<plat>/`
/// directory. Move its contents up one level so consumers can look at
/// `vendor/cef/<plat>/Release` directly.
fn flatten_top_level(dir: &Path) -> Result<()> {
    let mut top: Option<PathBuf> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with("cef_binary_")
        {
            if top.is_some() {
                // Multiple matches — bail rather than guess.
                return Ok(());
            }
            top = Some(path);
        }
    }
    let Some(top) = top else {
        return Ok(());
    };
    for entry in fs::read_dir(&top)? {
        let entry = entry?;
        let from = entry.path();
        let to = dir.join(entry.file_name());
        if to.exists() {
            continue;
        }
        fs::rename(&from, &to).or_else(|_| {
            copy_dir_recursive(&from, &to).and_then(|_| Ok(fs::remove_dir_all(&from)?))
        })?;
    }
    let _ = fs::remove_dir_all(&top);
    Ok(())
}

/// Copy a single file into `dest_dir`, preserving the file name.
fn copy_into_dir(src: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir).with_context(|| format!("creating {}", dest_dir.display()))?;
    let name = src
        .file_name()
        .ok_or_else(|| anyhow!("copy_into_dir: src `{}` has no file name", src.display()))?;
    let dest = dest_dir.join(name);
    fs::copy(src, &dest)
        .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_dir_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        io::copy(&mut File::open(from)?, &mut File::create(to)?)?;
    }
    Ok(())
}

// ----------------------------- bundle-macos ------------------------------

/// Args for `cargo xtask bundle-macos`.
#[derive(Debug, Default)]
struct BundleArgs {
    release: bool,
    target: Option<String>,
    /// Which `vendor/cef/<platform>/` to draw the framework from. If
    /// unset we default to `macosarm64` (the most common Apple target).
    platform: Option<String>,
}

fn bundle_macos(args: Vec<String>) -> Result<()> {
    let mut parsed = BundleArgs::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--release" => parsed.release = true,
            "--target" => {
                parsed.target = Some(iter.next().context("--target requires a value")?);
            }
            "--platform" => {
                parsed.platform = Some(iter.next().context("--platform requires a value")?);
            }
            other => bail!("unknown bundle-macos arg `{other}`"),
        }
    }

    let workspace = workspace_root()?;
    let profile = if parsed.release { "release" } else { "debug" };

    // 1. Build the binaries.
    eprintln!("xtask: building buffr + buffr-helper ({profile})");
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&workspace)
        .arg("build")
        .arg("-p")
        .arg("buffr")
        .arg("-p")
        .arg("buffr-helper");
    if parsed.release {
        cmd.arg("--release");
    }
    if let Some(t) = parsed.target.as_deref() {
        cmd.arg("--target").arg(t);
    }
    let status = cmd.status().context("spawning cargo build")?;
    if !status.success() {
        bail!("cargo build failed (status {status:?})");
    }

    // Resolve cargo's per-target output dir.
    let target_dir = match parsed.target.as_deref() {
        Some(t) => workspace.join("target").join(t).join(profile),
        None => workspace.join("target").join(profile),
    };

    let buffr_bin = target_dir.join("buffr");
    let helper_bin = target_dir.join("buffr-helper");
    if !buffr_bin.exists() {
        bail!("expected `{}` after build", buffr_bin.display());
    }
    if !helper_bin.exists() {
        bail!("expected `{}` after build", helper_bin.display());
    }

    // 2. Resolve framework dir.
    let framework_dir = resolve_framework_dir(&workspace, parsed.platform.as_deref())?;

    // 3. Stage bundle (idempotent — wipe + rebuild).
    let app_dir = target_dir.join("buffr.app");
    if app_dir.exists() {
        fs::remove_dir_all(&app_dir)
            .with_context(|| format!("removing existing {}", app_dir.display()))?;
    }

    let version = workspace_version(&workspace)?;
    stage_bundle(
        &app_dir,
        &buffr_bin,
        &helper_bin,
        &framework_dir,
        version.as_str(),
    )?;

    eprintln!();
    eprintln!("xtask: buffr.app staged at {}", app_dir.display());
    eprintln!("xtask: For ad-hoc local signing:");
    eprintln!(
        "           codesign --force --deep --sign - {}",
        app_dir.display()
    );
    eprintln!("xtask: For distribution: see docs/macos-signing.md (TODO)");
    Ok(())
}

/// Pick the macOS CEF framework path:
///
/// 1. `BUFFR_BUNDLE_FRAMEWORK_DIR` env override (CI uses this with a
///    stub directory so bundle-script regressions get caught on a
///    Linux runner without a real macOS CEF tarball on disk).
/// 2. `vendor/cef/<platform>/Release/Chromium Embedded Framework.framework`.
fn resolve_framework_dir(workspace: &Path, platform_override: Option<&str>) -> Result<PathBuf> {
    if let Ok(p) = env::var(FRAMEWORK_OVERRIDE_ENV) {
        let path = PathBuf::from(p);
        if !path.exists() {
            bail!(
                "{FRAMEWORK_OVERRIDE_ENV} = `{}` does not exist",
                path.display()
            );
        }
        eprintln!(
            "xtask: using framework override {}={}",
            FRAMEWORK_OVERRIDE_ENV,
            path.display()
        );
        return Ok(path);
    }

    let platform = platform_override.unwrap_or("macosarm64");
    let candidate = workspace
        .join("vendor/cef")
        .join(platform)
        .join("Release")
        .join("Chromium Embedded Framework.framework");
    if !candidate.exists() {
        bail!(
            "no macOS CEF framework at {}; \
             run `cargo xtask fetch-cef --platform {platform}` (cross-fetch) \
             or set {FRAMEWORK_OVERRIDE_ENV}=<dir> for assembly-only testing",
            candidate.display()
        );
    }
    Ok(candidate)
}

/// Read the workspace package version from the root `Cargo.toml`.
///
/// We avoid pulling a TOML parser in just for this: the value lives at
/// `[workspace.package] version = "..."`, and a tiny line scan is
/// enough for our needs.
fn workspace_version(workspace: &Path) -> Result<String> {
    let manifest = workspace.join("Cargo.toml");
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let mut in_workspace_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
            continue;
        }
        if in_workspace_package && let Some(rest) = trimmed.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                let val = rest.trim_matches('"');
                return Ok(val.to_string());
            }
        }
    }
    bail!(
        "could not find workspace.package.version in {}",
        manifest.display()
    )
}

/// Build the bundle layout. See module docs for the tree.
fn stage_bundle(
    app_dir: &Path,
    buffr_bin: &Path,
    helper_bin: &Path,
    framework_dir: &Path,
    version: &str,
) -> Result<()> {
    let contents = app_dir.join("Contents");
    let macos = contents.join("MacOS");
    let frameworks = contents.join("Frameworks");
    fs::create_dir_all(&macos).with_context(|| format!("creating {}", macos.display()))?;
    fs::create_dir_all(&frameworks)
        .with_context(|| format!("creating {}", frameworks.display()))?;

    // Main Info.plist + PkgInfo.
    let main_plist = render_main_plist(version);
    fs::write(contents.join("Info.plist"), main_plist)
        .with_context(|| format!("writing {}/Info.plist", contents.display()))?;
    fs::write(contents.join("PkgInfo"), b"APPL????")
        .with_context(|| format!("writing {}/PkgInfo", contents.display()))?;

    // Main executable.
    let main_exec = macos.join("buffr");
    copy_file_executable(buffr_bin, &main_exec)?;

    // Framework — always present in a real build, but we still copy via
    // recursive walk so the bundle works on Linux runners pointing at a
    // stub directory via BUFFR_BUNDLE_FRAMEWORK_DIR.
    let dest_framework = frameworks.join("Chromium Embedded Framework.framework");
    copy_dir_recursive(framework_dir, &dest_framework)
        .with_context(|| format!("copying framework into {}", dest_framework.display()))?;

    // Nested helper bundles. Apple's signing model wants four distinct
    // helpers (catch-all, GPU, Renderer, Plugin); cef-rs 147 only
    // resolves a single `browser_subprocess_path` so every flavor's
    // executable is a `fs::copy` of the same `buffr-helper` binary
    // (notarisation rejects symlinks for executables).
    for flavor in HELPER_FLAVORS {
        let bundle_name = format!("{} Helper{}.app", NAME, flavor.suffix);
        let exec_name = format!("{} Helper{}", NAME, flavor.suffix);
        let helper_app = frameworks.join(&bundle_name);
        let helper_contents = helper_app.join("Contents");
        let helper_macos = helper_contents.join("MacOS");
        fs::create_dir_all(&helper_macos)
            .with_context(|| format!("creating {}", helper_macos.display()))?;

        let helper_plist = render_helper_plist(flavor, version, &exec_name);
        fs::write(helper_contents.join("Info.plist"), helper_plist)
            .with_context(|| format!("writing {}/Info.plist", helper_contents.display()))?;
        fs::write(helper_contents.join("PkgInfo"), b"APPL????")
            .with_context(|| format!("writing {}/PkgInfo", helper_contents.display()))?;

        let helper_exec = helper_macos.join(&exec_name);
        copy_file_executable(helper_bin, &helper_exec)?;
    }

    Ok(())
}

/// Helper-bundle flavors shipped inside `buffr.app/Contents/Frameworks/`.
///
/// `suffix` is appended to `"buffr Helper"` for the bundle + executable
/// names — `""` is the catch-all helper (`buffr Helper.app`), `" (GPU)"`
/// becomes `buffr Helper (GPU).app`, etc. Apple requires every nested
/// `.app`'s Mach-O have a *distinct* file name; `fs::copy` is used for
/// each (notarisation rejects symlinks for executables).
#[derive(Debug, Clone, Copy)]
struct HelperFlavor {
    /// Name suffix, e.g. `""`, `" (GPU)"`, `" (Renderer)"`, `" (Plugin)"`.
    suffix: &'static str,
    /// Embedded plist template body.
    plist_template: &'static str,
}

const HELPER_FLAVORS: &[HelperFlavor] = &[
    HelperFlavor {
        suffix: "",
        plist_template: HELPER_PLIST_TEMPLATE,
    },
    HelperFlavor {
        suffix: " (GPU)",
        plist_template: HELPER_GPU_PLIST_TEMPLATE,
    },
    HelperFlavor {
        suffix: " (Renderer)",
        plist_template: HELPER_RENDERER_PLIST_TEMPLATE,
    },
    HelperFlavor {
        suffix: " (Plugin)",
        plist_template: HELPER_PLUGIN_PLIST_TEMPLATE,
    },
];

fn render_main_plist(version: &str) -> String {
    MAIN_PLIST_TEMPLATE
        .replace("{NAME}", NAME)
        .replace("{VERSION}", version)
        .replace("{BUNDLE_ID_MAIN}", BUNDLE_ID_MAIN)
        .replace("{EXECUTABLE}", "buffr")
        .replace("{COPYRIGHT}", COPYRIGHT)
}

fn render_helper_plist(flavor: &HelperFlavor, version: &str, executable: &str) -> String {
    flavor
        .plist_template
        .replace("{NAME}", &format!("{NAME} Helper{}", flavor.suffix))
        .replace("{VERSION}", version)
        .replace("{BUNDLE_ID_HELPER}", BUNDLE_ID_HELPER)
        .replace("{EXECUTABLE}", executable)
        .replace("{COPYRIGHT}", COPYRIGHT)
}

/// Copy a single file and set executable mode on Unix hosts.
///
/// `fs::copy` already preserves permissions on Unix, but we set the
/// bits explicitly so cross-bundling from a Linux box (where the
/// source file already has +x) lands a +x file on the destination
/// regardless of `umask`.
fn copy_file_executable(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dest).with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

// ----------------------------- package-linux -----------------------------

/// Embedded `AppRun` template for the AppImage AppDir.
const APPRUN_TEMPLATE: &str = include_str!("../templates/AppRun");
/// Embedded shared `.desktop` file (canonical source under `pkg/`).
const DESKTOP_TEMPLATE: &str = include_str!("../../pkg/buffr.desktop");
/// Embedded Debian control file template.
const DEB_CONTROL_TEMPLATE: &str = include_str!("../templates/deb.control");
/// Embedded Debian postinst hook.
const DEB_POSTINST: &str = include_str!("../templates/deb.postinst");
/// Embedded Debian prerm hook.
const DEB_PRERM: &str = include_str!("../templates/deb.prerm");
/// Embedded PKGBUILD template (`{VERSION}` substituted).
const PKGBUILD_TEMPLATE: &str = include_str!("../templates/PKGBUILD.in");

/// Pre-built `appimagetool` URL. We mirror this under
/// `vendor/appimagetool/` so CI hits the cache after the first run.
const APPIMAGETOOL_URL: &str = "https://github.com/AppImage/appimagetool/releases/download/continuous/\
     appimagetool-x86_64.AppImage";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxVariant {
    AppImage,
    Deb,
    Aur,
    All,
}

impl LinuxVariant {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "appimage" => Ok(Self::AppImage),
            "deb" => Ok(Self::Deb),
            "aur" => Ok(Self::Aur),
            "all" => Ok(Self::All),
            other => bail!("unknown --variant `{other}` (appimage|deb|aur|all)"),
        }
    }
}

#[derive(Debug)]
struct PackageLinuxArgs {
    release: bool,
    variant: LinuxVariant,
}

impl Default for PackageLinuxArgs {
    fn default() -> Self {
        Self {
            release: false,
            variant: LinuxVariant::All,
        }
    }
}

fn package_linux(args: Vec<String>) -> Result<()> {
    let mut parsed = PackageLinuxArgs::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--release" => parsed.release = true,
            "--variant" => {
                let v = iter.next().context("--variant requires a value")?;
                parsed.variant = LinuxVariant::parse(&v)?;
            }
            other => bail!("unknown package-linux arg `{other}`"),
        }
    }

    let workspace = workspace_root()?;
    let profile = if parsed.release { "release" } else { "debug" };
    let version = workspace_version(&workspace)?;

    eprintln!(
        "xtask: package-linux variant={:?} profile={profile} version={version}",
        parsed.variant
    );

    let dist_dir = workspace.join("target/dist/linux");
    fs::create_dir_all(&dist_dir).with_context(|| format!("creating {}", dist_dir.display()))?;

    // 1. Build the workspace binaries. The buffr-core build.rs will stage
    //    libcef.so, *.pak, locales/, icudtl.dat next to the binaries.
    eprintln!("xtask: building buffr + buffr-helper ({profile})");
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&workspace)
        .arg("build")
        .arg("-p")
        .arg("buffr")
        .arg("-p")
        .arg("buffr-helper");
    if parsed.release {
        cmd.arg("--release");
    }
    let status = cmd.status().context("spawning cargo build")?;
    if !status.success() {
        bail!("cargo build failed (status {status:?})");
    }

    let target_dir = workspace.join("target").join(profile);
    let payload = collect_runtime_payload(&target_dir)?;

    // 2. Always (re)write the AUR PKGBUILD with the current version. It
    //    is cheap and keeps `pkg/aur/PKGBUILD` in lockstep with the
    //    workspace version even if the user only asked for AppImage.
    if matches!(parsed.variant, LinuxVariant::Aur | LinuxVariant::All) {
        write_pkgbuild(&workspace, &version)?;
    }

    if matches!(parsed.variant, LinuxVariant::AppImage | LinuxVariant::All) {
        build_appimage(&workspace, &dist_dir, &target_dir, &payload, &version)?;
    }

    if matches!(parsed.variant, LinuxVariant::Deb | LinuxVariant::All) {
        build_deb(&workspace, &dist_dir, &target_dir, &payload, &version)?;
    }

    eprintln!();
    eprintln!("xtask: package-linux complete");
    eprintln!("       artifacts: {}", dist_dir.display());
    Ok(())
}

/// Filesystem locations of the runtime payload that all three variants
/// embed. `target/<profile>/` is populated by the `buffr-core` build
/// script; if `libcef.so` is missing we treat that as fatal — the
/// resulting package would be unusable.
#[derive(Debug)]
struct RuntimePayload {
    /// Absolute path to the `buffr` binary.
    buffr: PathBuf,
    /// Absolute path to the `buffr-helper` binary.
    helper: PathBuf,
    /// Absolute path to `libcef.so` (Linux dist).
    libcef: PathBuf,
    /// Absolute paths to `*.pak` files.
    paks: Vec<PathBuf>,
    /// Absolute paths to `*.dat` / `*.bin` blobs (icudtl, snapshot).
    blobs: Vec<PathBuf>,
    /// Absolute path to the `locales/` directory.
    locales: PathBuf,
}

fn collect_runtime_payload(target_dir: &Path) -> Result<RuntimePayload> {
    let buffr = target_dir.join("buffr");
    let helper = target_dir.join("buffr-helper");
    let libcef = target_dir.join("libcef.so");
    let locales = target_dir.join("locales");

    if !buffr.exists() {
        bail!("expected `{}` after build", buffr.display());
    }
    if !helper.exists() {
        bail!("expected `{}` after build", helper.display());
    }
    if !libcef.exists() {
        bail!(
            "expected `{}` after build — buffr-core build.rs should have staged \
             libcef.so. Did you `cargo xtask fetch-cef`?",
            libcef.display()
        );
    }
    if !locales.exists() {
        bail!(
            "expected `{}` after build — buffr-core build.rs should have staged \
             the locales/ tree.",
            locales.display()
        );
    }

    let mut paks = Vec::new();
    let mut blobs = Vec::new();
    for entry in
        fs::read_dir(target_dir).with_context(|| format!("reading {}", target_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if name.ends_with(".pak") {
            paks.push(path);
        } else if name.ends_with(".dat") || name.ends_with(".bin") {
            blobs.push(path);
        }
    }
    paks.sort();
    blobs.sort();

    Ok(RuntimePayload {
        buffr,
        helper,
        libcef,
        paks,
        blobs,
        locales,
    })
}

/// Stage the shared `/opt/buffr/` payload inside `dest`. Used by both
/// the AppImage (`<AppDir>/usr/lib/`-ish) and the Debian package
/// (`/opt/buffr/`).
fn stage_payload(dest: &Path, payload: &RuntimePayload) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    copy_file_executable(&payload.buffr, &dest.join("buffr"))?;
    copy_file_executable(&payload.helper, &dest.join("buffr-helper"))?;
    copy_into_dir(&payload.libcef, dest)?;
    for pak in &payload.paks {
        copy_into_dir(pak, dest)?;
    }
    for blob in &payload.blobs {
        copy_into_dir(blob, dest)?;
    }
    let locales_dest = dest.join("locales");
    let _ = fs::remove_dir_all(&locales_dest);
    copy_dir_recursive(&payload.locales, &locales_dest)?;
    Ok(())
}

// ---------------------------- AppImage ----------------------------------

fn build_appimage(
    workspace: &Path,
    dist_dir: &Path,
    target_dir: &Path,
    payload: &RuntimePayload,
    version: &str,
) -> Result<()> {
    let appdir = target_dir.join("buffr.AppDir");
    if appdir.exists() {
        fs::remove_dir_all(&appdir)
            .with_context(|| format!("wiping existing {}", appdir.display()))?;
    }
    let usr_bin = appdir.join("usr/bin");
    let usr_lib = appdir.join("usr/lib");
    fs::create_dir_all(&usr_bin)?;
    fs::create_dir_all(&usr_lib)?;

    // Binaries land in usr/bin/, libcef + paks + locales in usr/lib/.
    copy_file_executable(&payload.buffr, &usr_bin.join("buffr"))?;
    copy_file_executable(&payload.helper, &usr_bin.join("buffr-helper"))?;
    copy_into_dir(&payload.libcef, &usr_lib)?;
    for pak in &payload.paks {
        copy_into_dir(pak, &usr_lib)?;
    }
    for blob in &payload.blobs {
        copy_into_dir(blob, &usr_lib)?;
    }
    let locales_dest = usr_lib.join("locales");
    copy_dir_recursive(&payload.locales, &locales_dest)?;

    // AppRun launcher script.
    let apprun = appdir.join("AppRun");
    fs::write(&apprun, APPRUN_TEMPLATE).with_context(|| format!("writing {}", apprun.display()))?;
    set_executable(&apprun)?;

    // .desktop + icon at AppDir root (appimagetool requirement).
    fs::write(appdir.join("buffr.desktop"), DESKTOP_TEMPLATE)?;

    let icon_src = workspace.join("pkg/buffr.png");
    if icon_src.exists() {
        fs::copy(&icon_src, appdir.join("buffr.png"))?;
    } else {
        eprintln!("xtask: warning — pkg/buffr.png missing; AppImage will lack an icon");
    }

    // Try to invoke appimagetool. Resolve in this order:
    // 1. $PATH
    // 2. vendor/appimagetool/appimagetool-x86_64.AppImage (cached)
    // 3. download into vendor/appimagetool/
    // If none of those work, leave the AppDir as the "artifact" — CI
    // will exercise the full path on a runner with internet.
    let tool = match resolve_appimagetool(workspace) {
        Ok(p) => Some(p),
        Err(err) => {
            eprintln!(
                "xtask: appimagetool unavailable ({err}); leaving AppDir at {}",
                appdir.display()
            );
            None
        }
    };

    let Some(tool) = tool else {
        return Ok(());
    };

    let out = dist_dir.join(format!("buffr-{version}-x86_64.AppImage"));
    eprintln!("xtask: running appimagetool -> {}", out.display());
    let status = Command::new(&tool)
        .env("ARCH", "x86_64")
        .arg(&appdir)
        .arg(&out)
        .status()
        .with_context(|| format!("spawning {}", tool.display()))?;
    if !status.success() {
        eprintln!("xtask: warning — appimagetool exited {status:?}");
        return Ok(());
    }
    set_executable(&out)?;
    eprintln!("xtask: AppImage written to {}", out.display());
    Ok(())
}

fn resolve_appimagetool(workspace: &Path) -> Result<PathBuf> {
    // 1. PATH lookup.
    if let Ok(out) = Command::new("which").arg("appimagetool").output()
        && out.status.success()
    {
        let line = String::from_utf8_lossy(&out.stdout);
        let line = line.trim();
        if !line.is_empty() {
            return Ok(PathBuf::from(line));
        }
    }

    // 2. vendor cache.
    let cache_dir = workspace.join("vendor/appimagetool");
    let cached = cache_dir.join("appimagetool-x86_64.AppImage");
    if cached.exists() {
        return Ok(cached);
    }

    // 3. Download. Strip whitespace from the URL constant (folded above
    //    for readability) before handing it to ureq.
    let url: String = APPIMAGETOOL_URL.split_whitespace().collect();
    fs::create_dir_all(&cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    download(&url, &cached).context("downloading appimagetool")?;
    set_executable(&cached)?;
    Ok(cached)
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

// ---------------------------- .deb --------------------------------------

fn build_deb(
    workspace: &Path,
    dist_dir: &Path,
    target_dir: &Path,
    payload: &RuntimePayload,
    version: &str,
) -> Result<()> {
    let debroot = target_dir.join("buffr-deb");
    if debroot.exists() {
        fs::remove_dir_all(&debroot)
            .with_context(|| format!("wiping existing {}", debroot.display()))?;
    }

    // /opt/buffr/<payload>
    let opt = debroot.join("opt/buffr");
    stage_payload(&opt, payload)?;

    let icon_src = workspace.join("pkg/buffr.png");
    if icon_src.exists() {
        fs::copy(&icon_src, opt.join("icon.png"))?;
        let icon_dest = debroot.join("usr/share/icons/hicolor/512x512/apps");
        fs::create_dir_all(&icon_dest)?;
        fs::copy(&icon_src, icon_dest.join("buffr.png"))?;
    }

    // .desktop in usr/share/applications.
    let apps = debroot.join("usr/share/applications");
    fs::create_dir_all(&apps)?;
    fs::write(apps.join("buffr.desktop"), DESKTOP_TEMPLATE)?;

    // DEBIAN/{control,postinst,prerm}.
    let debian = debroot.join("DEBIAN");
    fs::create_dir_all(&debian)?;
    let control = DEB_CONTROL_TEMPLATE.replace("{VERSION}", version);
    fs::write(debian.join("control"), control)?;
    let postinst = debian.join("postinst");
    fs::write(&postinst, DEB_POSTINST)?;
    set_executable(&postinst)?;
    let prerm = debian.join("prerm");
    fs::write(&prerm, DEB_PRERM)?;
    set_executable(&prerm)?;

    // Invoke dpkg-deb if available. Otherwise leave the staging tree
    // and let CI pick it up.
    let out = dist_dir.join(format!("buffr-{version}-amd64.deb"));
    let dpkg = Command::new("which").arg("dpkg-deb").output().ok();
    let dpkg_ok = dpkg.as_ref().map(|o| o.status.success()).unwrap_or(false);
    if !dpkg_ok {
        eprintln!(
            "xtask: dpkg-deb not on PATH; leaving deb staging tree at {}",
            debroot.display()
        );
        return Ok(());
    }

    eprintln!("xtask: running dpkg-deb --build -> {}", out.display());
    let status = Command::new("dpkg-deb")
        .arg("--build")
        .arg("--root-owner-group")
        .arg(&debroot)
        .arg(&out)
        .status()
        .context("spawning dpkg-deb")?;
    if !status.success() {
        eprintln!("xtask: warning — dpkg-deb exited {status:?}");
        return Ok(());
    }
    eprintln!("xtask: deb written to {}", out.display());
    Ok(())
}

// ---------------------------- AUR PKGBUILD ------------------------------

fn write_pkgbuild(workspace: &Path, version: &str) -> Result<()> {
    let pkgbuild_dir = workspace.join("pkg/aur");
    fs::create_dir_all(&pkgbuild_dir)
        .with_context(|| format!("creating {}", pkgbuild_dir.display()))?;
    let rendered = PKGBUILD_TEMPLATE.replace("{VERSION}", version);
    let path = pkgbuild_dir.join("PKGBUILD");
    fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
    eprintln!(
        "xtask: PKGBUILD updated at {} (pkgver={version})",
        path.display()
    );
    Ok(())
}

// ----------------------------- package-macos-dmg ------------------------

#[derive(Debug, Default)]
struct PackageMacosDmgArgs {
    release: bool,
    /// Override the source `.app` (default: `target/<profile>/buffr.app`).
    /// Mostly a hook for tests; CLI users go through `bundle-macos`.
    app: Option<PathBuf>,
}

fn package_macos_dmg(args: Vec<String>) -> Result<()> {
    let mut parsed = PackageMacosDmgArgs::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--release" => parsed.release = true,
            "--app" => {
                parsed.app = Some(PathBuf::from(
                    iter.next().context("--app requires a value")?,
                ))
            }
            other => bail!("unknown package-macos-dmg arg `{other}`"),
        }
    }

    let workspace = workspace_root()?;
    let profile = if parsed.release { "release" } else { "debug" };
    let version = workspace_version(&workspace)?;
    let target_dir = workspace.join("target").join(profile);

    let app_dir = parsed.app.unwrap_or_else(|| target_dir.join("buffr.app"));
    if !app_dir.exists() {
        bail!(
            "no buffr.app at {} — run `cargo xtask bundle-macos{}` first",
            app_dir.display(),
            if parsed.release { " --release" } else { "" }
        );
    }

    let dist_dir = workspace.join("target/dist/macos");
    fs::create_dir_all(&dist_dir).with_context(|| format!("creating {}", dist_dir.display()))?;

    // Stage the DMG layout under target/<profile>/dmg-staging/.
    let staging = target_dir.join("dmg-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).with_context(|| format!("wiping {}", staging.display()))?;
    }
    stage_dmg(&staging, &app_dir)?;

    // Architecture suffix: macOS arm64 vs x86_64. Default is host arch
    // since the bundle binary tracks the build target.
    let arch = macos_arch_suffix();
    let dmg_name = format!("buffr-{version}-{arch}.dmg");
    let dmg_path = dist_dir.join(&dmg_name);

    // Pick a tool: hdiutil (macOS) or genisoimage (Linux fallback). If
    // neither is on PATH we leave the staging tree and warn — CI on a
    // macos-latest runner exercises hdiutil for real.
    let tool = resolve_dmg_tool();
    match tool {
        DmgTool::Hdiutil => {
            eprintln!("xtask: hdiutil create -> {}", dmg_path.display());
            let status = Command::new("hdiutil")
                .arg("create")
                .arg("-volname")
                .arg("buffr")
                .arg("-srcfolder")
                .arg(&staging)
                .arg("-ov")
                .arg("-format")
                .arg("UDZO")
                .arg(&dmg_path)
                .status()
                .context("spawning hdiutil")?;
            if !status.success() {
                bail!("hdiutil exited {status:?}");
            }
            eprintln!("xtask: dmg written to {}", dmg_path.display());
        }
        DmgTool::Genisoimage => {
            eprintln!("xtask: hdiutil unavailable; using genisoimage fallback (UDF, not UDZO)");
            let status = Command::new("genisoimage")
                .arg("-V")
                .arg("buffr")
                .arg("-D")
                .arg("-R")
                .arg("-apple")
                .arg("-no-pad")
                .arg("-o")
                .arg(&dmg_path)
                .arg(&staging)
                .status()
                .context("spawning genisoimage")?;
            if !status.success() {
                bail!("genisoimage exited {status:?}");
            }
            eprintln!("xtask: dmg-equivalent written to {}", dmg_path.display());
            eprintln!(
                "xtask: warning — genisoimage output is an ISO9660 image, not a real \
                 hdiutil UDZO DMG; macOS will mount it but Finder layout / drag-target \
                 affordances are not preserved. Re-run on a macOS host for distribution."
            );
        }
        DmgTool::Missing => {
            eprintln!(
                "xtask: dmg tooling missing — staging tree at {}; install hdiutil (macOS) \
                 or genisoimage (Linux) to package",
                staging.display()
            );
            return Ok(());
        }
    }

    eprintln!();
    eprintln!("xtask: package-macos-dmg complete");
    eprintln!("       artifact: {}", dmg_path.display());
    eprintln!("       NOTE: unsigned. First-run users must clear the quarantine xattr:");
    eprintln!("           xattr -d com.apple.quarantine /Applications/buffr.app");
    eprintln!("       Signing + notarization land alongside docs/macos-signing.md.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmgTool {
    Hdiutil,
    Genisoimage,
    Missing,
}

fn resolve_dmg_tool() -> DmgTool {
    if which("hdiutil") {
        DmgTool::Hdiutil
    } else if which("genisoimage") {
        DmgTool::Genisoimage
    } else {
        DmgTool::Missing
    }
}

/// Lay out the DMG-staging directory:
///
/// ```text
/// dmg-staging/
///   buffr.app/                  (copy of the bundle)
///   Applications -> /Applications  (symlink, drag-target)
/// ```
///
/// `.background.png` and `.DS_Store` are intentionally not generated —
/// they only matter for visual layout when mounted, and producing them
/// faithfully needs an `osascript` + a mounted volume on macOS. Once
/// signing lands the post-Phase-6 release pipeline can layer those on.
fn stage_dmg(staging: &Path, app_dir: &Path) -> Result<()> {
    fs::create_dir_all(staging).with_context(|| format!("creating {}", staging.display()))?;

    // Copy buffr.app into the staging tree. We copy rather than symlink
    // so hdiutil sees a self-contained directory.
    let dest_app = staging.join("buffr.app");
    copy_dir_recursive(app_dir, &dest_app)
        .with_context(|| format!("copying bundle into {}", dest_app.display()))?;

    // Drag-target symlink to /Applications. On non-Unix hosts (Windows
    // dev box that somehow runs this) `symlink` won't compile; but
    // package-macos-dmg only ever runs on Unix anyway.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = staging.join("Applications");
        if link.exists() {
            fs::remove_file(&link).ok();
        }
        symlink("/Applications", &link)
            .with_context(|| format!("creating symlink {}", link.display()))?;
    }

    Ok(())
}

fn macos_arch_suffix() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        "unknown"
    }
}

fn which(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

// ----------------------------- package-windows-msi -----------------------

#[derive(Debug, Default)]
struct PackageWindowsMsiArgs {
    release: bool,
}

/// Render the WiX 3 source for the workspace.
///
/// Pulled out so unit tests can lock the substitution behaviour down
/// without spinning up the rest of the packaging pipeline.
fn render_wix(version: &str, install_dir: &str, arch: &str) -> String {
    WIX_TEMPLATE
        .replace("{VERSION}", version)
        .replace("{INSTALL_DIR}", install_dir)
        .replace("{ARCH}", arch)
}

fn package_windows_msi(args: Vec<String>) -> Result<()> {
    let mut parsed = PackageWindowsMsiArgs::default();
    for arg in args {
        match arg.as_str() {
            "--release" => parsed.release = true,
            other => bail!("unknown package-windows-msi arg `{other}`"),
        }
    }

    let workspace = workspace_root()?;
    let profile = if parsed.release { "release" } else { "debug" };
    let version = workspace_version(&workspace)?;

    let dist_dir = workspace.join("target/dist/windows");
    fs::create_dir_all(&dist_dir).with_context(|| format!("creating {}", dist_dir.display()))?;

    // 1. Always write the WiX source first so it's available for
    //    inspection even on a Linux box without the Windows binaries.
    let wxs = render_wix(&version, "buffr", "x64");
    let wxs_path = dist_dir.join("buffr.wxs");
    fs::write(&wxs_path, &wxs).with_context(|| format!("writing {}", wxs_path.display()))?;
    eprintln!("xtask: wrote {}", wxs_path.display());

    // 2. Locate buffr.exe + buffr-helper.exe. On a Windows host the
    //    profile-default `target/<profile>/` already has them; on Linux
    //    we look for an explicit cross-compile output under
    //    `target/x86_64-pc-windows-{msvc,gnu}/<profile>/`. This
    //    subcommand does not drive cross-compilation itself — the CI
    //    Windows runner builds natively, and Linux dev boxes can opt
    //    into the cross workflow manually.
    let payload = match collect_windows_payload(&workspace, profile) {
        Ok(p) => p,
        Err(err) => {
            eprintln!(
                "xtask: warning — Windows payload unavailable ({err}); \
                 leaving .wxs at {} for a Windows runner to consume",
                wxs_path.display()
            );
            return Ok(());
        }
    };
    let payload_dir = dist_dir.join("payload");
    if payload_dir.exists() {
        fs::remove_dir_all(&payload_dir)
            .with_context(|| format!("wiping {}", payload_dir.display()))?;
    }
    fs::create_dir_all(&payload_dir)?;
    stage_windows_payload(&payload_dir, &payload)?;

    // 3. Resolve candle + light. Both must exist; partial WiX 3 install
    //    is treated as missing.
    let have_candle = which("candle") || which("candle.exe");
    let have_light = which("light") || which("light.exe");
    if !have_candle || !have_light {
        eprintln!(
            "xtask: candle/light from the WiX 3 toolset not on PATH; \
             leaving payload + .wxs at {}",
            dist_dir.display()
        );
        eprintln!(
            "       To produce the .msi: install WiX 3 (or the v3 build of WiX 4) \
             and re-run this command on a host with the toolset, or rely on the \
             CI windows-package job."
        );
        return Ok(());
    }

    // 4. Drive candle + light. `.wixobj` lands next to the wxs source;
    //    `light` produces the final `.msi` under `target/dist/windows/`.
    let wixobj = dist_dir.join("buffr.wixobj");
    eprintln!("xtask: candle -> {}", wixobj.display());
    let status = Command::new(if which("candle") {
        "candle"
    } else {
        "candle.exe"
    })
    .arg("-arch")
    .arg("x64")
    .arg("-o")
    .arg(&wixobj)
    .arg(&wxs_path)
    .current_dir(&dist_dir)
    .status()
    .context("spawning candle")?;
    if !status.success() {
        bail!("candle exited {status:?}");
    }

    let msi_path = dist_dir.join(format!("buffr-{version}-x64.msi"));
    eprintln!("xtask: light -> {}", msi_path.display());
    let status = Command::new(if which("light") { "light" } else { "light.exe" })
        .arg("-o")
        .arg(&msi_path)
        .arg(&wixobj)
        .current_dir(&dist_dir)
        .status()
        .context("spawning light")?;
    if !status.success() {
        bail!("light exited {status:?}");
    }
    eprintln!("xtask: msi written to {}", msi_path.display());
    eprintln!();
    eprintln!("xtask: package-windows-msi complete");
    eprintln!("       artifact: {}", msi_path.display());
    eprintln!(
        "       NOTE: unsigned. SmartScreen will warn until Authenticode signing lands \
         (see docs/windows-packaging.md)."
    );
    Ok(())
}

#[derive(Debug)]
struct WindowsPayload {
    buffr_exe: PathBuf,
    helper_exe: PathBuf,
    libcef_dll: PathBuf,
    icudtl: PathBuf,
    paks: Vec<PathBuf>,
    blobs: Vec<PathBuf>,
    locales: PathBuf,
}

/// Search the typical native-Windows + cross-compile output paths for
/// the MSI payload. Errors only when *no* candidate location has the
/// minimum binaries; otherwise picks the first one that does.
fn collect_windows_payload(workspace: &Path, profile: &str) -> Result<WindowsPayload> {
    let candidates: Vec<PathBuf> = vec![
        workspace.join("target").join(profile),
        workspace
            .join("target/x86_64-pc-windows-msvc")
            .join(profile),
        workspace.join("target/x86_64-pc-windows-gnu").join(profile),
    ];

    for dir in &candidates {
        let buffr_exe = dir.join("buffr.exe");
        let helper_exe = dir.join("buffr-helper.exe");
        let libcef_dll = dir.join("libcef.dll");
        if buffr_exe.exists() && helper_exe.exists() && libcef_dll.exists() {
            return collect_windows_payload_from(dir);
        }
    }

    bail!(
        "no Windows payload found under any of {} \
         — build via `cargo build --target x86_64-pc-windows-msvc --release` (Windows host) \
         or `cargo build --target x86_64-pc-windows-gnu --release` (Linux cross) first.\n\
         Cross-build prerequisites: see docs/windows-packaging.md",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn collect_windows_payload_from(dir: &Path) -> Result<WindowsPayload> {
    let buffr_exe = dir.join("buffr.exe");
    let helper_exe = dir.join("buffr-helper.exe");
    let libcef_dll = dir.join("libcef.dll");
    let icudtl = dir.join("icudtl.dat");
    let locales = dir.join("locales");

    if !icudtl.exists() {
        bail!(
            "missing `{}` next to buffr.exe — buffr-core build.rs should have staged it",
            icudtl.display()
        );
    }
    if !locales.exists() {
        bail!("missing `{}` next to buffr.exe", locales.display());
    }

    let mut paks = Vec::new();
    let mut blobs = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if name.ends_with(".pak") {
            paks.push(path);
        } else if name.ends_with(".bin") {
            blobs.push(path);
        }
    }
    paks.sort();
    blobs.sort();

    Ok(WindowsPayload {
        buffr_exe,
        helper_exe,
        libcef_dll,
        icudtl,
        paks,
        blobs,
        locales,
    })
}

fn stage_windows_payload(dest: &Path, p: &WindowsPayload) -> Result<()> {
    fs::create_dir_all(dest)?;
    copy_into_dir(&p.buffr_exe, dest)?;
    copy_into_dir(&p.helper_exe, dest)?;
    copy_into_dir(&p.libcef_dll, dest)?;
    copy_into_dir(&p.icudtl, dest)?;
    for pak in &p.paks {
        copy_into_dir(pak, dest)?;
    }
    for blob in &p.blobs {
        copy_into_dir(blob, dest)?;
    }
    let locales_dest = dest.join("locales");
    let _ = fs::remove_dir_all(&locales_dest);
    copy_dir_recursive(&p.locales, &locales_dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cef_minimal_url_macosarm64() {
        let url = cef_minimal_url(
            DEFAULT_CDN,
            "macosarm64",
            "147.0.10+gabcdef0+chromium-147.0.0.0",
        );
        assert_eq!(
            url,
            "https://cef-builds.spotifycdn.com/\
             cef_binary_147.0.10+gabcdef0+chromium-147.0.0.0_macosarm64_minimal.tar.bz2"
        );
    }

    #[test]
    fn cef_minimal_url_macosx64() {
        let url = cef_minimal_url(DEFAULT_CDN, "macosx64", "147.0.10");
        assert_eq!(
            url,
            "https://cef-builds.spotifycdn.com/cef_binary_147.0.10_macosx64_minimal.tar.bz2"
        );
    }

    #[test]
    fn cef_minimal_url_linux64() {
        let url = cef_minimal_url(DEFAULT_CDN, "linux64", "147.0.10");
        assert_eq!(
            url,
            "https://cef-builds.spotifycdn.com/cef_binary_147.0.10_linux64_minimal.tar.bz2"
        );
    }

    #[test]
    fn render_main_plist_substitutes_placeholders() {
        let s = render_main_plist("1.2.3");
        assert!(s.contains("<string>1.2.3</string>"));
        assert!(s.contains("<string>sh.kryptic.buffr</string>"));
        assert!(s.contains("<string>buffr</string>"));
        assert!(!s.contains("{VERSION}"));
        assert!(!s.contains("{BUNDLE_ID_MAIN}"));
        assert!(!s.contains("{EXECUTABLE}"));
        assert!(!s.contains("{NAME}"));
        assert!(!s.contains("{COPYRIGHT}"));
    }

    #[test]
    fn render_helper_plist_substitutes_placeholders() {
        let base = HelperFlavor {
            suffix: "",
            plist_template: HELPER_PLIST_TEMPLATE,
        };
        let s = render_helper_plist(&base, "1.2.3", "buffr Helper");
        assert!(s.contains("<string>sh.kryptic.buffr.helper</string>"));
        assert!(s.contains("<string>buffr Helper</string>"));
        // Helper plist drops the icon + category.
        assert!(!s.contains("CFBundleIconFile"));
        assert!(!s.contains("LSApplicationCategoryType"));
        // Helper plist must mark itself as a UI element so it never
        // shows up in the Dock alongside the main bundle.
        assert!(s.contains("<key>LSUIElement</key>"));
        assert!(!s.contains("{VERSION}"));
        assert!(!s.contains("{BUNDLE_ID_HELPER}"));
    }

    #[test]
    fn render_helper_plist_per_flavor_bundle_ids() {
        // Each flavor must produce its own CFBundleIdentifier suffix so
        // future per-helper signing entitlements don't collide.
        let cases = [
            (" (GPU)", HELPER_GPU_PLIST_TEMPLATE, ".gpu"),
            (" (Renderer)", HELPER_RENDERER_PLIST_TEMPLATE, ".renderer"),
            (" (Plugin)", HELPER_PLUGIN_PLIST_TEMPLATE, ".plugin"),
        ];
        for (suffix, template, want) in cases {
            let flavor = HelperFlavor {
                suffix,
                plist_template: template,
            };
            let exec = format!("buffr Helper{suffix}");
            let s = render_helper_plist(&flavor, "1.2.3", &exec);
            let expected_id = format!("sh.kryptic.buffr.helper{want}");
            assert!(
                s.contains(&format!("<string>{expected_id}</string>")),
                "missing {expected_id} in {suffix} plist:\n{s}"
            );
            assert!(s.contains(&format!("<string>{exec}</string>")));
        }
    }

    #[test]
    fn helper_flavors_count_is_four() {
        // GPU / Renderer / Plugin / catch-all. If this changes, the
        // bundle-layout test below + macos-signing.md need to track.
        assert_eq!(HELPER_FLAVORS.len(), 4);
    }

    #[test]
    fn bundle_macos_stage_layout() {
        // Build a fake framework + binaries on disk and run
        // `stage_bundle` against them; assert the resulting tree.
        let tmp = tempdir();
        let fw = tmp.path().join("Chromium Embedded Framework.framework");
        fs::create_dir_all(fw.join("Versions/A/Resources")).unwrap();
        fs::write(fw.join("Versions/A/Chromium Embedded Framework"), b"stub").unwrap();

        let buffr_bin = tmp.path().join("buffr");
        let helper_bin = tmp.path().join("buffr-helper");
        fs::write(&buffr_bin, b"#!/bin/sh\necho buffr\n").unwrap();
        fs::write(&helper_bin, b"#!/bin/sh\necho helper\n").unwrap();

        let app_dir = tmp.path().join("buffr.app");
        stage_bundle(&app_dir, &buffr_bin, &helper_bin, &fw, "9.9.9").unwrap();

        assert!(app_dir.join("Contents/Info.plist").exists());
        assert!(app_dir.join("Contents/PkgInfo").exists());
        assert!(app_dir.join("Contents/MacOS/buffr").exists());
        assert!(
            app_dir
                .join("Contents/Frameworks/Chromium Embedded Framework.framework")
                .exists()
        );
        // All four helper flavors are present (catch-all + GPU + Renderer + Plugin).
        for (suffix, exec_suffix) in [
            ("", ""),
            (" (GPU)", " (GPU)"),
            (" (Renderer)", " (Renderer)"),
            (" (Plugin)", " (Plugin)"),
        ] {
            let helper_app = app_dir.join(format!("Contents/Frameworks/buffr Helper{suffix}.app"));
            assert!(
                helper_app.join("Contents/Info.plist").exists(),
                "missing Info.plist for buffr Helper{suffix}.app"
            );
            assert!(
                helper_app.join("Contents/PkgInfo").exists(),
                "missing PkgInfo for buffr Helper{suffix}.app"
            );
            assert!(
                helper_app
                    .join(format!("Contents/MacOS/buffr Helper{exec_suffix}"))
                    .exists(),
                "missing executable for buffr Helper{suffix}.app"
            );
        }

        // PkgInfo content.
        assert_eq!(
            fs::read_to_string(app_dir.join("Contents/PkgInfo")).unwrap(),
            "APPL????"
        );

        // Main plist contains substituted version.
        let plist = fs::read_to_string(app_dir.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("<string>9.9.9</string>"));
    }

    #[test]
    fn linux_variant_parse_known() {
        assert_eq!(
            LinuxVariant::parse("appimage").unwrap(),
            LinuxVariant::AppImage
        );
        assert_eq!(LinuxVariant::parse("deb").unwrap(), LinuxVariant::Deb);
        assert_eq!(LinuxVariant::parse("aur").unwrap(), LinuxVariant::Aur);
        assert_eq!(LinuxVariant::parse("all").unwrap(), LinuxVariant::All);
    }

    #[test]
    fn linux_variant_parse_unknown_errors() {
        let err = LinuxVariant::parse("snap").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown --variant"));
        assert!(msg.contains("snap"));
    }

    #[test]
    fn deb_control_template_substitutes_version() {
        let rendered = DEB_CONTROL_TEMPLATE.replace("{VERSION}", "1.2.3");
        assert!(rendered.contains("Version: 1.2.3"));
        assert!(rendered.contains("Package: buffr"));
        assert!(rendered.contains("Architecture: amd64"));
        assert!(!rendered.contains("{VERSION}"));
        // The deb depends list is the contract with apt — surface the
        // exact set so accidental edits show up in CI.
        assert!(rendered.contains("libgtk-3-0"));
        assert!(rendered.contains("libnss3"));
        assert!(rendered.contains("libgbm1"));
        assert!(rendered.contains("libgles2"));
    }

    #[test]
    fn pkgbuild_template_substitutes_version() {
        let rendered = PKGBUILD_TEMPLATE.replace("{VERSION}", "0.1.0");
        assert!(rendered.contains("pkgver=0.1.0"));
        assert!(rendered.contains("pkgname=buffr"));
        assert!(rendered.contains("sha256sums=('SKIP')"));
        assert!(!rendered.contains("{VERSION}"));
        // makedepends should pin the toolchain as `rust` + `cargo`.
        assert!(rendered.contains("makedepends=('rust' 'cargo' 'cmake')"));
    }

    #[test]
    fn apprun_template_is_bash_launcher() {
        assert!(APPRUN_TEMPLATE.starts_with("#!/usr/bin/env bash"));
        assert!(APPRUN_TEMPLATE.contains("LD_LIBRARY_PATH"));
        assert!(APPRUN_TEMPLATE.contains("usr/bin/buffr"));
    }

    #[test]
    fn desktop_template_has_required_keys() {
        // Keep the minimum keys that LXQt / GNOME / KDE all parse.
        assert!(DESKTOP_TEMPLATE.contains("[Desktop Entry]"));
        assert!(DESKTOP_TEMPLATE.contains("Name=buffr"));
        assert!(DESKTOP_TEMPLATE.contains("Exec=buffr %U"));
        assert!(DESKTOP_TEMPLATE.contains("Icon=buffr"));
        assert!(DESKTOP_TEMPLATE.contains("Type=Application"));
        assert!(DESKTOP_TEMPLATE.contains("Categories=Network;WebBrowser;"));
    }

    #[test]
    fn stage_payload_lays_out_runtime_tree() {
        // Build a fake `target/release/` tree, hand it to
        // `collect_runtime_payload` + `stage_payload`, and assert the
        // resulting destination directory matches what the deb / aur
        // expectations encode.
        let tmp = tempdir();
        let target = tmp.path().join("target-release");
        fs::create_dir_all(target.join("locales")).unwrap();
        fs::write(target.join("buffr"), b"#!/bin/sh\n").unwrap();
        fs::write(target.join("buffr-helper"), b"#!/bin/sh\n").unwrap();
        fs::write(target.join("libcef.so"), b"\x7fELF").unwrap();
        fs::write(target.join("chrome_100_percent.pak"), b"pak").unwrap();
        fs::write(target.join("resources.pak"), b"pak").unwrap();
        fs::write(target.join("icudtl.dat"), b"dat").unwrap();
        fs::write(target.join("v8_context_snapshot.bin"), b"bin").unwrap();
        fs::write(target.join("locales/en-US.pak"), b"locale").unwrap();

        let payload = collect_runtime_payload(&target).unwrap();
        assert_eq!(payload.paks.len(), 2);
        assert_eq!(payload.blobs.len(), 2);

        let dest = tmp.path().join("opt-buffr");
        stage_payload(&dest, &payload).unwrap();
        assert!(dest.join("buffr").exists());
        assert!(dest.join("buffr-helper").exists());
        assert!(dest.join("libcef.so").exists());
        assert!(dest.join("chrome_100_percent.pak").exists());
        assert!(dest.join("resources.pak").exists());
        assert!(dest.join("icudtl.dat").exists());
        assert!(dest.join("v8_context_snapshot.bin").exists());
        assert!(dest.join("locales/en-US.pak").exists());
    }

    #[test]
    fn collect_runtime_payload_missing_libcef_errors() {
        let tmp = tempdir();
        let target = tmp.path().join("target-release");
        fs::create_dir_all(target.join("locales")).unwrap();
        fs::write(target.join("buffr"), b"#!/bin/sh\n").unwrap();
        fs::write(target.join("buffr-helper"), b"#!/bin/sh\n").unwrap();
        // No libcef.so on purpose.

        let err = collect_runtime_payload(&target).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("libcef.so"), "msg = {msg}");
    }

    #[test]
    fn render_wix_substitutes_placeholders() {
        let s = render_wix("0.1.2", "buffr", "x64");
        assert!(s.contains("Version=\"0.1.2\""));
        assert!(s.contains("Name=\"buffr\""));
        assert!(!s.contains("{VERSION}"));
        assert!(!s.contains("{INSTALL_DIR}"));
        assert!(!s.contains("{ARCH}"));
    }

    #[test]
    fn wix_template_targets_wix3_namespace() {
        // We deliberately target the WiX 3.x namespace + element set
        // (`<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">`,
        // `<Product>`, `<Package>`). WiX 4 / 5 use a different
        // namespace (`http://wixtoolset.org/schemas/v4/wxs`) and renamed
        // `<Product>` to `<Package>` at the root. WiX 3 tooling is the
        // most broadly available baseline today.
        assert!(WIX_TEMPLATE.contains("xmlns=\"http://schemas.microsoft.com/wix/2006/wi\""));
        assert!(WIX_TEMPLATE.contains("<Product"));
        assert!(WIX_TEMPLATE.contains("<MajorUpgrade"));
        assert!(WIX_TEMPLATE.contains("<MediaTemplate"));
    }

    #[test]
    fn wix_template_records_install_metadata() {
        // The HKLM\SOFTWARE\kryptic\buffr key + InstallPath/Version
        // values are how an external uninstaller / updater discovers
        // an existing install. Lock them down.
        assert!(WIX_TEMPLATE.contains("SOFTWARE\\kryptic\\buffr"));
        assert!(WIX_TEMPLATE.contains("InstallPath"));
        assert!(WIX_TEMPLATE.contains("Version"));
    }

    #[test]
    fn wix_template_uninstall_is_clean() {
        // Uninstall must remove the registry hive AND the install
        // folder. Without RemoveRegistryKey the HKLM entry would
        // linger; without RemoveFolder C:\Program Files\buffr\ would
        // remain as an empty directory.
        assert!(WIX_TEMPLATE.contains("<RemoveRegistryKey"));
        assert!(WIX_TEMPLATE.contains("<RemoveFolder"));
        assert!(WIX_TEMPLATE.contains("removeOnUninstall"));
    }

    #[test]
    fn wix_template_lists_msi_payload() {
        // The .wxs lists every required runtime file. If something is
        // dropped from this list the installed product won't run.
        assert!(WIX_TEMPLATE.contains("buffr.exe"));
        assert!(WIX_TEMPLATE.contains("buffr-helper.exe"));
        assert!(WIX_TEMPLATE.contains("libcef.dll"));
        assert!(WIX_TEMPLATE.contains("icudtl.dat"));
    }

    #[test]
    fn stage_windows_payload_lays_out_msi_tree() {
        let tmp = tempdir();
        let target = tmp.path().join("target-release");
        fs::create_dir_all(target.join("locales")).unwrap();
        fs::write(target.join("buffr.exe"), b"MZ").unwrap();
        fs::write(target.join("buffr-helper.exe"), b"MZ").unwrap();
        fs::write(target.join("libcef.dll"), b"MZ").unwrap();
        fs::write(target.join("icudtl.dat"), b"dat").unwrap();
        fs::write(target.join("resources.pak"), b"pak").unwrap();
        fs::write(target.join("v8_context_snapshot.bin"), b"bin").unwrap();
        fs::write(target.join("locales/en-US.pak"), b"locale").unwrap();

        let payload = collect_windows_payload_from(&target).unwrap();
        assert_eq!(payload.paks.len(), 1);
        assert_eq!(payload.blobs.len(), 1);

        let dest = tmp.path().join("staged");
        stage_windows_payload(&dest, &payload).unwrap();
        assert!(dest.join("buffr.exe").exists());
        assert!(dest.join("buffr-helper.exe").exists());
        assert!(dest.join("libcef.dll").exists());
        assert!(dest.join("icudtl.dat").exists());
        assert!(dest.join("resources.pak").exists());
        assert!(dest.join("v8_context_snapshot.bin").exists());
        assert!(dest.join("locales/en-US.pak").exists());
    }

    #[test]
    fn stage_dmg_creates_app_copy_and_applications_symlink() {
        let tmp = tempdir();
        let app = tmp.path().join("buffr.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/Info.plist"), "<plist/>").unwrap();
        fs::write(app.join("Contents/MacOS/buffr"), b"\x7fELF").unwrap();

        let staging = tmp.path().join("dmg-staging");
        stage_dmg(&staging, &app).unwrap();

        assert!(staging.join("buffr.app/Contents/Info.plist").exists());
        assert!(staging.join("buffr.app/Contents/MacOS/buffr").exists());
        #[cfg(unix)]
        {
            let link = staging.join("Applications");
            let meta = std::fs::symlink_metadata(&link).unwrap();
            assert!(
                meta.file_type().is_symlink(),
                "Applications must be a symlink"
            );
            let tgt = std::fs::read_link(&link).unwrap();
            assert_eq!(tgt, std::path::PathBuf::from("/Applications"));
        }
    }

    /// Minimal scratch dir helper. The xtask crate has no `tempfile`
    /// dep and we want to avoid pulling one in for one test, so we
    /// build a path under `target/tmp/` that's unique enough for
    /// parallel `cargo test` runs.
    fn tempdir() -> TempDir {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("buffr-xtask-{pid}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
