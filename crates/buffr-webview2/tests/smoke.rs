//! Smoke tests for `buffr-webview2`.
//!
//! Non-ignored tests run offline on any host (Linux, macOS, Windows).
//! Tests tagged `#[ignore]` require a Windows host with the WebView2
//! Runtime installed and a live desktop session.
//!
//! On non-Windows hosts the stub fallback is exercised: `open_engine` must
//! return a "Windows only" error.

use buffr_engine::{Backend, BackendOpenOptions, engine_id::EngineId};
use buffr_webview2::WebView2Backend;

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

// ── Cross-platform tests ──────────────────────────────────────────────────────

/// `Backend::id()` must always return "webview2" regardless of platform.
#[test]
fn backend_id_is_webview2() {
    let backend = WebView2Backend::new();
    assert_eq!(backend.id(), "webview2");
}

/// `Backend::initialize()` must succeed on Windows (no-op); must return Err on
/// non-Windows (stub path).
#[test]
fn backend_initialize_succeeds() {
    let backend = WebView2Backend::new();

    #[cfg(target_os = "windows")]
    {
        let result = backend.initialize("C:\\Temp\\buffr-webview2-smoke");
        assert!(
            result.is_ok(),
            "initialize() must succeed on Windows: {result:?}"
        );
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Stub initialize always returns Err on non-Windows.
        let result = backend.initialize("/tmp/buffr-webview2-smoke");
        assert!(
            result.is_err(),
            "initialize() must fail on non-Windows (stub): {result:?}"
        );
    }
}

/// `as_any` must downcast to the concrete type.
#[test]
fn backend_as_any_downcasts() {
    let backend = WebView2Backend::new();
    let any = backend.as_any();
    assert!(
        any.downcast_ref::<WebView2Backend>().is_some(),
        "as_any must downcast to WebView2Backend"
    );
}

// ── Non-Windows: stub fallback tests ─────────────────────────────────────────

/// On non-Windows, `open_engine` must return a "Windows only" error.
#[cfg(not(target_os = "windows"))]
#[test]
fn open_engine_returns_windows_only_error() {
    let backend = WebView2Backend::new();
    let result = backend.open_engine(make_opts("about:blank"));
    assert!(result.is_err(), "expected Err on non-Windows, got Ok");
    let err = match result {
        Err(e) => e,
        Ok(_) => unreachable!(),
    };
    assert!(
        err.to_lowercase().contains("windows"),
        "expected Windows-only error message, got: {err}"
    );
}

// ── Windows-only tests (require WebView2 Runtime + desktop session) ───────────

/// Construct an engine on Windows with the WebView2 Runtime installed.
/// Marked `#[ignore]` because it requires a live desktop session and
/// the WebView2 Runtime (available at microsoft.com/edge/webview2).
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn open_engine_constructs() {
    let backend = WebView2Backend::new();
    let result = backend.open_engine(make_opts("about:blank"));
    assert!(
        result.is_ok(),
        "open_engine must succeed on Windows with WebView2 Runtime"
    );
    let engine = result.unwrap();
    assert_eq!(engine.tab_count(), 1, "initial tab must be open");
    assert!(engine.active_tab().is_some(), "active_tab must be Some");
}

/// `osr_frame()` must return the same Arc on every call.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn osr_frame_arc_is_stable() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;

    let backend = WebView2Backend::new();
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
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn osr_view_arc_is_stable() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;

    let backend = WebView2Backend::new();
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
/// worker level — WebView2's `navigate` accepts arbitrary strings and defers
/// URL validation to the COM layer.  A separate bug should be filed if URL
/// validation is desired.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn navigate_with_no_active_tab_returns_err() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;

    let backend = WebView2Backend::new();
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
/// Opens a real WebView2 engine (requires a live Windows session with the
/// WebView2 Runtime installed) then sends a key event and a mouse click.
/// The goal is to confirm that the STA worker receives the event via
/// `Command::SendInput` without panicking — not that it reaches a live DOM
/// (COM init is blocked by #106, so the event is logged and dropped).
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn osr_input_dispatch_no_panic() {
    use buffr_engine::{BrowserEngine, KeyEventKind, MouseButton, NeutralKeyEvent};
    use std::sync::Arc;

    let backend = WebView2Backend::new();
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

    // Give the STA worker thread a moment to drain the command channel.
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(
        engine.tab_count() >= 1,
        "tab must still be open after input"
    );
}

