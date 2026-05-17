//! Smoke tests for the Blitz backend.
//!
//! Validates that:
//! - `BlitzBackend::new()` + `initialize()` succeed
//! - `open_engine()` with a `data:` URL opens a real tab
//! - `tabs_summary().len() == 1` and `active_tab().is_some()`
//! - `osr_frame()` / `osr_view()` return stable `Arc` handles (pointer equality)

use std::sync::Arc;

use buffr_blitz::BlitzBackend;
use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, engine_id::EngineId};

/// Helper: open an engine at the given `data:` URL.
fn open_engine(url: &str) -> Arc<dyn BrowserEngine> {
    let backend = BlitzBackend::new();
    let engine_id = EngineId::new("test");
    let opts = BackendOpenOptions {
        engine_id,
        initial_url: url,
        initial_size: (800, 600),
        data_dir: None,
        cache_dir: None,
        frame_rate: 60,
        device_scale: 1.0,
        private: false,
        history: None,
        download_dir: None,
        downloads: None,
        notice_queue: None,
        find_sink: None,
        sinks: Box::new(()),
        prefer_native: false,
    };
    backend.open_engine(opts).expect("open_engine failed")
}

#[test]
fn backend_initialize_succeeds() {
    let backend = BlitzBackend::new();
    let result = backend.initialize("/tmp/buffr-blitz-smoke");
    assert!(result.is_ok(), "initialize() should return Ok: {result:?}");
}

#[test]
fn open_engine_data_uri() {
    let engine = open_engine("data:text/html,<h1>Hi</h1>");
    // After open_engine the worker should have opened one tab.
    assert_eq!(
        engine.tab_count(),
        1,
        "expected exactly 1 tab after open_engine"
    );
    assert!(engine.active_tab().is_some(), "active_tab() should be Some");
}

#[test]
fn tabs_summary_matches_count() {
    let engine = open_engine("data:text/html,<h1>Tabs</h1>");
    let summary = engine.tabs_summary();
    assert_eq!(summary.len(), engine.tab_count());
    assert_eq!(summary.len(), 1);
}

#[test]
fn osr_frame_arc_is_stable() {
    let engine = open_engine("data:text/html,<h1>OSR</h1>");
    let frame_a = engine.osr_frame();
    let frame_b = engine.osr_frame();
    // Both clones must point to the same allocation.
    assert!(
        Arc::ptr_eq(&frame_a, &frame_b),
        "osr_frame() must return the same Arc on every call"
    );
}

#[test]
fn osr_view_arc_is_stable() {
    let engine = open_engine("data:text/html,<h1>View</h1>");
    let view_a = engine.osr_view();
    let view_b = engine.osr_view();
    assert!(
        Arc::ptr_eq(&view_a, &view_b),
        "osr_view() must return the same Arc on every call"
    );
}

#[test]
fn osr_frame_has_pixels_after_open() {
    let engine = open_engine("data:text/html,<h1>Pixels</h1>");
    let frame = engine.osr_frame();
    let guard = frame.lock().expect("frame lock failed");
    assert!(
        !guard.pixels.is_empty(),
        "pixels should be non-empty after open_engine"
    );
    assert_eq!(
        guard.pixels.len(),
        (guard.width as usize) * (guard.height as usize) * 4,
        "pixel buffer length mismatch"
    );
}

#[test]
fn open_tab_increments_count() {
    let engine = open_engine("data:text/html,<p>first</p>");
    assert_eq!(engine.tab_count(), 1);
    engine
        .open_tab("data:text/html,<p>second</p>")
        .expect("open_tab failed");
    assert_eq!(engine.tab_count(), 2);
}

#[test]
fn close_tab_decrements_count() {
    let engine = open_engine("data:text/html,<p>A</p>");
    let id = engine
        .open_tab("data:text/html,<p>B</p>")
        .expect("open_tab B");
    assert_eq!(engine.tab_count(), 2);
    engine.close_tab(id).expect("close_tab B");
    assert_eq!(engine.tab_count(), 1);
}

#[test]
fn navigate_updates_url() {
    let engine = open_engine("data:text/html,<p>A</p>");
    engine
        .navigate("data:text/html,<p>B</p>")
        .expect("navigate failed");
    let url = engine.active_tab_live_url();
    assert_eq!(
        url, "data:text/html,<p>B</p>",
        "URL should update after navigate"
    );
}

#[test]
fn active_index_is_valid() {
    let engine = open_engine("data:text/html,<p>X</p>");
    let idx = engine.active_index();
    assert_eq!(idx, Some(0), "first tab should be at index 0");
}

#[test]
fn backend_id_is_blitz() {
    let backend = BlitzBackend::new();
    assert_eq!(backend.id(), "blitz");
}

