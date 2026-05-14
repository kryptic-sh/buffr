//! Engine-agnostic profile path types.

use std::path::PathBuf;

/// Resolved on-disk paths buffr uses for cache + profile data.
///
/// The concrete paths are populated by the backend (e.g. `buffr_cef::profile_paths`),
/// but the struct itself is engine-agnostic so the apps layer can hold it
/// without importing `buffr-cef`.
#[derive(Debug, Clone)]
pub struct ProfilePaths {
    pub cache: PathBuf,
    pub data: PathBuf,
}