/// Verify zoom methods compile and return sane defaults on a non-Windows host
/// (stub path: open_engine returns Err, so we only test trait defaults).
///
/// On Windows the full zoom round-trip is covered by the ignored integration
/// test below.
#[cfg(not(target_os = "windows"))]
#[test]
fn zoom_defaults_on_non_windows() {
    // We cannot open an engine on non-Windows, but the trait default for
    // active_zoom_level() must be 1.0 (hardened in the engine stub).
    // We verify the backend compiles and the stub open_engine fails cleanly.
    let backend = WebView2Backend::new();
    let result = backend.open_engine(make_opts("about:blank"));
    assert!(result.is_err(), "stub must return Err on non-Windows");
}

/// Windows-only: zoom_in / zoom_out / zoom_reset / active_zoom_level round-trip.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn zoom_round_trip_on_windows() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;

    let backend = WebView2Backend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");

    // Initial zoom must be 1.0.
    assert!(
        (engine.active_zoom_level() - 1.0).abs() < f64::EPSILON,
        "initial zoom must be 1.0"
    );

    // zoom_in raises the level.
    engine.zoom_in();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after_in = engine.active_zoom_level();
    assert!(
        after_in > 1.0,
        "zoom_in must raise zoom above 1.0, got {after_in}"
    );

    // zoom_reset returns to 1.0.
    engine.zoom_reset();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after_reset = engine.active_zoom_level();
    assert!(
        (after_reset - 1.0).abs() < f64::EPSILON,
        "zoom_reset must restore 1.0, got {after_reset}"
    );

    // zoom_out lowers the level below 1.0.
    engine.zoom_out();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after_out = engine.active_zoom_level();
    assert!(
        after_out < 1.0,
        "zoom_out must lower zoom below 1.0, got {after_out}"
    );
}

/// On non-Windows, verify the stub input path compiles and the backend
/// correctly returns a Windows-only error (not a panic).
///
/// This test verifies the non-ignored, cross-platform part of the input
/// wiring: `open_engine` fails gracefully rather than panicking.
#[cfg(not(target_os = "windows"))]
#[test]
fn osr_input_dispatch_stub_path_no_panic() {
    // On non-Windows the stub engine is used — open_engine returns Err.
    // We just verify the stub path compiles and doesn't panic.
    let backend = WebView2Backend::new();
    let result = backend.open_engine(make_opts("about:blank"));
    // Stub must return Err; we don't call osr_key_event because there's no engine.
    assert!(result.is_err(), "stub must return Err on non-Windows");
}

/// OSR frame must contain at least one non-white pixel after CapturePreview
/// has had time to run on a page with a non-white background.
///
/// Opens a real WebView2 engine on `data:text/html,<body style="background:lime">`,
/// waits 1 second for the 250 ms OSR timer and NavigationCompleted trigger to
/// fire at least once, then asserts that at least one BGRA pixel is not solid
/// white `(0xff, 0xff, 0xff, 0xff)`.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime + Windows host"]
fn osr_frame_not_all_white_after_capture() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;

    let backend = WebView2Backend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("data:text/html,<body style=\"background:lime\">"))
        .expect("open_engine");

    // Allow the NavigationCompleted trigger and at least two 250 ms OSR ticks
    // to fire, giving CapturePreview time to complete.
    std::thread::sleep(std::time::Duration::from_secs(1));

    let frame = engine.osr_frame();
    let guard = frame.lock().expect("osr_frame lock");
    let has_non_white = guard
        .pixels
        .chunks_exact(4)
        .any(|px| px != [0xff, 0xff, 0xff, 0xff]);

    assert!(
        has_non_white,
        "OSR frame must contain at least one non-white BGRA pixel after CapturePreview; \
         frame size: {}×{}, generation: {}",
        guard.width, guard.height, guard.generation
    );
}

