//! `cargo xtask` — build automation tasks for buffr.
//!
//! Subcommands:
//!
//! - `fetch-cef [--platform <linux64|linuxarm64|macosarm64|macosx64|windows64|windowsarm64>] [--version <X.Y.Z>]`
//!   downloads the CEF Spotify minimal binary distribution matching the `cef`
//!   crate version (147 by default) and extracts it into
//!   `vendor/cef/<platform>/`.
//! - `bundle-macos [--release] [--target <triple>]` assembles a macOS `.app`
//!   bundle (with a nested `Buffr Helper.app`) under `target/<profile>/`. Runs
//!   on Linux too; the actual runtime needs macOS, but bundle assembly is
//!   purely filesystem work and is exercised by CI on a Linux runner.
//! - `package-linux [--release] [--variant {deb,rpm,tarball,aur,all}]` produces
//!   Linux distribution artifacts under `target/dist/linux/`. Cross-builds
//!   from any Linux dev box; `dpkg-deb` and `rpmbuild` are auto-detected
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
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha1::{Digest, Sha1};

const DEFAULT_CDN: &str = "https://cef-builds.spotifycdn.com";

/// Per-phase timeouts for CEF downloads (hardening §22): the default
/// ureq agent has no timeouts at all, so a hung CDN stalls CI until the
/// runner timeout. Per-phase, not global: the archive is ~150 MB and a
/// slow-but-alive connection must be allowed to finish. `recv_body`
/// restarts per read, so it bounds "no bytes arrived" stalls, not total
/// download time.
const FETCH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const FETCH_RECV_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const FETCH_RECV_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// ureq agent for CEF index + archive fetches with the timeouts above.
fn fetch_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(FETCH_CONNECT_TIMEOUT))
        .timeout_recv_response(Some(FETCH_RECV_RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(FETCH_RECV_BODY_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

/// CEF major version we pin against the `cef` crate (148.x).
///
/// Spotify CDN entries look like `cef_binary_148.0.10+gXXXXX+chromium-...`;
/// we pick the newest entry whose version starts with this prefix. Keep the
/// major line in lockstep with the `cef` crate resolved by Cargo.toml — the
/// wrapper it compiles must talk to the same API version the vendored
/// libcef reports, or every binary aborts with "Request for unsupported CEF
/// API version" at startup.
const CEF_VERSION_PREFIX: &str = "148.";

/// Embedded `Info.plist` template for the main `Buffr.app` bundle.
const MAIN_PLIST_TEMPLATE: &str = include_str!("../templates/main.plist");
/// Embedded `Info.plist` template for the nested `Buffr Helper.app` bundle
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
///
/// `DISPLAY_NAME` is the TitleCase bundle name (`Buffr.app`, plist
/// `CFBundleName`/`CFBundleDisplayName`). `CFBundleExecutable` is `buffr`
/// (the supervisor — the entry point Launch Services starts). The browser
/// binary `buffr-app` lives alongside it in `Contents/MacOS/`.
const DISPLAY_NAME: &str = "Buffr";
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
    windowsarm64: CefPlatform,
    linux64: CefPlatform,
    linuxarm64: CefPlatform,
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
    /// Hex SHA-1 of the archive, as published in the CDN index. Verified
    /// against the bytes we actually downloaded — see [`verify_sha1`].
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
    println!(
        "        PLATFORM: linux64 (default on Linux), linuxarm64, macosarm64, macosx64,\n         windows64, windowsarm64."
    );
    println!("        PREFIX:   version prefix to match (default: {CEF_VERSION_PREFIX}).");
    println!();
    println!("    bundle-macos [--release] [--target TRIPLE]");
    println!("        Assemble Buffr.app (with nested Buffr Helper.app) under");
    println!("        target/<profile>/. Runs on Linux too (cross-bundle).");
    println!();
    println!("    package-linux [--release] [--variant VARIANT]");
    println!("        Produce Linux distribution artifacts under target/dist/linux/.");
    println!("        VARIANT: deb | rpm | tarball | aur | all (default: all).");
    println!();
    println!("    package-macos-dmg [--release]");
    println!(
        "        Wrap target/<profile>/Buffr.app into target/dist/macos/buffr-<ver>-<arch>.dmg."
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
    let index: CefIndex = fetch_agent()
        .get(&index_url)
        .call()
        .context("fetching CEF index.json")?
        .body_mut()
        .read_json()
        .context("parsing CEF index.json")?;

    let plat = match platform.as_str() {
        "linux64" => &index.linux64,
        "linuxarm64" => &index.linuxarm64,
        "macosarm64" => &index.macosarm64,
        "macosx64" => &index.macosx64,
        "windows64" => &index.windows64,
        "windowsarm64" => &index.windowsarm64,
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

    // `file.name` is remote-controlled: it comes straight out of
    // `index.json`, which we fetch over the network. Treat it as hostile
    // before it becomes a path component — an entry named
    // `../../.cargo/config.toml` would otherwise write outside
    // `vendor/cef/<platform>/`.
    let archive_name = validate_archive_name(&file.name)?;

    let archive_url = format!("{DEFAULT_CDN}/{archive_name}");
    let archive_path = vendor_dir.join(archive_name);
    download(&archive_url, &archive_path)?;

    // The ~200 MB blob we just fetched becomes `libcef.so` in every
    // shipped package, so it does not get unpacked until its digest
    // matches the one the index advertised.
    verify_sha1(&archive_path, &file.sha1)?;

    eprintln!(
        "xtask: extracting {} -> {}",
        archive_name,
        vendor_dir.display()
    );
    extract_tar_bz2(&archive_path, &vendor_dir)
        .with_context(|| format!("extracting {archive_name}"))?;

    flatten_top_level(&vendor_dir)
        .with_context(|| format!("flattening {}", vendor_dir.display()))?;

    eprintln!("xtask: done. CEF extracted at {}", vendor_dir.display());
    eprintln!("       set CEF_PATH={} to override", vendor_dir.display());
    Ok(())
}

/// Debian package architecture suffix for the host. `dpkg-deb` is
/// strict about the value matching the `Architecture:` field in
/// DEBIAN/control.
fn host_deb_arch() -> &'static str {
    host_arch_token("amd64", "arm64")
}

/// RPM `ExclusiveArch` value for the host. rpmbuild rejects the build
/// outright if the spec arch doesn't match the build host.
fn host_rpm_arch() -> &'static str {
    host_arch_token("x86_64", "aarch64")
}

/// MSI / WiX architecture token. Plumbed through `{ARCH}` in
/// `buffr.wxs` and used in the output filename.
fn host_msi_arch() -> &'static str {
    host_arch_token("x64", "arm64")
}

/// The host's architecture token from two per-arch spellings, or
/// `"unknown"` for anything neither x86_64 nor aarch64.
fn host_arch_token(x86_64: &'static str, aarch64: &'static str) -> &'static str {
    if cfg!(target_arch = "x86_64") {
        x86_64
    } else if cfg!(target_arch = "aarch64") {
        aarch64
    } else {
        "unknown"
    }
}

fn host_platform() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linuxarm64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "macosarm64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "macosx64"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "windows64"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "windowsarm64"
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

/// Cap on the downloaded CEF archive. The real minimal builds are a few
/// hundred MiB; anything beyond 1 GiB is a hostile or broken CDN response,
/// not a legitimate archive. Bound the disk use of `fetch-cef` (audit
/// §12-12: the download path was previously unbounded).
const MAX_CEF_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Cap on the total uncompressed size of an extracted CEF tree. The real
/// trees are ~1.7 GB per platform; the bzip2 stream was previously
/// unbounded, so a decompression bomb could exhaust the dev box's disk.
/// Stays below the 8 GiB−1 ceiling of the tar octal size field so the
/// guard can actually fire on a hostile header.
const MAX_CEF_EXTRACTED_BYTES: u64 = 6 * 1024 * 1024 * 1024; // 6 GiB

