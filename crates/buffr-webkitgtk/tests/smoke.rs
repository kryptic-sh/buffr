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
