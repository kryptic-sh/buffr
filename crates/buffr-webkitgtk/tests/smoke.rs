//! Smoke tests for `buffr-webkitgtk`.
//!
//! Non-ignored tests run offline without a display server.
//! Tests tagged `#[ignore]` require a Wayland/X11 session and WebKitGTK 6.0.
//!
//! On non-Linux targets the stub backend is tested instead: `open_engine`
//! must return an "Linux only" error.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, engine_id::EngineId};
use buffr_webkitgtk::WebKitGtkBackend;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_opts(url: &str) -> BackendOpenOptions<'_> {
    BackendOpenOptions {
        engine_id: EngineId::new("smoke-test"),
        initial_url: url,
        initial_size: (800, 600),
        data_dir: None,
        cache_dir: None,
        frame_rate: 60,
        device_scale: 1.0,
        private: false,
        download_dir: None,
        downloads: None,
        notice_queue: None,
        find_sink: None,
        sinks: Box::new(()),
    }
}

// ── Offline tests (no display required) ──────────────────────────────────────

/// `Backend::id()` must always return "webkitgtk".
#[test]
fn backend_id_is_webkitgtk() {
    let backend = WebKitGtkBackend::new();
    assert_eq!(backend.id(), "webkitgtk");
}

/// `Backend::initialize()` must succeed on Linux (no-op); must return Err on
/// non-Linux (stub path).
#[test]
fn backend_initialize_succeeds() {
    let backend = WebKitGtkBackend::new();

    #[cfg(target_os = "linux")]
    {
        let result = backend.initialize("/tmp/buffr-webkitgtk-smoke");
        assert!(
            result.is_ok(),
            "initialize() must succeed on Linux: {result:?}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    {
        let result = backend.initialize("/tmp/buffr-webkitgtk-smoke");
        assert!(
            result.is_err(),
            "initialize() must fail on non-Linux (stub): {result:?}"
        );
    }
}

/// On non-Linux, `open_engine` must return a "Linux only" error.
#[cfg(not(target_os = "linux"))]
#[test]
fn stub_open_engine_returns_linux_only_error() {
    let backend = WebKitGtkBackend::new();
    let result = backend.open_engine(make_opts("about:blank"));
    assert!(result.is_err(), "stub must return Err on non-Linux");
    let msg = result.unwrap_err();
    assert!(
        msg.to_lowercase().contains("linux"),
        "error message must mention Linux: {msg}"
    );
}

/// `as_any` downcast must succeed.
#[test]
fn backend_as_any_downcasts() {
    let backend = WebKitGtkBackend::new();
    let any = backend.as_any();
    assert!(
        any.downcast_ref::<WebKitGtkBackend>().is_some(),
        "as_any must downcast to WebKitGtkBackend"
    );
}

// ── Runtime tests (require Wayland/X11 session + WebKitGTK 6.0) ──────────────

/// `open_engine` must succeed and construct a `BrowserEngine` on Linux with
/// a display server available.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn open_engine_constructs() {
    let backend = WebKitGtkBackend::new();
    let result = backend.open_engine(make_opts("about:blank"));
    assert!(
        result.is_ok(),
        "open_engine must succeed on Linux with display"
    );
    let engine = result.unwrap();
    assert_eq!(engine.tab_count(), 1, "initial tab must be open");
    assert!(engine.active_tab().is_some(), "active_tab must be Some");
}

/// `osr_frame()` must return the same Arc on every call.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn osr_frame_arc_is_stable() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");
    let f1 = engine.osr_frame();
    let f2 = engine.osr_frame();
    assert!(
        Arc::ptr_eq(&f1, &f2),
        "osr_frame() must return the same Arc on every call"
    );
}

/// `osr_view()` must return the same Arc on every call.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn osr_view_arc_is_stable() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");
    let v1 = engine.osr_view();
    let v2 = engine.osr_view();
    assert!(
        Arc::ptr_eq(&v1, &v2),
        "osr_view() must return the same Arc on every call"
    );
}