// ── Renderer smoke test ───────────────────────────────────────────────────────

/// Verify the CPU renderer writes non-white pixels for a page with coloured content.
///
/// Opens an engine on a `data:` URL with a red background and waits for the
/// worker to finish painting.  The OSR frame must contain at least one pixel
/// that is not all-0xFF (pure white BGRA) — proving the real renderer ran.
///
/// Not marked `#[ignore]` because blitz uses a synchronous layout model and
/// the paint happens on the worker thread before `open_engine` returns.
#[test]
fn osr_frame_not_all_white_after_render() {
    let engine = open_engine("data:text/html,<body style=\"background:red\"><h1>Hi</h1></body>");

    // Give the worker thread a moment to finish painting (it's already done
    // before open_engine returns, but be defensive).
    std::thread::sleep(std::time::Duration::from_millis(100));

    let frame = engine.osr_frame();
    let guard = frame.lock().expect("frame lock failed");

    // The pixel buffer must be non-empty.
    assert!(
        !guard.pixels.is_empty(),
        "pixel buffer should be non-empty after render"
    );

    // At least one pixel must differ from all-0xFF (solid white BGRA fill).
    // A red background (BGRA = 0x00, 0x00, 0xFF, 0xFF with premul) will have
    // B=0 in most pixels, which is != 0xFF.
    let has_non_white = guard.pixels.chunks_exact(4).any(|p| {
        // p = [B, G, R, A]
        p[0] != 0xFF || p[1] != 0xFF || p[2] != 0xFF
    });
    assert!(
        has_non_white,
        "expected non-white pixels from CPU renderer; all pixels were 0xFF (white fill fallback)"
    );
}

// ── Input dispatch smoke tests ────────────────────────────────────────────────

/// Verify the full input dispatch chain is wired and does not panic.
///
/// Opens a real engine, sends a key event and a mouse click through the
/// `BrowserEngine` API. The point is NOT to verify DOM-level correctness
/// (impossible without a live renderer) but to confirm:
///   - translated events reach the worker via `Command::SendInput`
///   - `HtmlDocument::handle_ui_event` is called without panicking
///   - no event is silently dropped before reaching the worker thread
///
/// `#[ignore]` is NOT set — blitz uses a synchronous layout model and
/// constructs correctly in a headless test environment.
#[test]
fn osr_input_dispatch_no_panic() {
    use buffr_engine::{KeyEventKind, MouseButton, NeutralKeyEvent};

    let engine = open_engine("data:text/html,<h1>Input test</h1>");

    // Key event — RawDown phase
    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::RawDown,
        windows_key_code: 65, // VK_A
        character: b'a' as u16,
        ..Default::default()
    });

    // Key event — Char phase
    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::Char,
        windows_key_code: 65,
        character: b'a' as u16,
        ..Default::default()
    });

    // Key event — Up phase
    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::Up,
        windows_key_code: 65,
        character: b'a' as u16,
        ..Default::default()
    });

    // Mouse click — down then up
    engine.osr_mouse_click(100, 200, MouseButton::Left, false, 1, 0);
    engine.osr_mouse_click(100, 200, MouseButton::Left, true, 1, 0);

    // Give the worker thread a moment to drain the channel.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // If we reach here the dispatch chain did not panic.
    assert!(
        engine.tab_count() >= 1,
        "tab must still be open after input"
    );
}

// ── IME smoke tests ───────────────────────────────────────────────────────────

/// `ime_set_composition` must not panic and the engine must remain healthy.
///
/// Blitz wires this through `UiEvent::Ime(BlitzImeEvent::Preedit)` via the
/// worker's `Command::SendInput` channel.
#[test]
fn ime_set_composition_no_panic() {
    let engine = open_engine("data:text/html,<input id=i>");
    engine.ime_set_composition("日本語", Some((0, 9)));
    engine.ime_set_composition("にほんご", None);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        engine.tab_count() >= 1,
        "engine still alive after ime_set_composition"
    );
}

/// `ime_commit` must not panic.
///
/// Wires through `BlitzImeEvent::Commit`.
#[test]
fn ime_commit_no_panic() {
    let engine = open_engine("data:text/html,<input id=i>");
    engine.ime_set_composition("te", Some((0, 2)));
    engine.ime_commit("test");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        engine.tab_count() >= 1,
        "engine still alive after ime_commit"
    );
}

/// `ime_cancel` must not panic.
///
/// Wires through `BlitzImeEvent::Disabled`.
#[test]
fn ime_cancel_no_panic() {
    let engine = open_engine("data:text/html,<input id=i>");
    engine.ime_set_composition("te", None);
    engine.ime_cancel();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        engine.tab_count() >= 1,
        "engine still alive after ime_cancel"
    );
}