fn download(url: &str, dest: &Path) -> Result<()> {
    eprintln!("xtask: downloading {url}");
    let resp = fetch_agent()
        .get(url)
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
        if total > MAX_CEF_ARCHIVE_BYTES {
            // Drop the partial file so a half-downloaded archive can't be
            // picked up by a later step as if it were complete.
            let _ = fs::remove_file(dest);
            bail!(
                "download of {url} exceeded the {:.1} GiB cap — refusing to continue",
                MAX_CEF_ARCHIVE_BYTES as f64 / (1024.0 * 1024.0 * 1024.0)
            );
        }
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

/// Validate a filename taken from the remote CEF `index.json`.
///
/// The value is used both as a URL suffix and as a path component under
/// `vendor/cef/<platform>/`, so it has to be a single, plain file name.
/// Anything with a directory separator, a `..` component, a drive letter
/// or a leading `-` (which could be read as a flag by tools further down
/// the pipeline) is rejected outright rather than sanitised — a
/// well-behaved index never produces one, so a hit means the index is
/// either broken or hostile and we should stop.
fn validate_archive_name(name: &str) -> Result<&str> {
    if name.is_empty() {
        bail!("CEF index entry has an empty file name");
    }
    if name.len() > 255 {
        bail!(
            "CEF index file name is implausibly long ({} bytes)",
            name.len()
        );
    }
    if name.contains('/') || name.contains('\\') {
        bail!("CEF index file name `{name}` contains a path separator");
    }
    if name.contains("..") {
        bail!("CEF index file name `{name}` contains `..`");
    }
    if name.contains(':') {
        bail!("CEF index file name `{name}` contains `:`");
    }
    if name.starts_with('-') {
        bail!("CEF index file name `{name}` starts with `-`");
    }
    if name.chars().any(|c| c.is_control()) {
        bail!("CEF index file name contains a control character");
    }
    // Belt and braces: after all of the above, `file_name()` must be the
    // whole string. If it isn't, the OS disagrees with us about what a
    // path component is and we bail instead of guessing.
    if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
        bail!("CEF index file name `{name}` is not a plain file name");
    }
    Ok(name)
}

/// Hash `path` with SHA-1 and compare against `expected` (hex).
///
/// SHA-1 is not a defence against a determined forger, but it is the
/// only digest the Spotify CDN index publishes, and it does catch a
/// truncated / corrupted / swapped download — which is the difference
/// between shipping the CEF we asked for and shipping whatever the CDN
/// (or something between us and it) handed back.
fn verify_sha1(path: &Path, expected: &str) -> Result<()> {
    let expected = expected.trim();
    if expected.len() != 40 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "CEF index published a malformed sha1 `{expected}` for {}",
            path.display()
        );
    }

    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex_lower(&hasher.finalize());

    if !actual.eq_ignore_ascii_case(expected) {
        // Don't leave a mismatched blob lying around for the next run to
        // find and happily extract.
        let _ = fs::remove_file(path);
        bail!(
            "sha1 mismatch for {}: index said {expected}, downloaded bytes hash to {actual} \
             (archive deleted)",
            path.display()
        );
    }
    eprintln!("xtask: sha1 ok ({actual})");
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Reject a tar entry whose path would escape the extraction root.
///
/// `tar::Archive::unpack` already refuses absolute paths and `..`, but
/// it does so silently in some versions and the guarantee isn't part of
/// the crate's stable contract. We check every entry ourselves so a
/// zip-slip archive fails loudly instead of being trusted to the tar
/// crate's discretion.
fn tar_path_is_safe(path: &Path) -> bool {
    use std::path::Component;
    if path.as_os_str().is_empty() {
        return false;
    }
    path.components().all(|c| match c {
        Component::Normal(part) => !part.to_string_lossy().contains('\0'),
        Component::CurDir => true,
        // Absolute paths, `..`, and Windows prefixes (`C:`, `\\?\…`) all
        // let the entry land outside `dest`.
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    })
}

fn extract_tar_bz2(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let bz2 = bzip2::read::BzDecoder::new(file);
    let mut tar = tar::Archive::new(bz2);
    // Total uncompressed bytes across all entries. A hostile archive can
    // claim an enormous size in an entry header; bail before writing
    // anything so a decompression bomb can't fill the disk (audit §12-12).
    let mut total_bytes: u64 = 0;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        total_bytes = total_bytes.saturating_add(entry.header().size()?);
        if total_bytes > MAX_CEF_EXTRACTED_BYTES {
            bail!(
                "refusing to extract {}: uncompressed contents exceed the {:.1} GiB cap — refusing a decompression bomb",
                archive.display(),
                MAX_CEF_EXTRACTED_BYTES as f64 / (1024.0 * 1024.0 * 1024.0)
            );
        }
        if !tar_path_is_safe(&path) {
            bail!(
                "refusing to extract `{}` from {}: entry path escapes the destination",
                path.display(),
                archive.display()
            );
        }
        // `unpack_in` returns Ok(false) when *it* decided the entry was
        // unsafe to write (its own traversal / link-target checks). Our
        // check above should have caught those already, so a `false`
        // here means the two disagree — bail rather than silently ship a
        // partial tree.
        let unpacked = entry
            .unpack_in(dest)
            .with_context(|| format!("unpacking {}", path.display()))?;
        if !unpacked {
            bail!(
                "tar refused to unpack `{}` from {}",
                path.display(),
                archive.display()
            );
        }
    }
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
                // Multiple matches — give up on flattening rather than
                // guess. Benign today: the real Spotify archive has
                // exactly one `cef_binary_*` dir, and the sha1 gate
                // precedes extraction.
                return Ok(());
            }
            top = Some(path);
        }
    }
    let Some(top) = top else {
        return Ok(());
    };
    let mut moved_all = true;
    for entry in fs::read_dir(&top)? {
        let entry = entry?;
        let from = entry.path();
        let to = dir.join(entry.file_name());
        if to.exists() {
            // Collision with a pre-existing file in the vendor dir — leave
            // the archive's copy in place (skip means keep, not discard).
            // When anything is skipped, keep `top` so the skipped entries
            // survive; remove_dir_all below would delete them.
            eprintln!("xtask: flatten_top_level skipping {}", to.display());
            moved_all = false;
            continue;
        }
        fs::rename(&from, &to).or_else(|_| {
            copy_dir_recursive(&from, &to).and_then(|_| Ok(fs::remove_dir_all(&from)?))
        })?;
    }
    // Only delete the archive dir when every entry moved out of it; if
    // one was skipped, its copy is still inside and must survive.
    if moved_all {
        let _ = fs::remove_dir_all(&top);
    }
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
        // `fs::copy` preserves Unix mode bits, so a 0o755 binary
        // (e.g. Contents/MacOS/buffr) stays executable through DMG
        // staging. The previous `io::copy(&mut File::open, &mut
        // File::create)` lost +x, shipping a non-executable bundle.
        fs::copy(from, to)?;
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
    eprintln!(
        "xtask: building buffr (supervisor) + buffr-app (browser) + buffr-helper ({profile})"
    );
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&workspace)
        .arg("build")
        .arg("-p")
        .arg("buffr")
        .arg("-p")
        .arg("buffr-app")
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

    let supervisor_bin = target_dir.join("buffr");
    let buffr_app_bin = target_dir.join("buffr-app");
    let helper_bin = target_dir.join("buffr-helper");
    if !supervisor_bin.exists() {
        bail!("expected `{}` after build", supervisor_bin.display());
    }
    if !buffr_app_bin.exists() {
        bail!("expected `{}` after build", buffr_app_bin.display());
    }
    if !helper_bin.exists() {
        bail!("expected `{}` after build", helper_bin.display());
    }

    // 2. Resolve framework dir.
    let framework_dir = resolve_framework_dir(&workspace, parsed.platform.as_deref())?;

    // 3. Stage bundle (idempotent — wipe + rebuild).
    let app_dir = target_dir.join("Buffr.app");
    if app_dir.exists() {
        fs::remove_dir_all(&app_dir)
            .with_context(|| format!("removing existing {}", app_dir.display()))?;
    }

    let version = workspace_version(&workspace)?;
    stage_bundle(
        &app_dir,
        &supervisor_bin,
        &buffr_app_bin,
        &helper_bin,
        &framework_dir,
        version.as_str(),
    )?;

    eprintln!();
    eprintln!("xtask: Buffr.app staged at {}", app_dir.display());
    eprintln!("xtask: For ad-hoc local signing:");
    eprintln!(
        "           codesign --force --deep --sign - {}",
        app_dir.display()
    );
    eprintln!("xtask: For distribution: see docs/site/macos-signing.md (TODO)");
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

    // On macOS hosts default to the host architecture; on cross-builds
    // (Linux runner staging the bundle for CI parity) default to
    // arm64 since that's the dominant Apple target.
    let host_default = if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macosx64"
    } else {
        "macosarm64"
    };
    let platform = platform_override.unwrap_or(host_default);
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
///
/// `supervisor_bin` is the `buffr` watchdog — it becomes `CFBundleExecutable`
/// (the entry point Launch Services starts). `buffr_app_bin` is the browser
/// binary and lives as a sibling in `Contents/MacOS/`. The supervisor's
/// child-resolution logic (`current_exe → parent → "buffr-app"`) finds it
/// automatically because both live in the same directory.
fn stage_bundle(
    app_dir: &Path,
    supervisor_bin: &Path,
    buffr_app_bin: &Path,
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

    // Supervisor (CFBundleExecutable = "buffr") — the entrypoint Launch Services
    // starts. It spawns buffr-app as its supervised child.
    let main_exec = macos.join("buffr");
    copy_file_executable(supervisor_bin, &main_exec)?;

    // Browser binary. The supervisor finds it via current_exe() → parent() →
    // "buffr-app", which resolves to Contents/MacOS/buffr-app alongside itself.
    let browser_exec = macos.join("buffr-app");
    copy_file_executable(buffr_app_bin, &browser_exec)?;

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
        let bundle_name = format!("{} Helper{}.app", DISPLAY_NAME, flavor.suffix);
        let exec_name = format!("{} Helper{}", DISPLAY_NAME, flavor.suffix);
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

/// Helper-bundle flavors shipped inside `Buffr.app/Contents/Frameworks/`.
///
/// `suffix` is appended to `"Buffr Helper"` for the bundle + executable
/// names — `""` is the catch-all helper (`Buffr Helper.app`), `" (GPU)"`
/// becomes `Buffr Helper (GPU).app`, etc. Apple requires every nested
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
        .replace("{NAME}", DISPLAY_NAME)
        .replace("{VERSION}", version)
        .replace("{BUNDLE_ID_MAIN}", BUNDLE_ID_MAIN)
        // CFBundleExecutable is the supervisor; it spawns buffr-app as its child.
        .replace("{EXECUTABLE}", "buffr")
        .replace("{COPYRIGHT}", COPYRIGHT)
}

