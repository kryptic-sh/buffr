//! Smoke tests for the WebKit Cocoa backend.
//!
//! On non-macOS hosts only the stub fallback is exercised (no AppKit or WebKit
//! link dependency). macOS-only tests are marked `#[ignore]` because they
//! require a live AppKit run loop.

use buffr_engine::{Backend, BackendOpenOptions, engine_id::EngineId};
use buffr_webkit_cocoa::WebKitCocoaBackend;

// ── Cross-platform tests (exercises stub on Linux / macOS struct on macOS) ──

/// Backend id must always be "webkit-cocoa", regardless of platform.
#[test]
fn backend_id_is_webkit_cocoa() {
    let backend = WebKitCocoaBackend::new();
    assert_eq!(backend.id(), "webkit-cocoa");
}

/// `as_any` must downcast to the concrete type.
#[test]
fn backend_as_any_downcasts() {
    let backend = WebKitCocoaBackend::new();
    let any = backend.as_any();
    assert!(
        any.downcast_ref::<WebKitCocoaBackend>().is_some(),
        "as_any must downcast to WebKitCocoaBackend"
    );
}

/// On macOS `initialize` is a no-op and must return `Ok`.
/// On non-macOS the stub's `initialize` also returns an error.
#[cfg(target_os = "macos")]
#[test]
fn initialize_succeeds_on_macos() {
    let backend = WebKitCocoaBackend::new();
    let result = backend.initialize("/tmp/buffr-webkit-cocoa-smoke");
    assert!(
        result.is_ok(),
        "initialize() should succeed on macOS: {result:?}"
    );
}

// ── Non-macOS: stub fallback tests ───────────────────────────────────────────

/// On non-macOS, `open_engine` must return an error whose message contains
/// "macOS" so the apps layer can surface a clear diagnostic.
#[cfg(not(target_os = "macos"))]
#[test]
fn open_engine_returns_macos_only_error() {
    let backend = WebKitCocoaBackend::new();
    let opts = BackendOpenOptions {
        engine_id: EngineId::new("smoke-test"),
        initial_url: "about:blank",
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
    };
    let result = backend.open_engine(opts);
    assert!(result.is_err(), "expected Err on non-macOS, got Ok");
    let err = match result {
        Err(e) => e,
        Ok(_) => unreachable!(),
    };
    assert!(
        err.to_lowercase().contains("macos"),
        "expected macOS-only error, got: {err}"
    );
}

/// On non-macOS, verify the zoom API stubs compile cleanly.
/// (No engine to open on non-macOS; we just verify the backend rejects.)
#[cfg(not(target_os = "macos"))]
#[test]
fn zoom_stub_compiles_on_non_macos() {
    let backend = WebKitCocoaBackend::new();
    let opts = BackendOpenOptions {
        engine_id: EngineId::new("zoom-stub"),
        initial_url: "about:blank",
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
    };
    let result = backend.open_engine(opts);
    // Stub must return Err — zoom methods are unreachable but must compile.
    assert!(result.is_err(), "stub must return Err on non-macOS");
}

/// On non-macOS, `initialize` must also return an error.
#[cfg(not(target_os = "macos"))]
#[test]
fn initialize_returns_macos_only_error() {
    let backend = WebKitCocoaBackend::new();
    let result = backend.initialize("/tmp/buffr-webkit-cocoa-smoke");
    assert!(
        result.is_err(),
        "initialize() should fail on non-macOS, got Ok"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_lowercase().contains("macos"),
        "expected macOS-only error, got: {err}"
    );
}

// ── macOS-only tests (require main run loop) ─────────────────────────────────

/// Construct an engine on macOS. Marked `#[ignore]` because without a live
/// NSApp / run loop, WKWebView cannot be initialised safely.
///
/// To run on a real macOS host:
///   cargo test -p buffr-webkit-cocoa -- --ignored
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS main run loop (AppKit NSApp)"]
fn open_engine_constructs_on_macos() {
    let backend = WebKitCocoaBackend::new();
    let opts = BackendOpenOptions {
        engine_id: EngineId::new("smoke-macos"),
        initial_url: "about:blank",
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
    };
    let engine = backend
        .open_engine(opts)
        .expect("open_engine must succeed on macOS");
    // Engine must expose a stable OSR frame Arc.
    let f1 = engine.osr_frame();
    let f2 = engine.osr_frame();
    assert!(
        std::sync::Arc::ptr_eq(&f1, &f2),
        "osr_frame() must return the same Arc on every call"
    );
    // Engine must expose a stable OSR view Arc.
    let v1 = engine.osr_view();
    let v2 = engine.osr_view();
    assert!(
        std::sync::Arc::ptr_eq(&v1, &v2),
        "osr_view() must return the same Arc on every call"
    );
}

// ── IME smoke tests ───────────────────────────────────────────────────────────
//
// Phase B: all three IME methods are debug-log only (NSTextInputClient not yet
// wired; Phase D deferred). They must not panic on any supported target.

/// `ime_set_composition` must not panic (debug-log only in Phase B).
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS + real WKWebView runtime"]
fn ime_set_composition_no_panic() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;
    let opts = BackendOpenOptions {
        engine_id: EngineId::new("ime-smoke"),
        initial_url: "about:blank",
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
    };
    let engine: Arc<dyn BrowserEngine> = WebKitCocoaBackend::new()
        .open_engine(opts)
        .expect("open_engine");
    engine.ime_set_composition("日本語", Some((0, 9)));
    engine.ime_set_composition("にほんご", None);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() >= 1);
}

/// `ime_commit` must not panic (debug-log only in Phase B).
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS + real WKWebView runtime"]
fn ime_commit_no_panic() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;
    let opts = BackendOpenOptions {
        engine_id: EngineId::new("ime-smoke"),
        initial_url: "about:blank",
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
    };
    let engine: Arc<dyn BrowserEngine> = WebKitCocoaBackend::new()
        .open_engine(opts)
        .expect("open_engine");
    engine.ime_set_composition("te", Some((0, 2)));
    engine.ime_commit("test");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() >= 1);
}

/// `ime_cancel` must not panic (debug-log only in Phase B).
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS + real WKWebView runtime"]
fn ime_cancel_no_panic() {
    use buffr_engine::BrowserEngine;
    use std::sync::Arc;
    let opts = BackendOpenOptions {
        engine_id: EngineId::new("ime-smoke"),
        initial_url: "about:blank",
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
    };
    let engine: Arc<dyn BrowserEngine> = WebKitCocoaBackend::new()
        .open_engine(opts)
        .expect("open_engine");
    engine.ime_set_composition("te", None);
    engine.ime_cancel();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() >= 1);
}
