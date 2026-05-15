//! One-time migration shim for Phase 11a — engine on-disk namespace layout.
//!
//! Before Phase 11a both engines placed their state in unnamespaced locations:
//!
//! - **CEF**: flat at `~/.cache/buffr/` (Default/, Cache/, ShaderCache/, etc.)
//! - **blink-cdp**: `~/.local/share/buffr/blink-cdp/<inst-id>/`
//!
//! After Phase 11a the canonical layout is:
//!
//! - **CEF**: `~/.cache/buffr/engines/<engine-id>/`
//! - **blink-cdp**: `~/.local/share/buffr/engines/<engine-id>/profile/`
//!
//! This module contains the startup migration function that moves existing
//! blink-cdp profiles to the new location and warns about stale CEF flat
//! state. Migrations that fail do NOT abort startup — a fresh profile is
//! used instead, so the user can always get a working browser.

use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Compute the default CEF per-engine data directory for `engine_id`
/// under `cache_root`.
///
/// Canonical location: `<cache_root>/engines/<engine_id>/`
pub fn compute_cef_default(cache_root: &Path, engine_id: &str) -> PathBuf {
    cache_root.join("engines").join(engine_id)
}

/// Compute the default blink-cdp per-engine profile directory for `engine_id`
/// under `data_root`.
///
/// Canonical location: `<data_root>/engines/<engine_id>/profile/`
pub fn compute_blink_cdp_default(data_root: &Path, engine_id: &str) -> PathBuf {
    data_root.join("engines").join(engine_id).join("profile")
}

/// Compute the default blink-cdp ephemeral cache directory for `engine_id`
/// under `cache_root`.
///
/// Canonical location: `<cache_root>/engines/<engine_id>/`
/// Used by Phase 11b for `--disk-cache-dir` split.
pub fn compute_blink_cdp_cache_default(cache_root: &Path, engine_id: &str) -> PathBuf {
    cache_root.join("engines").join(engine_id)
}

/// Run the Phase 11a layout migration on startup.
///
/// **blink-cdp**: If `<data_root>/blink-cdp/` exists, every `<inst-id>/`
/// subdirectory is moved to `<data_root>/engines/<inst-id>/profile/` via
/// `std::fs::rename` (atomic same-filesystem move). On failure the old
/// path is left in place and a warning is logged — the engine will simply
/// start with a fresh profile at the new location.
///
/// **CEF**: If `<cache_root>/Default` exists (indicating flat pre-11a CEF
/// state) a one-time warning is logged telling the user that CEF state was
/// reset due to the layout change and pointing them to the cleanup command.
/// No files are auto-deleted.
///
/// This function never panics and never returns an error — all failures are
/// logged at WARN level.
pub fn migrate_engine_layout(cache_root: &Path, data_root: &Path) {
    migrate_blink_cdp(data_root);
    warn_cef_flat_spill(cache_root);
}

fn migrate_blink_cdp(data_root: &Path) {
    let old_root = data_root.join("blink-cdp");
    if !old_root.exists() {
        return;
    }

    let entries = match std::fs::read_dir(&old_root) {
        Ok(e) => e,
        Err(err) => {
            warn!(
                path = %old_root.display(),
                error = %err,
                "engine_migrate: could not read blink-cdp directory — skipping migration"
            );
            return;
        }
    };

    for entry_result in entries {
        let entry = match entry_result {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    error = %err,
                    "engine_migrate: error reading blink-cdp dir entry — skipping entry"
                );
                continue;
            }
        };

        let old_path = entry.path();
        // Only migrate directories (each one is an instance profile).
        if !old_path.is_dir() {
            continue;
        }

        let inst_id = match old_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => {
                warn!(
                    path = %old_path.display(),
                    "engine_migrate: blink-cdp entry has non-UTF-8 name — skipping"
                );
                continue;
            }
        };

        let new_path = compute_blink_cdp_default(data_root, &inst_id);

        // If the target already exists (idempotent second run) skip the move.
        if new_path.exists() {
            info!(
                engine_id = %inst_id,
                new_path = %new_path.display(),
                "engine_migrate: new blink-cdp path already exists — skipping"
            );
            continue;
        }

        // Ensure parent directory exists.
        if let Some(parent) = new_path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            warn!(
                engine_id = %inst_id,
                path = %parent.display(),
                error = %err,
                "engine_migrate: failed to create new blink-cdp parent dir — skipping"
            );
            continue;
        }

        match std::fs::rename(&old_path, &new_path) {
            Ok(()) => {
                info!(
                    engine_id = %inst_id,
                    old = %old_path.display(),
                    new = %new_path.display(),
                    "engine_migrate: blink-cdp profile moved to new engines/ layout"
                );
            }
            Err(err) => {
                warn!(
                    engine_id = %inst_id,
                    old = %old_path.display(),
                    new = %new_path.display(),
                    error = %err,
                    "engine_migrate: failed to move blink-cdp profile — engine will start fresh"
                );
            }
        }
    }

    // If the old root is now empty, remove it.
    match std::fs::remove_dir(&old_root) {
        Ok(()) => {
            info!(
                path = %old_root.display(),
                "engine_migrate: removed empty blink-cdp directory"
            );
        }
        Err(_) => {
            // Non-empty (failed moves left files) or already gone — both fine.
        }
    }
}