/// `navigate` must propagate errors from the worker rather than silently
/// returning `Ok(())`.  With no active tab the worker returns an error.
///
/// NOTE: invalid URL strings (e.g. "invalid://bad") are not rejected at the
/// worker level — WebKitGTK's `load_uri` accepts arbitrary strings.  A
/// separate bug should be filed if URL validation is desired.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn navigate_with_no_active_tab_returns_err() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");

    // Close all tabs so there is no active tab.
    engine.close_all_browsers();

    let result = engine.navigate("https://example.com");
    assert!(
        result.is_err(),
        "navigate must return Err when no active tab exists, got: {result:?}"
    );
}

/// Verify the input dispatch chain is wired end-to-end and does not panic.
///
/// Opens a real WebKitGTK engine (requires a display server) then sends a key
/// event and a mouse click. The goal is to confirm that the translated event
/// reaches the GTK worker thread via `Command::SendInput` without panicking —
/// not that it lands in a live DOM (gdk4 0.11 synthetic dispatch is still a
/// TODO).
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn osr_input_dispatch_no_panic() {
    use buffr_engine::{BrowserEngine, KeyEventKind, MouseButton, NeutralKeyEvent};
    use std::sync::Arc;

    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");

    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::RawDown,
        windows_key_code: 65,
        character: b'a' as u16,
        ..Default::default()
    });
    engine.osr_mouse_click(100, 200, MouseButton::Left, false, 1, 0);
    engine.osr_mouse_click(100, 200, MouseButton::Left, true, 1, 0);

    // Give the GTK worker thread a moment to drain the command channel.
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(
        engine.tab_count() >= 1,
        "tab must still be open after input"
    );
}

/// After the GTK worker runs for ~500 ms on a green-background data: URL,
/// the OSR frame must contain at least one pixel that is not solid white.
///
/// This validates the real `WebView::snapshot()` → `gdk::Texture::download()`
/// pipeline. The white pixel check is intentionally loose — we only prove that
/// *something* was rendered, not that it is pixel-perfect.
///
/// Marked `#[ignore]` because it requires a live Wayland/X11 display and a
/// working WebKitGTK 6.0 installation.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn osr_frame_not_all_white_after_snapshot() {
    const GREEN_URL: &str = "data:text/html,<body style=\"background:lime\"><h1>Hi</h1></body>";

    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts(GREEN_URL))
        .expect("open_engine on green data: URL");

    // Give the GTK worker enough time for:
    //   - gtk4::init() + WebView creation
    //   - data: URL parse + render
    //   - at least two 250 ms snapshot ticks
    std::thread::sleep(std::time::Duration::from_millis(700));

    let frame = engine.osr_frame();
    let guard = frame.lock().expect("osr_frame lock");

    // The frame must have been populated (non-zero dimensions).
    assert!(
        guard.width > 0 && guard.height > 0,
        "osr_frame must have non-zero dimensions after snapshot ({}×{})",
        guard.width,
        guard.height,
    );
    assert!(
        !guard.pixels.is_empty(),
        "osr_frame pixel buffer must not be empty"
    );

    // At least one pixel must differ from solid white (0xFF, 0xFF, 0xFF, 0xFF).
    let all_white = guard
        .pixels
        .chunks_exact(4)
        .all(|p| p[0] == 0xFF && p[1] == 0xFF && p[2] == 0xFF && p[3] == 0xFF);

    assert!(
        !all_white,
        "osr_frame must contain at least one non-white pixel after snapshot \
         (frame is {}×{}, {} bytes)",
        guard.width,
        guard.height,
        guard.pixels.len(),
    );
}

