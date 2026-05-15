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
