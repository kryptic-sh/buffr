//! Smoke tests for the Ladybird backend.
//!
//! Validates that:
//! - `LadybirdBackend::new()` + `initialize()` succeed
//! - `open_engine()` constructs without error
//! - `open_tab()` / `close_tab()` correctly track the count
//! - `navigate()` updates the active tab URL
//! - `osr_frame()` / `osr_view()` return stable `Arc` handles
//! - The OSR frame has pixels after `open_engine`
//! - `backend.id()` returns `"ladybird"`

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, engine_id::EngineId};
use buffr_ladybird::LadybirdBackend;

/// Helper: open an engine at the given URL.
fn open_engine(url: &str) -> Arc<dyn BrowserEngine> {
    let backend = LadybirdBackend::new();
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
        download_dir: None,
        downloads: None,
        notice_queue: None,
        find_sink: None,
        sinks: Box::new(()),
    };
    backend.open_engine(opts).expect("open_engine failed")
}

#[test]
fn backend_initialize_succeeds() {
    let backend = LadybirdBackend::new();
    let result = backend.initialize("/tmp/buffr-ladybird-smoke");
    assert!(result.is_ok(), "initialize() should return Ok: {result:?}");
}

#[test]
fn open_engine_constructs() {
    // Must not panic or return Err.
    let engine = open_engine("about:blank");
    // After construction the worker opens the initial tab.
    assert_eq!(
        engine.tab_count(),
        1,
        "expected exactly 1 tab after open_engine"
    );
    assert!(engine.active_tab().is_some(), "active_tab() should be Some");
}

#[test]
fn open_tab_increments_count() {
    let engine = open_engine("about:blank");
    assert_eq!(engine.tab_count(), 1);
    engine.open_tab("about:blank").expect("open_tab failed");
    assert_eq!(engine.tab_count(), 2);
}

#[test]
fn close_tab_decrements_count() {
    let engine = open_engine("about:blank");
    let id = engine.open_tab("about:blank").expect("open_tab failed");
    assert_eq!(engine.tab_count(), 2);
    engine.close_tab(id).expect("close_tab failed");
    assert_eq!(engine.tab_count(), 1);
}

#[test]
fn navigate_updates_url() {
    let engine = open_engine("about:blank");
    engine
        .navigate("https://example.com")
        .expect("navigate failed");
    let url = engine.active_tab_live_url();
    assert_eq!(
        url, "https://example.com",
        "URL should update after navigate"
    );
}

#[test]
fn osr_frame_arc_is_stable() {
    let engine = open_engine("about:blank");
    let frame_a = engine.osr_frame();
    let frame_b = engine.osr_frame();
    assert!(
        Arc::ptr_eq(&frame_a, &frame_b),
        "osr_frame() must return the same Arc on every call"
    );
}

#[test]
fn osr_view_arc_is_stable() {
    let engine = open_engine("about:blank");
    let view_a = engine.osr_view();
    let view_b = engine.osr_view();
    assert!(
        Arc::ptr_eq(&view_a, &view_b),
        "osr_view() must return the same Arc on every call"
    );
}

#[test]
fn osr_frame_has_pixels_after_open() {
    let engine = open_engine("about:blank");
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
fn backend_id_is_ladybird() {
    let backend = LadybirdBackend::new();
    assert_eq!(backend.id(), "ladybird");
}
