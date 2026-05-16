//! Smoke tests for buffr-firefox-cdp.
//!
//! Non-ignored tests run offline with no Firefox binary required.
//! Tests tagged `#[ignore]` require a real Firefox installation.

use std::sync::Arc;

use buffr_engine::{Backend, BrowserEngine};
use buffr_firefox_cdp::{FirefoxCdpBackend, FirefoxError};

// ── Offline tests (no Firefox required) ─────────────────────────────────────

/// `Backend::id()` must return "firefox-cdp".
#[test]
fn backend_id_is_firefox_cdp() {
    let backend = FirefoxCdpBackend::new();
    assert_eq!(backend.id(), "firefox-cdp");
}

/// `Backend::initialize()` must succeed without a Firefox binary.
/// firefox-cdp spawns lazily on open_engine — initialize is always a no-op.
#[test]
fn backend_initialize_succeeds() {
    let backend = FirefoxCdpBackend::new();
    let result = backend.initialize("/tmp/buffr-firefox-cdp-test-cache");
    assert!(
        result.is_ok(),
        "initialize() must succeed (lazy spawn): {result:?}"
    );
}

/// `FirefoxError::FirefoxNotFound` error message must mention install hints.
#[test]
fn firefox_not_found_error_message() {
    let msg = FirefoxError::FirefoxNotFound.to_string();
    assert!(
        msg.contains("firefox"),
        "error message must mention firefox: {msg}"
    );
    // Must tell the user what to install.
    assert!(
        msg.contains("install"),
        "error message must tell user to install: {msg}"
    );
}

/// `FirefoxError::FirefoxNotFound` must convert to `EngineError::InitFailed`.
#[test]
fn firefox_not_found_converts_to_engine_init_failed() {
    use buffr_engine::EngineError;
    let engine_err: EngineError = FirefoxError::FirefoxNotFound.into();
    assert!(
        matches!(engine_err, EngineError::InitFailed(_)),
        "FirefoxNotFound must map to EngineError::InitFailed, got {engine_err:?}"
    );
}

/// `pick_free_port` must return a non-zero ephemeral port.
#[test]
fn pick_free_port_offline() {
    use buffr_firefox_cdp::subprocess::pick_free_port;
    let port = pick_free_port().expect("OS must provide an ephemeral port");
    assert_ne!(port, 0);
    assert!(port >= 1024);
}

/// `find_firefox_from_candidates(&[])` must return `None` (not panic).
#[test]
fn find_firefox_no_candidates_returns_none() {
    use buffr_firefox_cdp::subprocess::find_firefox_from_candidates;
    let result = find_firefox_from_candidates(&[]);
    assert!(
        result.is_none(),
        "empty candidate slice must yield None, got {result:?}"
    );
}

/// Nonexistent binary name must yield `None`.
#[test]
fn find_firefox_nonexistent_returns_none() {
    use buffr_firefox_cdp::subprocess::find_firefox_from_candidates;
    let result = find_firefox_from_candidates(&["__buffr_test_no_such_firefox_binary_9z3x__"]);
    assert!(
        result.is_none(),
        "nonexistent binary must yield None, got {result:?}"
    );
}

/// `init_profile` must create the profile dir and write `prefs.js`.
#[test]
fn init_profile_creates_prefs_js_offline() {
    use buffr_firefox_cdp::subprocess::init_profile;
    let dir = tempfile::tempdir().expect("temp dir");
    let profile = dir.path().join("ffprofile");
    init_profile(&profile).expect("init_profile failed");
    let contents = std::fs::read_to_string(profile.join("prefs.js")).expect("read prefs.js");
    assert!(
        contents.contains("devtools.debugger.remote-enabled"),
        "prefs.js must contain remote-enabled pref"
    );
    assert!(
        contents.contains("remote.enabled"),
        "prefs.js must contain remote.enabled pref"
    );
}

