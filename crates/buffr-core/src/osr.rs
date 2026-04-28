//! Off-screen rendering (OSR) shared frame buffer + RenderHandler.
//!
//! ## Architecture
//!
//! CEF's OSR path skips all windowed embedding:
//!
//! ```text
//!   +--------------+    on_paint(BGRA, w, h)    +----------------------+
//!   |  CEF (OSR)   | -------------------------> |  OsrPaintHandler     |
//!   |  no window   |                            |  → SharedOsrFrame    |
//!   +--------------+                            +----------+-----------+
//!                                                          |
//!                                                          v
//!                                              +-----------+----------+
//!                                              |   step 4 compositor  |
//!                                              |   (winit surface)    |
//!                                              +----------------------+
//! ```
//!
//! [`OsrPaintHandler`] implements CEF's `RenderHandler` trait. It writes
//! raw BGRA pixels into a [`SharedOsrFrame`] on every `on_paint` call and
//! bumps a monotonic `generation` counter so downstream compositors can
//! skip work when nothing changed.
//!
//! [`OsrViewState`] holds the current viewport dimensions as atomics so
//! both the CEF IO thread (reading from `view_rect`) and the UI thread
//! (writing via `BrowserHost::osr_resize`) can access them without a mutex.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cef::*;

// ── Shared data types ──────────────────────────────────────────────────────────

/// A single captured OSR frame.
///
/// The `pixels` buffer is raw BGRA, 4 bytes/pixel; length == `width * height * 4`.
/// `generation` is bumped on every successful [`on_paint`] so consumers can
/// skip compositing when nothing changed.
pub struct OsrFrame {
    pub width: u32,
    pub height: u32,
    /// BGRA pixels straight from CEF; length = width * height * 4.
    pub pixels: Vec<u8>,
    /// Bumped on every successful on_paint so consumers can skip composite
    /// when nothing changed.
    pub generation: u64,
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
        }
    }
}

/// Thread-safe shared frame buffer.
pub type SharedOsrFrame = Arc<Mutex<OsrFrame>>;

/// Viewport dimensions + device scale factor, readable from any thread.
///
/// All values are accessed with `Ordering::Relaxed` — they are written from
/// the UI thread and read from the CEF IO thread. Tearing is not a concern
/// because each field is a single 32-bit atomic; slight lag between a width
/// and height write is acceptable (CEF will call `view_rect` again).
pub struct OsrViewState {
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Device scale factor stored as thousandths (e.g. 1000 = 1.0×, 1500 = 1.5×).
    pub scale: AtomicU32,
}

impl OsrViewState {
    /// Default viewport: 1280×800, scale 1.0×.
    pub fn new() -> Self {
        Self {
            width: AtomicU32::new(1280),
            height: AtomicU32::new(800),
            scale: AtomicU32::new(1000),
        }
    }
}

impl Default for OsrViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared viewport state.
pub type SharedOsrViewState = Arc<OsrViewState>;

// ── RenderHandler impl ─────────────────────────────────────────────────────────

wrap_render_handler! {
    pub struct OsrPaintHandler {
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            rect.x = 0;
            rect.y = 0;
            rect.width = self.view.width.load(Ordering::Relaxed) as i32;
            rect.height = self.view.height.load(Ordering::Relaxed) as i32;
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            view_x: ::std::os::raw::c_int,
            view_y: ::std::os::raw::c_int,
            screen_x: Option<&mut ::std::os::raw::c_int>,
            screen_y: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            // No multi-monitor positioning yet — view coords == screen coords.
            if let Some(sx) = screen_x {
                *sx = view_x;
            }
            if let Some(sy) = screen_y {
                *sy = view_y;
            }
            1
        }

        // The `buffer` raw pointer is provided by CEF and is valid for
        // `width * height * 4` bytes for the duration of this call. The
        // lint fires because the trait method signature contains `*const u8`,
        // but the safety obligation is on CEF, not on our call site.
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // Only handle the main View paint. Popup compositing is deferred.
            if type_.get_raw() != PaintElementType::VIEW.get_raw() {
                tracing::trace!("osr: on_paint Popup — deferred (TODO: composite popup)");
                return;
            }

            let w = width as u32;
            let h = height as u32;
            let len = (w as usize) * (h as usize) * 4;

            // SAFETY: CEF guarantees `buffer` points to `width * height * 4`
            // valid bytes for the duration of this call.
            let src = unsafe { std::slice::from_raw_parts(buffer, len) };

            let Ok(mut guard) = self.frame.lock() else {
                tracing::warn!("osr: on_paint — frame mutex poisoned, skipping");
                return;
            };

            // Resize the backing buffer only when dimensions change.
            if guard.width != w || guard.height != h {
                guard.pixels.resize(len, 0);
                guard.width = w;
                guard.height = h;
            }

            guard.pixels.copy_from_slice(src);
            guard.generation = guard.generation.wrapping_add(1);
        }
    }
}
