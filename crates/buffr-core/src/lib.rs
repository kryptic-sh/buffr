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