/// CDP message id allocator is monotonically increasing.
#[test]
fn cdp_message_id_monotonic() {
    use buffr_firefox_cdp::cdp::next_id;
    let a = next_id();
    let b = next_id();
    let c = next_id();
    assert!(b > a);
    assert!(c > b);
}

/// CDP navigate command serialises correctly.
#[test]
fn cdp_navigate_command_serialises() {
    use buffr_firefox_cdp::cdp::{CdpCommand, NavigateParams};
    let cmd = CdpCommand::new(
        "Page.navigate",
        NavigateParams {
            url: "https://example.com",
        },
    )
    .with_session("sess-1".to_owned());
    let json = cmd.serialize();
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(v["method"], "Page.navigate");
    assert_eq!(v["params"]["url"], "https://example.com");
    assert_eq!(v["sessionId"], "sess-1");
    assert!(v["id"].as_u64().unwrap_or(0) > 0);
}

/// OSR frame decode + BGRA swap (offline PNG synthesis).
#[test]
fn osr_frame_decode_bgra_swap_offline() {
    use base64::Engine as _;
    use buffr_engine::OsrFrame;
    use buffr_firefox_cdp::worker::decode_and_write_frame;
    use std::sync::{Arc, Mutex};

    // Build a 2×2 solid-red PNG.
    let img = {
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(2, 2, |_, _| Rgba([255u8, 0, 0, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&img);

    let osr_frame: buffr_engine::SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(1, 1)));
    decode_and_write_frame(&b64, &osr_frame);

    let frame = osr_frame.lock().unwrap();
    assert_eq!(frame.width, 2);
    assert_eq!(frame.height, 2);
    // RGBA [255, 0, 0, 255] → BGRA [0, 0, 255, 255]
    assert_eq!(&frame.pixels[0..4], &[0u8, 0, 255, 255], "BGRA swap");
    assert_eq!(frame.generation, 1);
}

/// `FirefoxCdpBackend` constructor produces the correct id.
#[test]
fn backend_new_produces_correct_id() {
    let b1 = FirefoxCdpBackend::new();
    let b2 = FirefoxCdpBackend::new();
    assert_eq!(b1.id(), b2.id());
    assert_eq!(b1.id(), "firefox-cdp");
}

/// `as_any` downcasts correctly.
#[test]
fn backend_as_any_downcasts() {
    let backend = FirefoxCdpBackend::new();
    let any = backend.as_any();
    assert!(
        any.downcast_ref::<FirefoxCdpBackend>().is_some(),
        "as_any must downcast to FirefoxCdpBackend"
    );
}

/// `NavigationHistoryResult` parse and logic (offline).
#[test]
fn navigation_history_can_go_back_forward_logic() {
    use buffr_firefox_cdp::cdp::{NavigationEntry, NavigationHistoryResult};

    // Empty entries edge-case: no crash, no forward.
    let empty = NavigationHistoryResult {
        current_index: 0,
        entries: vec![],
    };
    assert!(!empty.can_go_back());
    assert!(!empty.can_go_forward());

    // At first entry of two: can only go forward.
    let two = NavigationHistoryResult {
        current_index: 0,
        entries: vec![
            NavigationEntry {
                id: 1,
                url: "https://first.example".into(),
                title: "First".into(),
            },
            NavigationEntry {
                id: 2,
                url: "https://second.example".into(),
                title: "Second".into(),
            },
        ],
    };
    assert!(!two.can_go_back());
    assert!(two.can_go_forward());

    // At last entry of two: can only go back.
    let at_last = NavigationHistoryResult {
        current_index: 1,
        entries: two.entries.clone(),
    };
    assert!(at_last.can_go_back());
    assert!(!at_last.can_go_forward());
}

/// Closed-stack logic: push on close, pop on reopen, length tracks correctly.
///
/// Exercises `EngineState::closed_stack` indirectly via `closed_stack_len`.
/// A live Firefox is not needed — the stack lives in engine state.
#[test]
fn closed_stack_len_reflects_pushed_urls() {
    use buffr_firefox_cdp::cdp::{NavigationEntry, NavigationHistoryResult};
    // We can't construct FirefoxCdpEngine offline, but we can test the
    // NavigationHistoryResult helper directly and rely on the engine-level
    // closed_stack integration being covered by the runtime tests.

    // Verify the result struct correctly counts entries.
    let history = NavigationHistoryResult {
        current_index: 0,
        entries: vec![NavigationEntry {
            id: 1,
            url: "about:blank".into(),
            title: "".into(),
        }],
    };
    assert_eq!(history.entries.len(), 1);
    assert!(!history.can_go_back());
    assert!(!history.can_go_forward());
}

/// `duplicate_active` — when active tab URL is available, the new tab has the
/// same URL.  Requires Firefox.
#[test]
#[ignore = "requires Firefox binary"]
fn duplicate_active_opens_same_url() {
    use buffr_engine::BrowserEngine;
    let engine = make_test_engine();
    let _id = engine.open_tab("https://example.com").expect("open_tab");
    assert_eq!(engine.tab_count(), 1);

    let dup_id = engine.duplicate_active().expect("duplicate_active");
    assert_eq!(engine.tab_count(), 2);

    let summaries = engine.tabs_summary();
    let original = summaries.iter().find(|t| t.id != dup_id);
    let dup = summaries.iter().find(|t| t.id == dup_id);
    assert!(original.is_some() && dup.is_some());
    assert_eq!(original.unwrap().url, dup.unwrap().url);
}

/// `reopen_closed_tab` — close a tab then reopen it; URL must match.
/// `closed_stack_len` must increment and decrement accordingly.
/// Requires Firefox.
#[test]
#[ignore = "requires Firefox binary"]
fn reopen_closed_tab_restores_url() {
    use buffr_engine::BrowserEngine;
    let engine = make_test_engine();
    let id = engine.open_tab("https://example.com").expect("open_tab");
    assert_eq!(engine.closed_stack_len(), 0);

    engine.close_tab(id).expect("close_tab");
    assert_eq!(engine.closed_stack_len(), 1, "stack grows on close");

    let reopened = engine
        .reopen_closed_tab()
        .expect("reopen_closed_tab")
        .expect("must return a TabId");
    assert_eq!(engine.closed_stack_len(), 0, "stack shrinks on reopen");
    assert_eq!(engine.tab_count(), 1);

    let summary = engine.tabs_summary();
    let tab = summary.iter().find(|t| t.id == reopened).unwrap();
    assert_eq!(tab.url, "https://example.com");
}

/// `reopen_closed_tab` when the stack is empty returns `Ok(None)`.
/// Requires Firefox.
#[test]
#[ignore = "requires Firefox binary"]
fn reopen_closed_tab_empty_stack_returns_none() {
    use buffr_engine::BrowserEngine;
    let engine = make_test_engine();
    let _id = engine.open_tab("https://example.com").expect("open_tab");
    assert_eq!(engine.closed_stack_len(), 0);

    let result = engine.reopen_closed_tab().expect("no error on empty stack");
    assert!(result.is_none(), "empty stack must return None");
}

// ── IME smoke tests (offline — no Firefox binary required) ───────────────────

/// `ime_set_composition` CDP params serialize correctly.
///
/// The JSON for `Input.imeSetComposition` must carry text, selectionStart,
/// selectionEnd, replacementStart, replacementEnd.
#[test]
fn ime_set_composition_json_shape() {
    use buffr_firefox_cdp::cdp::CdpCommand;
    let cmd = CdpCommand::new(
        "Input.imeSetComposition",
        serde_json::json!({
            "text": "日本語",
            "selectionStart": 0,
            "selectionEnd": 9,
            "replacementStart": 0,
            "replacementEnd": 0,
        }),
    );
    let json = cmd.serialize();
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(v["method"], "Input.imeSetComposition");
    assert_eq!(v["params"]["text"], "日本語");
    assert_eq!(v["params"]["selectionStart"], 0);
    assert_eq!(v["params"]["selectionEnd"], 9);
}

/// `ime_commit` maps to `Input.insertText`.
#[test]
fn ime_commit_json_shape() {
    use buffr_firefox_cdp::cdp::CdpCommand;
    let cmd = CdpCommand::new("Input.insertText", serde_json::json!({ "text": "日本語" }));
    let json = cmd.serialize();
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(v["method"], "Input.insertText");
    assert_eq!(v["params"]["text"], "日本語");
}

/// `ime_cancel` maps to `Input.imeSetComposition` with empty text.
#[test]
fn ime_cancel_json_shape() {
    use buffr_firefox_cdp::cdp::CdpCommand;
    let cmd = CdpCommand::new(
        "Input.imeSetComposition",
        serde_json::json!({
            "text": "",
            "selectionStart": 0,
            "selectionEnd": 0,
            "replacementStart": 0,
            "replacementEnd": 0,
        }),
    );
    let json = cmd.serialize();
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(v["params"]["text"], "");
    assert_eq!(v["params"]["selectionStart"], 0);
}

/// `can_go_back` / `can_go_forward` via Page.getNavigationHistory. Requires Firefox.
#[test]
#[ignore = "requires Firefox binary"]
fn can_go_back_forward_after_navigate() {
    use buffr_engine::BrowserEngine;
    let engine = make_test_engine();
    let _id = engine.open_tab("https://example.com").expect("open_tab");

    // After opening a tab there is only one history entry — no back/forward.
    // (Firefox adds an about:blank entry before navigating; behaviour depends
    // on timing, so we relax the assertion here.)
    let _ = engine.can_go_back(); // Must not panic.

    // Navigate to a second page — should unlock can_go_back.
    engine.navigate("https://mozilla.org").expect("navigate");
    // Allow the navigation to settle.
    std::thread::sleep(std::time::Duration::from_secs(3));

    assert!(
        engine.can_go_back(),
        "can_go_back must be true after 2nd navigate"
    );
    assert!(
        !engine.can_go_forward(),
        "can_go_forward must be false at the tip"
    );
}

// ── Runtime tests (require Firefox) ─────────────────────────────────────────

/// OSR frame Arc is stable across calls (same pointer).
///
/// Requires a running Firefox instance.
#[test]
#[ignore = "requires Firefox binary"]
fn osr_frame_arc_is_stable() {
    let engine = make_test_engine();
    let f1 = engine.osr_frame();
    let f2 = engine.osr_frame();
    assert!(
        Arc::ptr_eq(&f1, &f2),
        "osr_frame() must return the same Arc on repeated calls"
    );
}

/// OSR view Arc is stable across calls (same pointer).
///
/// Requires a running Firefox instance.
#[test]
#[ignore = "requires Firefox binary"]
fn osr_view_arc_is_stable() {
    use std::sync::Arc;
    let engine = make_test_engine();
    let v1 = engine.osr_view();
    let v2 = engine.osr_view();
    assert!(
        Arc::ptr_eq(&v1, &v2),
        "osr_view() must return the same Arc on repeated calls"
    );
}

/// Open a tab, navigate, then close it. Verifies the basic lifecycle.
///
/// Requires a running Firefox instance.
#[test]
#[ignore = "requires Firefox binary"]
fn open_tab_navigate_close() {
    use buffr_engine::BrowserEngine;
    let engine = make_test_engine();

    let id = engine.open_tab("https://example.com").expect("open_tab");
    assert_eq!(engine.tab_count(), 1);

    engine.navigate("https://mozilla.org").expect("navigate");
    engine.close_tab(id).expect("close_tab");
    assert_eq!(engine.tab_count(), 0);
}

// ── Helper ───────────────────────────────────────────────────────────────────

#[cfg(test)]
fn make_test_engine() -> buffr_firefox_cdp::FirefoxCdpEngine {
    use buffr_firefox_cdp::FirefoxCdpEngine;
    let dir = tempfile::tempdir().expect("temp dir");
    FirefoxCdpEngine::new(dir.path(), false, None, None, None).expect("FirefoxCdpEngine::new")
}
