//! Atomic JSON file writes — write to a sibling tempfile, then rename.
//!
//! Both the session store and the crash guard persist JSON state that a
//! crash mid-write must not corrupt; the rename makes the write atomic
//! (the previous good file survives until the new one is complete).

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// Atomically write `value` as pretty JSON to `path`. Parent dir is
/// created on demand. Writes to a sibling `<stem>.json.tmp`, restricts
/// it to owner-only on unix, then renames over `path`, so a crash
/// mid-write leaves the previous good file intact. `what` names the
/// payload for error context ("session file", "launch log").
pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize, what: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {what} parent directory {}", parent.display()))?;
    }
    let json =
        serde_json::to_string_pretty(value).with_context(|| format!("serializing {what}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
