//! Off-screen rendering shared frame buffer types (engine-agnostic).
//!
//! Identical to the types in `buffr-core::osr` but without any CEF
//! dependency. `buffr-cef` re-exports these through its own `osr`
//! module so callers don't need to import from both crates.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A single captured OSR frame.
///
/// `pixels` is raw BGRA, 4 bytes/pixel; length == `width * height * 4`.
/// `generation` is bumped on every successful paint so consumers can
/// skip compositing when nothing changed.
pub struct OsrFrame {
    pub width: u32,
    pub height: u32,
    /// BGRA pixels from the renderer; length = width * height * 4.
    pub pixels: Vec<u8>,
    /// Bumped on every successful on_paint so consumers can skip
    /// compositing when nothing changed.
    pub generation: u64,
    /// Set by `osr_resize`, cleared by the next paint. Forces consumers
    /// to wait for a real post-resize paint before trusting dims.
    pub needs_fresh: bool,
}

impl OsrFrame {
    /// Allocate a zeroed frame of the given dimensions.
    pub fn new(width: u32, height: u32) -> Self {
        let len = (width as usize) * (height as usize) * 4;
        Self {
            width,
            height,
            pixels: vec![0u8; len],
            generation: 0,
            needs_fresh: false,
        }
    }
}

/// Thread-safe shared frame buffer.
pub type SharedOsrFrame = Arc<Mutex<OsrFrame>>;

/// Viewport dimensions + device scale factor, readable from any thread.
///
/// All values are accessed with `Ordering::Relaxed` — they are written
/// from the UI thread and read from the engine IO thread.
pub struct OsrViewState {
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Device scale factor stored as thousandths (e.g. 1000 = 1.0×).
    pub scale: AtomicU32,
    /// Target frame rate in Hz (default 60).
    pub frame_rate_hz: AtomicU32,
    /// Whether the surface is sleeping (was_hidden). Written by the
    /// OSR sleep path; read by the engine's paint scheduler.
    pub sleeping: AtomicBool,
    /// Optional wake callback. Fired on every paint so the embedder
    /// can nudge its UI loop (e.g. winit EventLoopProxy::send_event).
    /// First setter wins; subsequent calls are silently ignored.
    pub wake: OnceLock<Arc<dyn Fn() + Send + Sync>>,
    /// CEF browser id for the tab owning this view. Set at construction.
    pub main_id: AtomicI32,
}

impl OsrViewState {
    pub fn new() -> Self {
        Self {
            width: AtomicU32::new(800),
            height: AtomicU32::new(600),
            scale: AtomicU32::new(1000),
            frame_rate_hz: AtomicU32::new(60),
            sleeping: AtomicBool::new(false),
            wake: OnceLock::new(),
            main_id: AtomicI32::new(-1),
        }
    }

    pub fn scale(&self) -> f32 {
        self.scale.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn set_scale(&self, scale: f32) {
        let v = (scale * 1000.0).round().max(0.0) as u32;
        self.scale.store(v, Ordering::Relaxed);
    }
}

impl Default for OsrViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared viewport state.
pub type SharedOsrViewState = Arc<OsrViewState>;

/// Per-popup OSR frame map: `browser_id → (frame, view)`.
pub type PopupFrameMap =
    Arc<Mutex<std::collections::HashMap<i32, (SharedOsrFrame, SharedOsrViewState)>>>;