fn render_helper_plist(flavor: &HelperFlavor, version: &str, executable: &str) -> String {
    flavor
        .plist_template
        .replace("{NAME}", &format!("{DISPLAY_NAME} Helper{}", flavor.suffix))
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
    set_executable(dest)?;
    Ok(())
}

// ----------------------------- package-linux -----------------------------

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
/// Embedded RPM spec template (`{VERSION}` substituted).
const RPM_SPEC_TEMPLATE: &str = include_str!("../templates/buffr.spec.in");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxVariant {
    Deb,
    Rpm,
    Tarball,
    Aur,
    All,
}

impl LinuxVariant {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "deb" => Ok(Self::Deb),
            "rpm" => Ok(Self::Rpm),
            "tarball" | "tar" => Ok(Self::Tarball),
            "aur" => Ok(Self::Aur),
            "all" => Ok(Self::All),
            other => bail!("unknown --variant `{other}` (deb|rpm|tarball|aur|all)"),
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
    eprintln!(
        "xtask: building buffr (supervisor) + buffr-app (browser) + buffr-helper ({profile})"
    );
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&workspace)
        .arg("build")
        .arg("-p")
        .arg("buffr")
        .arg("-p")
        .arg("buffr-app")
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
    //    workspace version even if the user only asked for the .deb.
    if matches!(parsed.variant, LinuxVariant::Aur | LinuxVariant::All) {
        write_pkgbuild(&workspace, &version)?;
    }

    if matches!(parsed.variant, LinuxVariant::Deb | LinuxVariant::All) {
        build_deb(&workspace, &dist_dir, &target_dir, &payload, &version)?;
    }

    if matches!(parsed.variant, LinuxVariant::Rpm | LinuxVariant::All) {
        build_rpm(&workspace, &dist_dir, &target_dir, &payload, &version)?;
    }

    if matches!(parsed.variant, LinuxVariant::Tarball | LinuxVariant::All) {
        build_tarball(&workspace, &dist_dir, &target_dir, &payload, &version)?;
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
    /// Absolute path to the `buffr` supervisor binary.
    buffr: PathBuf,
    /// Absolute path to the `buffr-app` browser binary.
    buffr_app: PathBuf,
    /// Absolute path to the `buffr-helper` binary.
    helper: PathBuf,
    /// Absolute path to `libcef.so` (Linux dist).
    libcef: PathBuf,
    /// Every other .so the CEF binary distribution ships next to libcef.so:
    /// libEGL.so + libGLESv2.so (ANGLE — needed for GPU rendering),
    /// libvk_swiftshader.so + libvulkan.so.1 (SwiftShader Vulkan fallback).
    /// Missing any of these → "Failed to load GLES library" at startup +
    /// GPU process crash + wgpu no-adapter → app exits cleanly. Required
    /// at runtime — `target/<profile>/` should have all of them after
    /// `cargo build` thanks to buffr-core's build.rs staging.
    runtime_libs: Vec<PathBuf>,
    /// Absolute paths to `*.pak` files.
    paks: Vec<PathBuf>,
    /// Absolute paths to `*.dat` / `*.bin` blobs (icudtl, snapshot).
    blobs: Vec<PathBuf>,
    /// JSON metadata CEF ships in Release/ (vk_swiftshader_icd.json).
    /// Sits next to libvk_swiftshader.so; required for SwiftShader's
    /// Vulkan ICD discovery.
    jsons: Vec<PathBuf>,
    /// Absolute path to the `locales/` directory.
    locales: PathBuf,
}

fn collect_runtime_payload(target_dir: &Path) -> Result<RuntimePayload> {
    let buffr = target_dir.join("buffr");
    let buffr_app = target_dir.join("buffr-app");
    let helper = target_dir.join("buffr-helper");
    let libcef = target_dir.join("libcef.so");
    let locales = target_dir.join("locales");

    if !buffr.exists() {
        bail!("expected `{}` after build", buffr.display());
    }
    if !buffr_app.exists() {
        bail!("expected `{}` after build", buffr_app.display());
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
    let mut jsons = Vec::new();
    let mut runtime_libs = Vec::new();
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
        } else if name.ends_with(".json") {
            jsons.push(path);
        } else if name == "libEGL.so"
            || name == "libGLESv2.so"
            || name == "libvk_swiftshader.so"
            || name == "libvulkan.so.1"
        {
            runtime_libs.push(path);
        }
    }
    paks.sort();
    blobs.sort();
    jsons.sort();
    runtime_libs.sort();

    if runtime_libs.is_empty() {
        bail!(
            "expected runtime .so files (libEGL/libGLESv2/libvk_swiftshader/libvulkan.so.1) \
             in {} — buffr-core build.rs should have staged them from CEF binary distribution. \
             Did you `cargo xtask fetch-cef`?",
            target_dir.display()
        );
    }

    Ok(RuntimePayload {
        buffr,
        buffr_app,
        helper,
        libcef,
        runtime_libs,
        jsons,
        paks,
        blobs,
        locales,
    })
}

