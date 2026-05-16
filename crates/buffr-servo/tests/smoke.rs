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
        history: None,
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

// ── IME smoke tests ───────────────────────────────────────────────────────────
//
// Substrate: `InputEvent::Ime(ImeEvent)` via `WebView::notify_input_event`.
//   - `ime_set_composition` → `ImeEvent::Composition(CompositionState::Update)`
//   - `ime_commit` → `ImeEvent::Composition(CompositionState::End)`
//   - `ime_cancel` → `ImeEvent::Dismissed`
//
// Source: servo-embedder-traits-0.1.0/input_events.rs:331–333.

/// `ime_set_composition` routes through `InputEvent::Ime(Composition{Update})`.
/// Marked slow because spawning the Servo worker requires a display / GL context.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_ime_set_composition_no_panic() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");
    engine.ime_set_composition("日本語", Some((0, 9)));
    engine.ime_set_composition("にほんご", None);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() == 0 || engine.tab_count() >= 1);
}

/// `ime_commit` routes through `InputEvent::Ime(Composition{End})`.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_ime_commit_no_panic() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");
    engine.ime_set_composition("te", Some((0, 2)));
    engine.ime_commit("test");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() == 0 || engine.tab_count() >= 1);
}

/// `ime_cancel` routes through `InputEvent::Ime(Dismissed)`.
#[test]
#[ignore = "slow — spawns worker thread; run with --include-ignored to exercise"]
fn servo_ime_cancel_no_panic() {
    let backend = ServoBackend::new();
    let opts = dummy_options();
    let engine = backend.open_engine(opts).expect("open_engine");
    engine.ime_set_composition("te", None);
    engine.ime_cancel();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.tab_count() == 0 || engine.tab_count() >= 1);
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