/// Phase C input dispatch smoke test.
///
/// Opens an engine on a `data:` URL with an autofocused `<input>`, sends a
/// key event for 'X' (VK 88), waits 200 ms for the GTK worker to drain the
/// command channel and fire the `evaluate_javascript` callback. Asserts no
/// panic and that the tab is still alive.
///
/// DOM-level assertion (checking the input value via a second JS evaluation)
/// is omitted; that belongs to Phase D. The goal here is to confirm the full
/// JS injection path is wired end-to-end without panic.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn osr_input_dispatch_evaluates_js() {
    use buffr_engine::{BrowserEngine, KeyEventKind, NeutralKeyEvent};
    use std::sync::Arc;

    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("data:text/html,<input id=x autofocus>"))
        .expect("open_engine on autofocus input URL");

    // Allow the GTK worker to create the WebView and load the data: URL.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Send a key event for 'X' (VK 88). The worker maps it to JS and calls
    // evaluate_javascript on the active WebView — fire-and-forget.
    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::RawDown,
        windows_key_code: 88, // VK for 'X' / 'x'
        character: b'x' as u16,
        ..Default::default()
    });

    // Wait for the GTK 10 ms poll tick + evaluate_javascript async callback.
    std::thread::sleep(std::time::Duration::from_millis(200));

    assert!(
        engine.tab_count() >= 1,
        "tab must still be open after JS input dispatch"
    );
}

// ── IME smoke tests (require display server + WebKitGTK 6.0) ─────────────────

/// `ime_set_composition` dispatches a CompositionEvent via JS injection.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn ime_set_composition_no_panic() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("data:text/html,<input id=i>"))
        .expect("open_engine");
    engine.ime_set_composition("日本語", Some((0, 9)));
    engine.ime_set_composition("にほんご", None);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(engine.tab_count() >= 1);
}

/// `ime_commit` dispatches compositionend + execCommand('insertText') via JS.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn ime_commit_no_panic() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("data:text/html,<input id=i>"))
        .expect("open_engine");
    engine.ime_set_composition("te", Some((0, 2)));
    engine.ime_commit("test");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(engine.tab_count() >= 1);
}

/// `ime_cancel` dispatches compositionend with empty data via JS.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn ime_cancel_no_panic() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("data:text/html,<input id=i>"))
        .expect("open_engine");
    engine.ime_set_composition("te", None);
    engine.ime_cancel();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(engine.tab_count() >= 1);
}

/// IME JS snippet builder — set_composition produces valid CompositionEvent JS.
#[test]
#[cfg(target_os = "linux")]
fn ime_set_composition_js_shape() {
    use buffr_webkitgtk::ime_set_composition_js;
    let js = ime_set_composition_js("日本語", Some((0, 9)));
    assert!(
        js.contains("compositionupdate"),
        "missing compositionupdate: {js}"
    );
    assert!(js.contains("日本語"), "missing text: {js}");
}

/// IME JS snippet builder — commit produces compositionend + execCommand.
#[test]
#[cfg(target_os = "linux")]
fn ime_commit_js_shape() {
    use buffr_webkitgtk::ime_commit_js;
    let js = ime_commit_js("test");
    assert!(
        js.contains("compositionend"),
        "missing compositionend: {js}"
    );
    assert!(js.contains("insertText"), "missing insertText: {js}");
    assert!(js.contains("test"), "missing commit text: {js}");
}

/// IME JS snippet builder — cancel produces compositionend with empty data.
#[test]
#[cfg(target_os = "linux")]
fn ime_cancel_js_shape() {
    use buffr_webkitgtk::ime_cancel_js;
    let js = ime_cancel_js();
    assert!(
        js.contains("compositionend"),
        "missing compositionend: {js}"
    );
}

/// This test uses a separate engine instance to avoid GTK re-init issues.
#[test]
#[ignore = "requires display server and WebKitGTK 6.0"]
fn navigate_after_worker_death_returns_err() {
    let backend = WebKitGtkBackend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");

    // Keep a second Arc clone, drop the first to send Shutdown.
    let engine2 = Arc::clone(&engine);
    drop(engine);
    // Give the worker a moment to process the Shutdown command.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // navigate on engine2 — the worker has exited; the reply channel will time
    // out and the timeout maps to WorkerTimeout → EngineError::Other.
    let result = engine2.navigate("https://example.com");
    assert!(
        result.is_err(),
        "navigate must return Err after worker shutdown, got: {result:?}"
    );
}