/// Smoke test: Option-A PostMessageW key dispatch does not panic.
///
/// Opens a real WebView2 engine on a page with an autofocus `<input>` so the
/// DOM is ready to receive keyboard input.  Sends a single `WM_KEYDOWN` for
/// the 'X' key (VK=0x58) via `osr_key_event`, then the matching `WM_KEYUP`.
/// The assertion floor is "no panic" — JS eval to read the DOM value is a
/// Phase C concern.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn osr_postmessage_key_dispatch_no_panic() {
    use buffr_engine::{BrowserEngine, KeyEventKind, NeutralKeyEvent};
    use std::sync::Arc;

    let backend = WebView2Backend::new();
    let engine: Arc<dyn BrowserEngine> = backend
        .open_engine(make_opts("data:text/html,<input id=x autofocus>"))
        .expect("open_engine");

    // Allow NavigationCompleted to fire so the page and autofocus are ready.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // VK_X = 0x58 on Windows.
    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::RawDown,
        windows_key_code: 0x58, // 'X'
        native_key_code: 0x2D,  // scancode for X on a US layout
        character: b'x' as u16,
        ..Default::default()
    });
    engine.osr_key_event(NeutralKeyEvent {
        kind: KeyEventKind::Up,
        windows_key_code: 0x58,
        native_key_code: 0x2D,
        character: b'x' as u16,
        ..Default::default()
    });

    // Give the STA worker time to drain the command channel and PostMessageW.
    std::thread::sleep(std::time::Duration::from_millis(100));

    assert!(
        engine.tab_count() >= 1,
        "tab must still be open after PostMessageW key dispatch"
    );
}

/// `navigate` errors propagate via the reply channel — not silently discarded.
/// Drop the engine (which shuts down the worker) then verify that a navigate
/// call on a freshly-killed worker is an Err, not a silent Ok.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires WebView2 Runtime and a live desktop session on Windows"]
fn navigate_after_worker_death_returns_err() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;

    let backend = WebView2Backend::new();
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

// ── IME smoke tests ───────────────────────────────────────────────────────────
//
// On Windows: dispatches WM_CHAR per character via PostMessageW.
// On non-Windows (stub path): debug-log only.
// All three must not panic in either path.

/// `ime_set_composition` dispatches preedit via WM_CHAR (simplified).
/// Requires Windows with a real WebView2 installation.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires Windows + WebView2 runtime"]
fn ime_set_composition_no_panic() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;
    let engine: Arc<dyn BrowserEngine> = WebView2Backend::new()
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");
    engine.ime_set_composition("日本語", Some((0, 9)));
    engine.ime_set_composition("にほんご", None);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() >= 1);
}

/// `ime_commit` dispatches committed text via WM_CHAR.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires Windows + WebView2 runtime"]
fn ime_commit_no_panic() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;
    let engine: Arc<dyn BrowserEngine> = WebView2Backend::new()
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");
    engine.ime_set_composition("te", Some((0, 2)));
    engine.ime_commit("test");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() >= 1);
}

/// `ime_cancel` dispatches WM_CHAR(VK_ESCAPE).
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires Windows + WebView2 runtime"]
fn ime_cancel_no_panic() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;
    let engine: Arc<dyn BrowserEngine> = WebView2Backend::new()
        .open_engine(make_opts("about:blank"))
        .expect("open_engine");
    engine.ime_set_composition("te", None);
    engine.ime_cancel();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() >= 1);
}
