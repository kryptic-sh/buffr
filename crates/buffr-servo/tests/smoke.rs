//! Smoke tests for the Servo backend.
//!
//! These tests verify that `ServoBackend::new()` constructs without panic and
//! that `initialize()` succeeds.  Tests that require engine init (which spawns
//! a Servo worker + SoftwareRenderingContext) are marked `#[ignore]` because:
//!
//!   1. `SoftwareRenderingContext::new()` may require an OpenGL / EGL context
//!      that is unavailable in headless CI environments without a GPU or X server.
//!   2. The Servo worker thread calls `ServoBuilder::build()` which touches
//!      gfx init paths that may panic without a display on some platforms.
//!
//! Run these locally with:
//!   cargo test -p buffr-servo -- --include-ignored

use buffr_engine::{Backend, BackendOpenOptions, EngineId};
use buffr_servo::ServoBackend;

fn dummy_options() -> BackendOpenOptions<'static> {
    BackendOpenOptions {
        engine_id: EngineId::new("servo-smoke"),
        data_dir: None,
        cache_dir: None,
        initial_url: "about:blank",
        frame_rate: 60,
        device_scale: 1.0,
        initial_size: (800, 600),
        private: false,
        download_dir: None,
        downloads: None,
        notice_queue: None,
        find_sink: None,
        sinks: Box::new(()),
    }
}

#[test]
fn servo_backend_new_and_id() {
    let backend = ServoBackend::new();
    assert_eq!(backend.id(), "servo");
}

#[test]
fn servo_backend_initialize_succeeds() {
    let backend = ServoBackend::new();
    let result = backend.initialize("/tmp/buffr-servo-smoke");
    assert!(result.is_ok(), "initialize() should succeed: {result:?}");
}

/// Full engine init — requires OpenGL / EGL and a display.
///
/// In Phase B skeleton mode this still succeeds (no real Servo init).
/// Marked `#[ignore]` to keep CI fast; re-enable when real Servo is wired.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_open_engine_constructs() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts);
    assert!(engine.is_ok(), "open_engine should succeed");
}

/// `active_zoom_level()` returns 1.0 before any zoom change.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_zoom_default_is_one() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");
    let zoom = engine.active_zoom_level();
    assert!(
        (zoom - 1.0).abs() < f64::EPSILON,
        "initial zoom must be 1.0, got {zoom}"
    );
}

/// `zoom_in()` raises the zoom level above 1.0.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_zoom_in_raises_level() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");
    engine.zoom_in();
    let after = engine.active_zoom_level();
    assert!(
        after > 1.0,
        "zoom_in must raise zoom above 1.0, got {after}"
    );
}

/// `zoom_reset()` restores the zoom level to 1.0.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_zoom_reset_restores_one() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");
    engine.zoom_in();
    engine.zoom_reset();
    let after = engine.active_zoom_level();
    assert!(
        (after - 1.0).abs() < f64::EPSILON,
        "zoom_reset must restore 1.0, got {after}"
    );
}

/// Verify `osr_frame` and `osr_view` return stable Arc handles.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_osr_handles_stable() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");

    // Multiple calls must return handles to the *same* underlying Arc.
    let frame_a = engine.osr_frame();
    let frame_b = engine.osr_frame();
    assert!(
        std::sync::Arc::ptr_eq(&frame_a, &frame_b),
        "osr_frame() must return the same Arc each call"
    );

    let view_a = engine.osr_view();
    let view_b = engine.osr_view();
    assert!(
        std::sync::Arc::ptr_eq(&view_a, &view_b),
        "osr_view() must return the same Arc each call"
    );
}
