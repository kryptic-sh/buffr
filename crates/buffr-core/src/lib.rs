//! CEF integration and browser host for buffr.
//!
//! Phase 1 surface: a [`BuffrApp`] wrapper around the `cef` crate's
//! `App` trait and a [`BrowserHost`] that creates a single browser
//! attached to a winit-backed native window. Both are intentionally
//! minimal — Phase 2 will expand them to wire up the modal engine
//! and render-process IPC.

use std::path::PathBuf;

use directories::ProjectDirs;
use thiserror::Error;

pub mod app;
pub mod host;

/// Off-screen rendering scaffold. Gated behind the `osr` feature
/// because it pulls in (eventually) `wgpu` and `softbuffer` deps.
/// Currently scaffolded only — runtime entries panic. See `PLAN.md`
/// Phase 3.
#[cfg(feature = "osr")]
pub mod osr;

pub use app::{BuffrApp, ProfilePaths};
pub use host::BrowserHost;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("cef initialize() returned a failure code")]
    InitFailed,
    #[error("could not resolve project directories")]
    NoProjectDirs,
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("creating browser failed")]
    CreateBrowserFailed,
}

/// `crates/buffr-core` version (`CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Pin the CEF runtime API version before any CEF entry point.
///
/// `cef-rs` 147 wraps libcef 147, which in turn ships an API-version
/// negotiation layer: every C-to-C++ wrapper checks an integer
/// "API version" field on the wrapped struct (`App`, `Client`, …) and
/// aborts with `CefXxx_0_CToCpp called with invalid version -1` if it
/// hasn't been initialized.
///
/// `cef::api_hash(version, 0)` performs that negotiation; the `version`
/// is sticky after the first call. We call it with `CEF_API_VERSION_LAST`
/// (the highest version cef-dll-sys was generated against) so all
/// wrapper entries route through versioned slots instead of the bogus
/// slot-0 path.
///
/// MUST be invoked **before** `cef::execute_process` / `cef::initialize`
/// in every process — both the browser binary and any helper that
/// re-enters CEF for renderer/GPU/utility subprocess dispatch.
pub fn init_cef_api() {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
}

/// Resolve buffr's per-user profile + cache directories. Created on
/// first call.
pub fn profile_paths() -> Result<ProfilePaths, CoreError> {
    let dirs = ProjectDirs::from("sh", "kryptic", "buffr").ok_or(CoreError::NoProjectDirs)?;
    let cache: PathBuf = dirs.cache_dir().to_path_buf();
    let data: PathBuf = dirs.data_dir().to_path_buf();
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::create_dir_all(&data);
    Ok(ProfilePaths { cache, data })
}
