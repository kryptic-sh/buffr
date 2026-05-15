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
