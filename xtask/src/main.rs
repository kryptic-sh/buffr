//! `cargo xtask` — build automation tasks for buffr.
//!
//! Subcommands:
//!
//! - `fetch-cef [--platform <linux64|macosarm64|macosx64|windows64>] [--version <X.Y.Z>]`
//!   downloads the CEF Spotify minimal binary distribution matching the `cef`
//!   crate version (147 by default) and extracts it into
//!   `vendor/cef/<platform>/`.
//!
//! Run from the workspace root: `cargo xtask fetch-cef`.

use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

const DEFAULT_CDN: &str = "https://cef-builds.spotifycdn.com";
/// CEF major version we pin against the `cef` crate (147.x).
///
/// Spotify CDN entries look like `cef_binary_147.0.10+gXXXXX+chromium-...`;
/// we pick the newest entry whose version starts with this prefix.
const CEF_VERSION_PREFIX: &str = "147.";

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
    println!("    fetch-cef [--platform PLATFORM] [--version PREFIX]");
    println!("        Download + extract CEF minimal binary distribution.");
    println!("        PLATFORM: linux64 (default on Linux), macosarm64, macosx64, windows64.");
    println!("        PREFIX:   version prefix to match (default: {CEF_VERSION_PREFIX}).");
}

fn fetch_cef(args: Vec<String>) -> Result<()> {
    let mut platform: Option<String> = None;
    let mut version_prefix = CEF_VERSION_PREFIX.to_string();

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--platform" => {
                platform = Some(iter.next().context("--platform requires a value")?);
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
        .into_json()
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
    let mut reader = resp.into_reader();
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
}