fn warn_cef_flat_spill(cache_root: &Path) {
    let old_default = cache_root.join("Default");
    if old_default.exists() {
        warn!(
            path = %old_default.display(),
            "engine_migrate: detected pre-Phase-11a CEF state at flat cache root. \
             CEF will now use ~/.cache/buffr/engines/<id>/ for new state. \
             Old files (Default/, Cache/, ShaderCache/, etc.) were NOT deleted — \
             you can clean them manually: \
             `rm -rf ~/.cache/buffr/Default ~/.cache/buffr/Cache \
             ~/.cache/buffr/ShaderCache ~/.cache/buffr/chrome_debug.log`"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- pure path helpers -----------------------------------------------

    #[test]
    fn compute_blink_cdp_default_matches_expected() {
        let data_root = PathBuf::from("/home/user/.local/share/buffr");
        let result = compute_blink_cdp_default(&data_root, "engine-x");
        assert_eq!(
            result,
            PathBuf::from("/home/user/.local/share/buffr/engines/engine-x/profile")
        );
    }

    #[test]
    fn compute_blink_cdp_cache_default_matches_expected() {
        let cache_root = PathBuf::from("/home/user/.cache/buffr");
        let result = compute_blink_cdp_cache_default(&cache_root, "engine-z");
        assert_eq!(
            result,
            PathBuf::from("/home/user/.cache/buffr/engines/engine-z")
        );
    }

    #[test]
    fn compute_cef_default_matches_expected() {
        let cache_root = PathBuf::from("/home/user/.cache/buffr");
        let result = compute_cef_default(&cache_root, "engine-y");
        assert_eq!(
            result,
            PathBuf::from("/home/user/.cache/buffr/engines/engine-y")
        );
    }

    // ---- migration: blink-cdp -------------------------------------------

    /// Build a tempdir that looks like the pre-11a blink-cdp layout:
    ///   <data_root>/blink-cdp/eng1/Cookies
    ///   <data_root>/blink-cdp/eng1/Cookies-journal
    fn setup_old_blink_cdp(data_root: &Path) {
        let old_dir = data_root.join("blink-cdp").join("eng1");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("Cookies"), b"cookie-data").unwrap();
        std::fs::write(old_dir.join("Cookies-journal"), b"journal-data").unwrap();
    }

    #[test]
    fn migration_moves_blink_cdp_profile_to_new_layout() {
        let tmp = TempDir::new().unwrap();
        let data_root = tmp.path();

        setup_old_blink_cdp(data_root);

        // Run migration (cache_root doesn't matter for blink-cdp migration).
        migrate_engine_layout(data_root, data_root);

        let new_profile = compute_blink_cdp_default(data_root, "eng1");
        assert!(
            new_profile.exists(),
            "new profile dir should exist: {}",
            new_profile.display()
        );
        assert_eq!(
            std::fs::read(new_profile.join("Cookies")).unwrap(),
            b"cookie-data"
        );
        assert_eq!(
            std::fs::read(new_profile.join("Cookies-journal")).unwrap(),
            b"journal-data"
        );

        // Old location must be gone.
        let old_dir = data_root.join("blink-cdp").join("eng1");
        assert!(
            !old_dir.exists(),
            "old profile dir should be removed: {}",
            old_dir.display()
        );
    }

    #[test]
    fn migration_is_idempotent_when_no_old_dir_present() {
        // Existing behaviour: no old dir → noop on second run.
        let tmp = TempDir::new().unwrap();
        let data_root = tmp.path();

        setup_old_blink_cdp(data_root);

        migrate_engine_layout(data_root, data_root);
        migrate_engine_layout(data_root, data_root); // should not panic or error

        let new_profile = compute_blink_cdp_default(data_root, "eng1");
        assert!(new_profile.exists());
        assert_eq!(
            std::fs::read(new_profile.join("Cookies")).unwrap(),
            b"cookie-data"
        );
    }

    #[test]
    fn migration_skips_when_new_path_already_exists() {
        // Both old AND new exist (e.g. from a previous partial migration or
        // user manual setup). New must be preserved; old must remain in place
        // (NOT overwritten).
        let tmp = TempDir::new().unwrap();
        let data_root = tmp.path();

        // Pre-existing new layout with different content.
        let new_profile = compute_blink_cdp_default(data_root, "eng1");
        std::fs::create_dir_all(&new_profile).unwrap();
        std::fs::write(new_profile.join("Cookies"), b"new-cookies").unwrap();

        // Old layout with stale content alongside.
        setup_old_blink_cdp(data_root); // writes b"cookie-data" to old/Cookies

        migrate_engine_layout(data_root, data_root);

        // New content preserved (NOT overwritten by old).
        assert_eq!(
            std::fs::read(new_profile.join("Cookies")).unwrap(),
            b"new-cookies",
            "migration must not overwrite existing new-layout content"
        );

        // Old still in place because new existed (we skipped the rename).
        let old_dir = data_root.join("blink-cdp").join("eng1");
        assert!(
            old_dir.exists(),
            "old dir should remain because new already existed"
        );
    }

    #[test]
    fn migration_no_old_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let data_root = tmp.path();

        // No blink-cdp directory — should not panic or create anything.
        migrate_engine_layout(data_root, data_root);

        assert!(!data_root.join("blink-cdp").exists());
        assert!(!data_root.join("engines").exists());
    }
}