/// Stage the runtime payload (binaries + CEF runtime tree) inside
/// `dest`. Used by the Debian package builder (`/opt/buffr/`).
///
/// Both `buffr` (supervisor, the Linux entrypoint) and `buffr-app`
/// (browser binary) are installed. `buffr-helper` handles CEF subprocesses.
fn stage_payload(dest: &Path, payload: &RuntimePayload) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    copy_file_executable(&payload.buffr, &dest.join("buffr"))?;
    copy_file_executable(&payload.buffr_app, &dest.join("buffr-app"))?;
    copy_file_executable(&payload.helper, &dest.join("buffr-helper"))?;
    copy_into_dir(&payload.libcef, dest)?;
    for lib in &payload.runtime_libs {
        copy_into_dir(lib, dest)?;
    }
    for pak in &payload.paks {
        copy_into_dir(pak, dest)?;
    }
    for blob in &payload.blobs {
        copy_into_dir(blob, dest)?;
    }
    for json in &payload.jsons {
        copy_into_dir(json, dest)?;
    }
    let locales_dest = dest.join("locales");
    let _ = fs::remove_dir_all(&locales_dest);
    copy_dir_recursive(&payload.locales, &locales_dest)?;
    Ok(())
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
    let arch = host_deb_arch();
    let control = DEB_CONTROL_TEMPLATE
        .replace("{VERSION}", version)
        .replace("{ARCH}", arch);
    fs::write(debian.join("control"), control)?;
    let postinst = debian.join("postinst");
    fs::write(&postinst, DEB_POSTINST)?;
    set_executable(&postinst)?;
    let prerm = debian.join("prerm");
    fs::write(&prerm, DEB_PRERM)?;
    set_executable(&prerm)?;

    // Invoke dpkg-deb if available. Otherwise leave the staging tree
    // and let CI pick it up.
    let out = dist_dir.join(format!("buffr-{version}-{arch}.deb"));
    if !which("dpkg-deb") {
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

// ---------------------------- .rpm --------------------------------------

fn build_rpm(
    workspace: &Path,
    dist_dir: &Path,
    target_dir: &Path,
    payload: &RuntimePayload,
    version: &str,
) -> Result<()> {
    if !which("rpmbuild") {
        eprintln!("xtask: rpmbuild not on PATH; skipping rpm build");
        return Ok(());
    }

    let rpmroot = target_dir.join("buffr-rpm");
    if rpmroot.exists() {
        fs::remove_dir_all(&rpmroot)
            .with_context(|| format!("wiping existing {}", rpmroot.display()))?;
    }
    let sources = rpmroot.join("SOURCES");
    let specs = rpmroot.join("SPECS");
    let rpms = rpmroot.join("RPMS");
    let buildroot = rpmroot.join("BUILDROOT");
    fs::create_dir_all(sources.join("payload"))?;
    fs::create_dir_all(&specs)?;
    fs::create_dir_all(&rpms)?;
    fs::create_dir_all(&buildroot)?;

    // Stage the runtime payload at SOURCES/payload/ — the spec %install
    // hook copies from %{_sourcedir}/payload/ into %{buildroot}/opt/buffr.
    stage_payload(&sources.join("payload"), payload)?;

    // .desktop + icon as plain SOURCES (the spec installs them into
    // /usr/share/applications and /usr/share/icons/.../apps).
    fs::write(sources.join("buffr.desktop"), DESKTOP_TEMPLATE)?;
    let icon_src = workspace.join("pkg/buffr.png");
    if icon_src.exists() {
        fs::copy(&icon_src, sources.join("buffr.png"))?;
    }

    // Render and write the spec. ExclusiveArch must match the build
    // host or rpmbuild aborts with "package x86_64 not in arches".
    let arch = host_rpm_arch();
    let spec = RPM_SPEC_TEMPLATE
        .replace("{VERSION}", version)
        .replace("{ARCH}", arch);
    let spec_path = specs.join("buffr.spec");
    fs::write(&spec_path, spec)?;

    eprintln!("xtask: rpmbuild -bb -> {}", rpms.join(arch).display());
    let topdir = rpmroot.canonicalize().unwrap_or(rpmroot.clone());
    let status = Command::new("rpmbuild")
        .arg("-bb")
        .arg("--define")
        .arg(format!("_topdir {}", topdir.display()))
        .arg(&spec_path)
        .status()
        .context("spawning rpmbuild")?;
    if !status.success() {
        eprintln!("xtask: warning — rpmbuild exited {status:?}");
        return Ok(());
    }

    // rpmbuild deposits at RPMS/<arch>/buffr-<ver>-1.<arch>.rpm — copy
    // into target/dist/linux/ with the same name pattern as the deb.
    let arch_dir = rpms.join(arch);
    let mut found = None;
    if arch_dir.exists() {
        for entry in fs::read_dir(&arch_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("rpm") {
                found = Some(path);
                break;
            }
        }
    }
    let Some(src) = found else {
        eprintln!(
            "xtask: warning — rpmbuild produced no .rpm under {}",
            arch_dir.display()
        );
        return Ok(());
    };

    let out = dist_dir.join(format!("buffr-{version}-{arch}.rpm"));
    fs::copy(&src, &out).with_context(|| format!("copy {} -> {}", src.display(), out.display()))?;
    eprintln!("xtask: rpm written to {}", out.display());
    Ok(())
}

// ---------------------------- portable tarball -------------------------

/// Plain `.tar.gz` of the runtime tree (binaries + CEF + pak/locales).
/// Sandbox packagers (Flatpak, Snap, custom installers) consume this
/// instead of unpacking the .deb so they don't have to deal with
/// Debian metadata or the postinst's /usr/local/bin symlink.
///
/// Layout inside the tarball:
///
/// ```text
/// buffr-<ver>-<arch>/
///   buffr               (supervisor — Linux default entrypoint)
///   buffr-app           (browser binary)
///   buffr-helper        (CEF subprocess helper)
///   libcef.so
///   *.pak / *.dat / *.bin
///   locales/...
///   buffr.desktop       (XDG desktop entry; AUR/manual installs use it)
///   buffr.png           (512x512 icon; same)
/// ```
fn build_tarball(
    workspace: &Path,
    dist_dir: &Path,
    target_dir: &Path,
    payload: &RuntimePayload,
    version: &str,
) -> Result<()> {
    let arch = host_rpm_arch();
    let stage_name = format!("buffr-{version}-{arch}");
    let staging = target_dir.join("buffr-tarball").join(&stage_name);
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("wiping existing {}", staging.display()))?;
    }
    stage_payload(&staging, payload)?;

    // Bundle XDG metadata so packagers (-bin AUR, manual installs) can
    // wire `buffr.desktop` + icon without fetching extra sources. The
    // .deb/.rpm builders install these from the workspace directly.
    fs::write(staging.join("buffr.desktop"), DESKTOP_TEMPLATE)?;
    let icon_src = workspace.join("pkg/buffr.png");
    if icon_src.exists() {
        fs::copy(&icon_src, staging.join("buffr.png"))?;
    }

    // Shell out to GNU tar — every Linux runner ships it. Avoids a
    // Rust gzip dep just for this one call site.
    if !which("tar") {
        eprintln!("xtask: tar not on PATH; skipping tarball build");
        return Ok(());
    }
    let out = dist_dir.join(format!("{stage_name}.tar.gz"));
    let parent = staging
        .parent()
        .ok_or_else(|| anyhow!("staging has no parent"))?;
    eprintln!("xtask: tar czf -> {}", out.display());
    let status = Command::new("tar")
        .arg("-C")
        .arg(parent)
        .arg("-czf")
        .arg(&out)
        .arg(&stage_name)
        .status()
        .context("spawning tar")?;
    if !status.success() {
        eprintln!("xtask: warning — tar exited {status:?}");
        return Ok(());
    }
    eprintln!("xtask: tarball written to {}", out.display());
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

    let app_dir = parsed.app.unwrap_or_else(|| target_dir.join("Buffr.app"));
    if !app_dir.exists() {
        bail!(
            "no Buffr.app at {} — run `cargo xtask bundle-macos{}` first",
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
    eprintln!("           xattr -d com.apple.quarantine /Applications/Buffr.app");
    eprintln!("       Signing + notarization land alongside docs/site/macos-signing.md.");
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
///   Buffr.app/                  (copy of the bundle)
///   Applications -> /Applications  (symlink, drag-target)
/// ```
///
/// `.background.png` and `.DS_Store` are intentionally not generated —
/// they only matter for visual layout when mounted, and producing them
/// faithfully needs an `osascript` + a mounted volume on macOS. Once
/// signing lands the post-Phase-6 release pipeline can layer those on.
fn stage_dmg(staging: &Path, app_dir: &Path) -> Result<()> {
    fs::create_dir_all(staging).with_context(|| format!("creating {}", staging.display()))?;

    // Copy Buffr.app into the staging tree. We copy rather than symlink
    // so hdiutil sees a self-contained directory.
    let dest_app = staging.join("Buffr.app");
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

/// Locate `tool` on `PATH`, returning the full path to the executable.
///
/// Implemented in Rust rather than by shelling out to `which(1)`: that
/// binary does not exist on a stock Windows host (`where` is a `cmd`
/// builtin, not an executable on `PATH`), so the old version reported
/// *every* tool as missing on any Windows runner without
/// Git-for-Windows' `usr/bin` on `PATH` — which silently degraded
/// `package-windows-msi` into a payload-staging no-op.
///
/// Windows semantics: a bare name is tried against every extension in
/// `PATHEXT` (defaulting to the usual `.COM;.EXE;.BAT;.CMD`) as well as
/// verbatim, so `which_path("candle")` finds `candle.exe`.
fn which_path(tool: &str) -> Option<PathBuf> {
    // An explicit path (`./foo`, `C:\wix\candle.exe`) bypasses the PATH
    // walk entirely, matching what `Command::new` would do with it.
    if tool.contains('/') || (cfg!(windows) && tool.contains('\\')) {
        let p = PathBuf::from(tool);
        return is_executable_file(&p).then_some(p);
    }

    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in exe_candidates(tool) {
            let full = dir.join(&candidate);
            if is_executable_file(&full) {
                return Some(full);
            }
        }
    }
    None
}

/// File-name spellings to try for `tool`, in priority order.
fn exe_candidates(tool: &str) -> Vec<String> {
    if !cfg!(windows) {
        return vec![tool.to_string()];
    }
    let mut out = Vec::new();
    // A name that already carries a known extension is used verbatim
    // first; otherwise PATHEXT spellings win over the bare name so we
    // don't match an extensionless sibling on Windows.
    let pathext =
        env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WSF;.MSC".into());
    let has_ext = pathext
        .split(';')
        .filter(|e| !e.is_empty())
        .any(|e| tool.to_ascii_lowercase().ends_with(&e.to_ascii_lowercase()));
    if has_ext {
        out.push(tool.to_string());
    }
    for ext in pathext.split(';').filter(|e| !e.is_empty()) {
        out.push(format!("{tool}{ext}"));
    }
    out.push(tool.to_string());
    out.dedup();
    out
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `which_path(tool).is_some()` — the presence-only form used by the
/// "is this optional packaging tool available?" checks.
fn which(tool: &str) -> bool {
    which_path(tool).is_some()
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
    let arch = host_msi_arch();
    let wxs = render_wix(&version, "buffr", arch);
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
    // Two staging dirs. wxs <File> Source paths reference these.
    let bin_payload_dir = dist_dir.join("payload-bin");
    let cef_payload_dir = dist_dir.join("payload-cef");
    for d in [&bin_payload_dir, &cef_payload_dir] {
        if d.exists() {
            fs::remove_dir_all(d).with_context(|| format!("wiping {}", d.display()))?;
        }
    }
    stage_windows_payload(&bin_payload_dir, &cef_payload_dir, &payload)?;

    // 3. Resolve candle + light + heat. All three must exist; a partial
    //    WiX 3 install is treated as missing. `which_path` already tries
    //    the PATHEXT spellings on Windows, so a bare name finds
    //    `candle.exe`.
    //
    //    This is a hard failure, not a warning: the caller asked for an
    //    .msi, and returning Ok() here left CI's "validate msi exists"
    //    step to report the absence as an unrelated error several steps
    //    later.
    let missing: Vec<&str> = ["candle", "light", "heat"]
        .into_iter()
        .filter(|t| !which(t))
        .collect();
    if !missing.is_empty() {
        bail!(
            "WiX 3 toolset incomplete: {} not found on PATH. The payload and \
             .wxs have been staged at {}; install WiX 3 (or the v3 build of \
             WiX 4) and re-run, or use the CI windows-package job.",
            missing.join(", "),
            dist_dir.display()
        );
    }
    let heat_exe = which_path("heat").expect("heat resolved above");
    let candle_exe = which_path("candle").expect("candle resolved above");
    let light_exe = which_path("light").expect("light resolved above");

    // 4. heat.exe — harvest the CEF runtime tree into a ComponentGroup.
    //    `-srd` suppresses the root-dir element so files install
    //    directly under INSTALLFOLDER. `-var var.CefPayloadDir` makes
    //    candle resolve $(var.CefPayloadDir) at compile time so we
    //    don't bake an absolute path into the wxs.
    let cef_wxs_path = dist_dir.join("cef-payload.wxs");
    eprintln!("xtask: heat -> {}", cef_wxs_path.display());
    let status = Command::new(&heat_exe)
        .arg("dir")
        .arg(&cef_payload_dir)
        .arg("-nologo")
        .arg("-gg") // generate guids now (deterministic)
        .arg("-srd") // suppress root directory
        .arg("-sreg") // skip registry harvest
        .arg("-sfrag") // single fragment
        .arg("-cg")
        .arg("CefRuntime")
        .arg("-dr")
        .arg("INSTALLFOLDER")
        .arg("-var")
        .arg("var.CefPayloadDir")
        .arg("-out")
        .arg(&cef_wxs_path)
        .current_dir(&dist_dir)
        .status()
        .context("spawning heat")?;
    if !status.success() {
        bail!("heat exited {status:?}");
    }

    // 5. Drive candle + light. Both wxs files compile together; light
    //    links both wixobj into a single MSI.
    let wixobj = dist_dir.join("buffr.wixobj");
    let cef_wixobj = dist_dir.join("cef-payload.wixobj");
    eprintln!("xtask: candle -> {}", wixobj.display());
    let status = Command::new(&candle_exe)
        .arg("-arch")
        .arg(arch)
        .arg(format!("-dCefPayloadDir={}", cef_payload_dir.display()))
        .arg("-o")
        .arg(format!("{}\\", dist_dir.display()))
        .arg(&wxs_path)
        .arg(&cef_wxs_path)
        .current_dir(&dist_dir)
        .status()
        .context("spawning candle")?;
    if !status.success() {
        bail!("candle exited {status:?}");
    }

    let msi_path = dist_dir.join(format!("buffr-{version}-{arch}.msi"));
    eprintln!("xtask: light -> {}", msi_path.display());
    // ICE38/ICE64/ICE91 all fire because the install is per-user. The
    // strict reading wants every File component to have an HKCU
    // RegistryValue KeyPath and every Directory listed in the
    // RemoveFile table — impractical for the heat-harvested CEF
    // fragment (50+ files, nested locales/ tree). Suppress: install
    // scope is already perUser via the Package element and the
    // INSTALLFOLDER component carries a `RemoveFolder` for the root.
    let status = Command::new(&light_exe)
        .arg("-o")
        .arg(&msi_path)
        .arg("-sice:ICE38")
        .arg("-sice:ICE64")
        .arg("-sice:ICE91")
        .arg(&wixobj)
        .arg(&cef_wixobj)
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
         (see docs/site/windows-packaging.md)."
    );
    Ok(())
}

#[derive(Debug)]
struct WindowsPayload {
    supervisor_exe: PathBuf,
    buffr_exe: PathBuf,
    helper_exe: PathBuf,
    /// Every .dll under target/<profile>/. Includes libcef.dll plus the
    /// CEF runtime helpers (chrome_elf, libEGL, libGLESv2, vk_swiftshader,
    /// vulkan-1, d3dcompiler_47, ...). All required at process start —
    /// missing any one trips the Windows loader before main runs.
    dlls: Vec<PathBuf>,
    /// vk_swiftshader_icd.json (Vulkan ICD descriptor) plus any future
    /// .json metadata CEF ships in Release/.
    jsons: Vec<PathBuf>,
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
        let supervisor_exe = dir.join("buffr.exe");
        let buffr_exe = dir.join("buffr-app.exe");
        let helper_exe = dir.join("buffr-helper.exe");
        let libcef_dll = dir.join("libcef.dll");
        if supervisor_exe.exists()
            && buffr_exe.exists()
            && helper_exe.exists()
            && libcef_dll.exists()
        {
            return collect_windows_payload_from(dir.as_path());
        }
    }

    bail!(
        "no Windows payload found under any of {} \
         — build via `cargo build --target x86_64-pc-windows-msvc --release` (Windows host) \
         or `cargo build --target x86_64-pc-windows-gnu --release` (Linux cross) first.\n\
         Cross-build prerequisites: see docs/site/windows-packaging.md",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn collect_windows_payload_from(dir: &Path) -> Result<WindowsPayload> {
    let supervisor_exe = dir.join("buffr.exe");
    let buffr_exe = dir.join("buffr-app.exe");
    let helper_exe = dir.join("buffr-helper.exe");
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

    let mut dlls = Vec::new();
    let mut jsons = Vec::new();
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
        // buffr.exe (supervisor) + buffr-app.exe + buffr-helper.exe are
        // tracked separately so the Shortcut Target can reference them by id.
        if name == "buffr.exe" || name == "buffr-app.exe" || name == "buffr-helper.exe" {
            continue;
        }
        if name.ends_with(".dll") {
            dlls.push(path);
        } else if name.ends_with(".json") {
            jsons.push(path);
        } else if name.ends_with(".pak") {
            paks.push(path);
        } else if name.ends_with(".bin") {
            blobs.push(path);
        }
    }
    dlls.sort();
    jsons.sort();
    paks.sort();
    blobs.sort();

    if !dlls
        .iter()
        .any(|p| p.file_name().map(|n| n == "libcef.dll").unwrap_or(false))
    {
        bail!(
            "missing `libcef.dll` under {} — buffr-core build.rs should have staged it",
            dir.display()
        );
    }

    Ok(WindowsPayload {
        supervisor_exe,
        buffr_exe,
        helper_exe,
        dlls,
        jsons,
        icudtl,
        paks,
        blobs,
        locales,
    })
}

/// Two-dir staging: buffr.exe (supervisor) + buffr-app.exe + buffr-helper.exe
/// go to `bin_dest` (the hand-rolled wxs Components reference these explicitly
/// so the Shortcut Target=`[#filSupervisorExe]` resolves), everything else
/// goes to `cef_dest` (heat.exe harvests it into a generated ComponentGroup).
fn stage_windows_payload(bin_dest: &Path, cef_dest: &Path, p: &WindowsPayload) -> Result<()> {
    fs::create_dir_all(bin_dest)?;
    copy_into_dir(&p.supervisor_exe, bin_dest)?;
    copy_into_dir(&p.buffr_exe, bin_dest)?;
    copy_into_dir(&p.helper_exe, bin_dest)?;

    fs::create_dir_all(cef_dest)?;
    for dll in &p.dlls {
        copy_into_dir(dll, cef_dest)?;
    }
    for json in &p.jsons {
        copy_into_dir(json, cef_dest)?;
    }
    copy_into_dir(&p.icudtl, cef_dest)?;
    for pak in &p.paks {
        copy_into_dir(pak, cef_dest)?;
    }
    for blob in &p.blobs {
        copy_into_dir(blob, cef_dest)?;
    }
    let locales_dest = cef_dest.join("locales");
    let _ = fs::remove_dir_all(&locales_dest);
    copy_dir_recursive(&p.locales, &locales_dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_skipped_entries_survive_without_deleting_the_top_dir() {
        // The bug: when a destination already exists, the entry is left
        // inside `top`, and the unconditional remove_dir_all(&top) then
        // deleted it — a skipped entry was discarded, not kept.
        let tmp = std::env::temp_dir().join(format!("buffr-flatten-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Pre-existing collision in the vendor dir.
        let vendor = tmp.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("README.txt"), "stale").unwrap();
        // The archive's top dir with one colliding and one fresh entry.
        let top = vendor.join("cef_binary_99_linux64");
        std::fs::create_dir_all(top.join("Release")).unwrap();
        std::fs::write(top.join("README.txt"), "fresh").unwrap();
        flatten_top_level(&vendor).expect("flatten completes");
        // The stale file must have survived — the fix, not the bug.
        assert_eq!(
            std::fs::read_to_string(vendor.join("README.txt")).unwrap(),
            "stale",
            "pre-existing vendor file must win over the archive copy"
        );
        // Release moved out; the top dir stays (it still holds the
        // skipped README.txt, and deleting it would destroy that file).
        assert!(vendor.join("Release").is_dir());
        assert!(top.is_dir(), "top dir survives when an entry was skipped");
        assert!(
            top.join("README.txt").exists(),
            "skipped entry intact inside top"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn flatten_moving_everything_removes_the_top_dir() {
        let tmp = std::env::temp_dir().join(format!("buffr-flatten-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let vendor = tmp.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        let top = vendor.join("cef_binary_99_linux64");
        std::fs::create_dir_all(top.join("Release")).unwrap();
        flatten_top_level(&vendor).expect("flatten completes");
        assert!(vendor.join("Release").is_dir());
        assert!(!top.exists(), "top dir removed when every entry moved out");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Spotify ships minimal builds at
    /// `<cdn>/cef_binary_<version>_<platform>_minimal.tar.bz2`. The full
    /// filename is normally read out of `index.json`; we expose the
    /// generator separately so unit tests can lock the URL pattern down
    /// without hitting the network.
    fn cef_minimal_url(cdn: &str, platform: &str, cef_version: &str) -> String {
        format!("{cdn}/cef_binary_{cef_version}_{platform}_minimal.tar.bz2")
    }

    #[test]
    fn extract_rejects_oversized_archive() {
        use std::io::Write as _;
        // A hostile tar header claiming 7 GiB of content (no actual data).
        // Extraction must bail on the header before writing anything.
        let mut header = [0u8; 512];
        header[..5].copy_from_slice(b"huge!");
        header[100..108].copy_from_slice(b"0000644\0");
        let seven_gib = 7u64 * 1024 * 1024 * 1024;
        header[124..136].copy_from_slice(format!("{seven_gib:011o}\0").as_bytes());
        header[156] = b'0';
        // ustar checksum: sum of all bytes with the checksum field
        // treated as spaces, stored as 6 octal digits, NUL, space.
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
        let mut compressed = Vec::new();
        {
            let mut enc =
                bzip2::write::BzEncoder::new(&mut compressed, bzip2::Compression::default());
            enc.write_all(&header).unwrap();
            enc.finish().unwrap();
        }
        let dir =
            std::env::temp_dir().join(format!("xtask-extract-bomb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("bomb.tar.bz2");
        fs::write(&archive_path, &compressed).unwrap();
        let dest = dir.join("out");
        let err = extract_tar_bz2(&archive_path, &dest).unwrap_err();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            err.to_string().contains("cap"),
            "expected a size-cap error, got: {err}"
        );
    }

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
        // CFBundleDisplayName / CFBundleName use the TitleCase display name.
        assert!(s.contains("<string>Buffr</string>"));
        // CFBundleExecutable is the supervisor binary name (the entrypoint
        // Launch Services starts; it spawns buffr-app as its child).
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
        let s = render_helper_plist(&base, "1.2.3", "Buffr Helper");
        assert!(s.contains("<string>sh.kryptic.buffr.helper</string>"));
        assert!(s.contains("<string>Buffr Helper</string>"));
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
            let exec = format!("Buffr Helper{suffix}");
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

        let supervisor_bin = tmp.path().join("buffr");
        let buffr_app_bin = tmp.path().join("buffr-app");
        let helper_bin = tmp.path().join("buffr-helper");
        fs::write(&supervisor_bin, b"#!/bin/sh\necho buffr\n").unwrap();
        fs::write(&buffr_app_bin, b"#!/bin/sh\necho buffr-app\n").unwrap();
        fs::write(&helper_bin, b"#!/bin/sh\necho helper\n").unwrap();

        let app_dir = tmp.path().join("Buffr.app");
        stage_bundle(
            &app_dir,
            &supervisor_bin,
            &buffr_app_bin,
            &helper_bin,
            &fw,
            "9.9.9",
        )
        .unwrap();

        assert!(app_dir.join("Contents/Info.plist").exists());
        assert!(app_dir.join("Contents/PkgInfo").exists());
        // Supervisor is CFBundleExecutable — the entrypoint Launch Services starts.
        assert!(app_dir.join("Contents/MacOS/buffr").exists());
        // Browser binary lives alongside the supervisor.
        assert!(app_dir.join("Contents/MacOS/buffr-app").exists());
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
            let helper_app = app_dir.join(format!("Contents/Frameworks/Buffr Helper{suffix}.app"));
            assert!(
                helper_app.join("Contents/Info.plist").exists(),
                "missing Info.plist for Buffr Helper{suffix}.app"
            );
            assert!(
                helper_app.join("Contents/PkgInfo").exists(),
                "missing PkgInfo for Buffr Helper{suffix}.app"
            );
            assert!(
                helper_app
                    .join(format!("Contents/MacOS/Buffr Helper{exec_suffix}"))
                    .exists(),
                "missing executable for Buffr Helper{suffix}.app"
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
        assert_eq!(LinuxVariant::parse("deb").unwrap(), LinuxVariant::Deb);
        assert_eq!(LinuxVariant::parse("rpm").unwrap(), LinuxVariant::Rpm);
        assert_eq!(
            LinuxVariant::parse("tarball").unwrap(),
            LinuxVariant::Tarball
        );
        assert_eq!(LinuxVariant::parse("tar").unwrap(), LinuxVariant::Tarball);
        assert_eq!(LinuxVariant::parse("aur").unwrap(), LinuxVariant::Aur);
        assert_eq!(LinuxVariant::parse("all").unwrap(), LinuxVariant::All);
    }

    #[test]
    fn rpm_spec_template_has_required_fields() {
        let rendered = RPM_SPEC_TEMPLATE
            .replace("{VERSION}", "1.2.3")
            .replace("{ARCH}", "x86_64");
        assert!(rendered.contains("Name:           buffr"));
        assert!(rendered.contains("Version:        1.2.3"));
        assert!(rendered.contains("ExclusiveArch:  x86_64"));
        assert!(!rendered.contains("{VERSION}"));
        assert!(!rendered.contains("{ARCH}"));
        // post-install symlinks for all buffr binaries.
        assert!(rendered.contains("ln -sf /opt/buffr/buffr /usr/local/bin/buffr"));
        assert!(rendered.contains("ln -sf /opt/buffr/buffr-app /usr/local/bin/buffr-app"));
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
        let rendered = DEB_CONTROL_TEMPLATE
            .replace("{VERSION}", "1.2.3")
            .replace("{ARCH}", "amd64");
        assert!(rendered.contains("Version: 1.2.3"));
        assert!(rendered.contains("Package: buffr"));
        assert!(rendered.contains("Architecture: amd64"));
        assert!(!rendered.contains("{VERSION}"));
        assert!(!rendered.contains("{ARCH}"));
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
    fn desktop_template_has_required_keys() {
        // Keep the minimum keys that LXQt / GNOME / KDE all parse.
        assert!(DESKTOP_TEMPLATE.contains("[Desktop Entry]"));
        assert!(DESKTOP_TEMPLATE.contains("Name=Buffr"));
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
        fs::write(target.join("buffr-app"), b"#!/bin/sh\n").unwrap();
        fs::write(target.join("buffr-helper"), b"#!/bin/sh\n").unwrap();
        fs::write(target.join("libcef.so"), b"\x7fELF").unwrap();
        // CEF runtime .so files staged next to libcef.so in real builds.
        // Required for GPU init (ANGLE EGL/GLES + SwiftShader Vulkan).
        fs::write(target.join("libEGL.so"), b"\x7fELF").unwrap();
        fs::write(target.join("libGLESv2.so"), b"\x7fELF").unwrap();
        fs::write(target.join("libvk_swiftshader.so"), b"\x7fELF").unwrap();
        fs::write(target.join("libvulkan.so.1"), b"\x7fELF").unwrap();
        fs::write(target.join("vk_swiftshader_icd.json"), b"{}").unwrap();
        fs::write(target.join("chrome_100_percent.pak"), b"pak").unwrap();
        fs::write(target.join("resources.pak"), b"pak").unwrap();
        fs::write(target.join("icudtl.dat"), b"dat").unwrap();
        fs::write(target.join("v8_context_snapshot.bin"), b"bin").unwrap();
        fs::write(target.join("locales/en-US.pak"), b"locale").unwrap();

        let payload = collect_runtime_payload(&target).unwrap();
        assert_eq!(payload.paks.len(), 2);
        assert_eq!(payload.blobs.len(), 2);
        assert_eq!(payload.runtime_libs.len(), 4);
        assert_eq!(payload.jsons.len(), 1);

        let dest = tmp.path().join("opt-buffr");
        stage_payload(&dest, &payload).unwrap();
        assert!(dest.join("buffr").exists());
        assert!(dest.join("buffr-app").exists());
        assert!(dest.join("buffr-helper").exists());
        assert!(dest.join("libcef.so").exists());
        assert!(dest.join("libEGL.so").exists());
        assert!(dest.join("libGLESv2.so").exists());
        assert!(dest.join("libvk_swiftshader.so").exists());
        assert!(dest.join("libvulkan.so.1").exists());
        assert!(dest.join("vk_swiftshader_icd.json").exists());
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
        fs::write(target.join("buffr-app"), b"#!/bin/sh\n").unwrap();
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
        // The HKCU\Software\kryptic\buffr key + InstallPath/Version
        // values are how an external uninstaller / updater discovers
        // an existing install. Lock them down. (Per-user install
        // → HKCU; HKLM is reserved for system-wide installs.)
        assert!(WIX_TEMPLATE.contains("Software\\kryptic\\buffr"));
        assert!(WIX_TEMPLATE.contains("InstallPath"));
        assert!(WIX_TEMPLATE.contains("Version"));
    }

    #[test]
    fn wix_template_is_per_user() {
        // No UAC prompt on install. Files land under
        // %LOCALAPPDATA%\Programs\buffr\, registry under HKCU.
        // `InstallScope="perUser"` implicitly sets ALLUSERS + MSIINSTALLPERUSER —
        // setting them explicitly is rejected by candle.exe (empty Value).
        assert!(WIX_TEMPLATE.contains("InstallScope=\"perUser\""));
        assert!(WIX_TEMPLATE.contains("LocalAppDataFolder"));
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
        // The hand-rolled .wxs lists buffr.exe (supervisor) + buffr-app.exe
        // + buffr-helper.exe; everything else (libcef.dll, paks, locales, ...)
        // is harvested by heat.exe at build time and referenced via
        // <ComponentGroupRef Id="CefRuntime" />.
        // Round 5: supervisor (buffr.exe) is included so the watchdog ships.
        assert!(WIX_TEMPLATE.contains("buffr.exe"));
        assert!(WIX_TEMPLATE.contains("buffr-app.exe"));
        assert!(WIX_TEMPLATE.contains("buffr-helper.exe"));
        assert!(WIX_TEMPLATE.contains("ComponentGroupRef Id=\"CefRuntime\""));
    }

    #[test]
    fn stage_windows_payload_lays_out_msi_tree() {
        let tmp = tempdir();
        let target = tmp.path().join("target-release");
        fs::create_dir_all(target.join("locales")).unwrap();
        // Round 5: supervisor (buffr.exe) is now required alongside the app.
        fs::write(target.join("buffr.exe"), b"MZ").unwrap();
        fs::write(target.join("buffr-app.exe"), b"MZ").unwrap();
        fs::write(target.join("buffr-helper.exe"), b"MZ").unwrap();
        fs::write(target.join("libcef.dll"), b"MZ").unwrap();
        fs::write(target.join("chrome_elf.dll"), b"MZ").unwrap();
        fs::write(target.join("vk_swiftshader_icd.json"), b"{}").unwrap();
        fs::write(target.join("icudtl.dat"), b"dat").unwrap();
        fs::write(target.join("resources.pak"), b"pak").unwrap();
        fs::write(target.join("v8_context_snapshot.bin"), b"bin").unwrap();
        fs::write(target.join("locales/en-US.pak"), b"locale").unwrap();

        let payload = collect_windows_payload_from(&target).unwrap();
        assert_eq!(payload.dlls.len(), 2, "libcef.dll + chrome_elf.dll");
        assert_eq!(payload.jsons.len(), 1);
        assert_eq!(payload.paks.len(), 1);
        assert_eq!(payload.blobs.len(), 1);

        let bin_dest = tmp.path().join("staged-bin");
        let cef_dest = tmp.path().join("staged-cef");
        stage_windows_payload(&bin_dest, &cef_dest, &payload).unwrap();
        // Bin dir holds supervisor + browser + helper.
        assert!(
            bin_dest.join("buffr.exe").exists(),
            "supervisor must be staged"
        );
        assert!(bin_dest.join("buffr-app.exe").exists());
        assert!(bin_dest.join("buffr-helper.exe").exists());
        assert!(!bin_dest.join("libcef.dll").exists());
        // CEF dir holds the runtime tree.
        assert!(cef_dest.join("libcef.dll").exists());
        assert!(cef_dest.join("chrome_elf.dll").exists());
        assert!(cef_dest.join("vk_swiftshader_icd.json").exists());
        assert!(cef_dest.join("icudtl.dat").exists());
        assert!(cef_dest.join("resources.pak").exists());
        assert!(cef_dest.join("v8_context_snapshot.bin").exists());
        assert!(cef_dest.join("locales/en-US.pak").exists());
    }

    #[test]
    fn stage_dmg_creates_app_copy_and_applications_symlink() {
        let tmp = tempdir();
        let app = tmp.path().join("Buffr.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/Info.plist"), "<plist/>").unwrap();
        fs::write(app.join("Contents/MacOS/buffr"), b"\x7fELF").unwrap();

        let staging = tmp.path().join("dmg-staging");
        stage_dmg(&staging, &app).unwrap();

        assert!(staging.join("Buffr.app/Contents/Info.plist").exists());
        assert!(staging.join("Buffr.app/Contents/MacOS/buffr").exists());
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

    // ------------------------------------------------------------------
    // M1: remote-controlled `index.json` file names + tar path traversal
    // ------------------------------------------------------------------

    #[test]
    fn archive_name_accepts_a_real_spotify_filename() {
        let n = "cef_binary_147.0.10+gabcdef0+chromium-147.0.0.0_linux64_minimal.tar.bz2";
        assert_eq!(validate_archive_name(n).unwrap(), n);
    }

    #[test]
    fn archive_name_rejects_traversal_and_separators() {
        for bad in [
            "../../.cargo/config.toml",
            "..",
            "../evil.tar.bz2",
            "sub/dir/evil.tar.bz2",
            "sub\\dir\\evil.tar.bz2",
            "/etc/passwd",
            "C:\\windows\\system32\\evil.dll",
            "",
            "-rf",
            "evil\u{0}.tar.bz2",
            "ev\nil.tar.bz2",
        ] {
            assert!(
                validate_archive_name(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn archive_name_rejects_implausibly_long_names() {
        let long = "a".repeat(256);
        assert!(validate_archive_name(&long).is_err());
    }

    #[test]
    fn tar_path_safety() {
        for good in [
            "cef_binary_147/Release/libcef.so",
            "./cef_binary_147/README.txt",
            "libcef.so",
        ] {
            assert!(tar_path_is_safe(Path::new(good)), "`{good}` should be safe");
        }
        for bad in [
            "../escape",
            "cef_binary_147/../../escape",
            "/etc/passwd",
            "cef/../../../../tmp/pwned",
            "",
        ] {
            assert!(
                !tar_path_is_safe(Path::new(bad)),
                "`{bad}` should be refused"
            );
        }
    }

    #[test]
    fn sha1_verification_round_trips_and_catches_tampering() {
        let dir = tempdir();
        let f = dir.path().join("blob.bin");
        fs::write(&f, b"abc").unwrap();
        // Known vector: SHA-1("abc").
        let expected = "a9993e364706816aba3e25717850c26c9cd0d89d";
        verify_sha1(&f, expected).unwrap();
        // Uppercase hex from the index must still match.
        fs::write(&f, b"abc").unwrap();
        verify_sha1(&f, &expected.to_ascii_uppercase()).unwrap();

        // Mismatch fails *and* removes the blob so a re-run can't pick
        // the bad archive back up and extract it.
        fs::write(&f, b"abd").unwrap();
        assert!(verify_sha1(&f, expected).is_err());
        assert!(!f.exists());
    }

    #[test]
    fn sha1_verification_rejects_a_malformed_digest() {
        let dir = tempdir();
        let f = dir.path().join("blob.bin");
        fs::write(&f, b"abc").unwrap();
        for bad in ["", "deadbeef", &"z".repeat(40)] {
            assert!(verify_sha1(&f, bad).is_err(), "`{bad}` should be rejected");
        }
    }

    // ------------------------------------------------------------------
    // M43: PATH resolution without shelling out to `which(1)`
    // ------------------------------------------------------------------

    #[test]
    fn which_finds_nothing_for_a_bogus_tool() {
        assert!(which_path("definitely-not-a-real-tool-xyzzy").is_none());
        assert!(!which("definitely-not-a-real-tool-xyzzy"));
    }

    #[cfg(unix)]
    #[test]
    fn which_finds_an_executable_on_path_and_skips_non_executables() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let exe = dir.path().join("buffr-fake-tool");
        fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();

        // Same dir, not executable — must not be resolved.
        let data = dir.path().join("buffr-fake-data");
        fs::write(&data, b"not a program").unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o644)).unwrap();

        // A directory that merely shares the name must not match either.
        let as_dir = dir.path().join("buffr-fake-dir");
        fs::create_dir_all(&as_dir).unwrap();

        let orig = env::var_os("PATH");
        // SAFETY: single-threaded test process; PATH is restored below.
        unsafe { env::set_var("PATH", dir.path()) };
        let found = which_path("buffr-fake-tool");
        let skipped = which_path("buffr-fake-data");
        let dir_hit = which_path("buffr-fake-dir");
        match orig {
            Some(p) => unsafe { env::set_var("PATH", p) },
            None => unsafe { env::remove_var("PATH") },
        }

        assert_eq!(found.as_deref(), Some(exe.as_path()));
        assert!(skipped.is_none(), "non-executable file must not resolve");
        assert!(dir_hit.is_none(), "directory must not resolve");
    }

    #[test]
    fn exe_candidates_shape_matches_the_host() {
        let got = exe_candidates("candle");
        assert!(got.contains(&"candle".to_string()));
        if cfg!(windows) {
            assert!(
                got.iter().any(|c| c.eq_ignore_ascii_case("candle.EXE")),
                "PATHEXT spellings missing: {got:?}"
            );
            // The bare name must be last so a PATHEXT hit wins.
            assert_eq!(got.last().unwrap(), "candle");
        } else {
            assert_eq!(got, vec!["candle".to_string()]);
        }
    }
}
