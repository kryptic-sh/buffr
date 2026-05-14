//! CEF subprocess helper.
//!
//! On Linux/Windows the main `buffr` binary re-launches itself with
//! `--type=...` for renderer / GPU / utility processes; we still ship
//! a separate `buffr-helper` binary for the macOS Helper.app bundle
//! path (CEF requires a distinct executable under `Contents/Frameworks`
//! on macOS).
//!
//! In all cases the helper does the bare minimum: forwards argv to
//! `buffr_cef::execute_subprocess`, exits with whatever code CEF returns.
//!
//! # Per-engine identification (#80)
//!
//! When the browser process launches a subprocess for a specific
//! `cef::RequestContext` (one per engine instance, see Phase 7a / #79),
//! it can append `--buffr-engine-id=<id>` via the
//! `OnBeforeChildProcessLaunch` callback. The helper logs the id at
//! startup so multi-engine triage (which engine's subprocess crashed?)
//! reads off stderr without needing pid → pstree archaeology.
//!
//! CEF's own arg parser ignores unknown `--buffr-*` flags so the flag
//! passes through unaltered.

/// Scan argv for `--buffr-engine-id=<id>`. Returns `Some(id)` if found.
fn extract_engine_id<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().find_map(|a| {
        a.as_ref()
            .strip_prefix("--buffr-engine-id=")
            .map(str::to_string)
    })
}

fn main() {
    if let Some(id) = extract_engine_id(std::env::args()) {
        // Subprocess stderr lands in the browser process console — useful
        // when debugging which engine's helper crashed.
        eprintln!("buffr-helper: engine_id={id}");
    }

    // Load the CEF framework library before any CEF call. On macOS this
    // resolves `Chromium Embedded Framework.framework` via `../../..`
    // (helper = true). On Linux/Windows this is a no-op.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("buffr-helper: resolving current_exe failed: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = buffr_cef::load_cef_library(&exe, true) {
        eprintln!("buffr-helper: {err}");
        std::process::exit(1);
    }

    // Returns >= 0 for child processes (renderer/GPU/utility) which exit
    // immediately afterwards; returns -1 for the browser process, which
    // never reaches a helper binary in practice.
    let exit = buffr_cef::execute_subprocess();
    std::process::exit(exit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_engine_id_finds_flag() {
        let argv = [
            "buffr-helper",
            "--type=renderer",
            "--buffr-engine-id=cef-a",
            "--no-sandbox",
        ];
        assert_eq!(extract_engine_id(argv), Some("cef-a".to_string()));
    }

    #[test]
    fn extract_engine_id_returns_none_when_absent() {
        let argv = ["buffr-helper", "--type=gpu-process"];
        assert_eq!(extract_engine_id(argv), None);
    }

    #[test]
    fn extract_engine_id_empty_value() {
        let argv = ["buffr-helper", "--buffr-engine-id="];
        assert_eq!(extract_engine_id(argv), Some(String::new()));
    }

    #[test]
    fn extract_engine_id_first_match_wins() {
        let argv = [
            "buffr-helper",
            "--buffr-engine-id=cef-a",
            "--buffr-engine-id=cef-b",
        ];
        assert_eq!(extract_engine_id(argv), Some("cef-a".to_string()));
    }
}
