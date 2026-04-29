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
//!
//! ## Multi-browser routing
//!
//! `OsrPaintHandler` is created once per CEF `Client` (one per tab) but
//! popup browsers created by `on_before_popup` share the same client path;
//! their browser id differs from the main tab's id.  The handler stores the
//! main browser id at construction time and a shared map of popup
//! `(frame, view)` pairs. On every CEF callback it routes by
//! `browser.identifier()`:
//!
//! - matches `main_id` → use main frame / view
//! - found in `popup_frames` → use that pair
//! - unknown → skip with a trace log

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
    /// CEF `windowless_frame_rate` to use when creating new browsers
    /// and (via `BrowserHost::set_frame_rate`) when retargeting live
    /// ones. Default 60. CEF clamps to its own max (typically 60 in
    /// CEF 147; future builds may go higher).
    pub frame_rate_hz: AtomicU32,
    /// Optional callback invoked from `on_paint` after a frame lands. The
    /// embedder uses this to wake the winit event loop (via
    /// `EventLoopProxy`) so the UI can pump a redraw without polling.
    /// Set once at startup via [`crate::BrowserHost::set_osr_wake`] or
    /// via [`OsrViewState::set_wake`].
    pub wake: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl OsrViewState {
    /// Default viewport: 1280×800, scale 1.0×.
    pub fn new() -> Self {
        Self {
            width: AtomicU32::new(1280),
            height: AtomicU32::new(800),
            scale: AtomicU32::new(1000),
            frame_rate_hz: AtomicU32::new(60),
            wake: OnceLock::new(),
        }
    }

    /// Install the wake callback. First call wins; subsequent calls are
    /// silently ignored (matches `BrowserHost::set_osr_wake` semantics).
    pub fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.wake.set(wake);
    }
}

impl Default for OsrViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared viewport state.
pub type SharedOsrViewState = Arc<OsrViewState>;

/// Map of popup browser OSR state, keyed by CEF `browser.identifier()`.
/// Shared between `BrowserHost` (which inserts/removes entries) and
/// `OsrPaintHandler` (which reads them on CEF IO callbacks).
pub type PopupFrameMap = Arc<Mutex<HashMap<i32, (SharedOsrFrame, SharedOsrViewState)>>>;

// ── RenderHandler impl ─────────────────────────────────────────────────────────

wrap_render_handler! {
    pub struct OsrPaintHandler {
        main_id: Arc<AtomicI32>,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        popup_frames: PopupFrameMap,
    }

    impl RenderHandler {
        fn view_rect(&self, browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let (w, h) = self.resolve_dims(browser.as_deref().map(|b| b.identifier()));
            rect.x = 0;
            rect.y = 0;
            rect.width = w as i32;
            rect.height = h as i32;
            tracing::debug!(browser_id = ?browser.as_deref().map(|b| b.identifier()), w = rect.width, h = rect.height, "osr: view_rect queried");
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
            browser: Option<&mut Browser>,
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

            let browser_id = browser.as_deref().map(|b| b.identifier());
            let w = width as u32;
            let h = height as u32;
            let len = (w as usize) * (h as usize) * 4;
            tracing::trace!(w, h, ?browser_id, "osr: on_paint fired");

            // SAFETY: CEF guarantees `buffer` points to `width * height * 4`
            // valid bytes for the duration of this call.
            let src = unsafe { std::slice::from_raw_parts(buffer, len) };

            // Route to the correct (frame, view) pair.
            let (frame, view) = match self.resolve_frame_view(browser_id) {
                Some(pair) => pair,
                None => {
                    tracing::trace!(
                        ?browser_id,
                        "osr: on_paint — unknown browser id, skipping"
                    );
                    return;
                }
            };

            let Ok(mut guard) = frame.lock() else {
                tracing::warn!("osr: on_paint — frame mutex poisoned, skipping");
                return;
            };

            // Resize the backing buffer when dimensions change OR when the
            // buffer length doesn't match expected — the embedder may have
            // taken/swapped the Vec out (mem::swap with a scratch buffer)
            // and left this side with an empty Vec while dims are unchanged.
            if guard.width != w || guard.height != h || guard.pixels.len() != len {
                if guard.width != w || guard.height != h {
                    tracing::debug!(
                        old_w = guard.width,
                        old_h = guard.height,
                        new_w = w,
                        new_h = h,
                        "osr: on_paint dimension change",
                    );
                }
                guard.pixels.resize(len, 0);
                guard.width = w;
                guard.height = h;
            }

            guard.pixels.copy_from_slice(src);
            guard.generation = guard.generation.wrapping_add(1);
            drop(guard);
            // Wake the embedder so the UI loop can pump a redraw.
            if let Some(wake) = view.wake.get() {
                wake();
            }
        }
    }
}

impl OsrPaintHandler {
    /// Resolve (width, height) for the given browser id.
    fn resolve_dims(&self, browser_id: Option<i32>) -> (u32, u32) {
        if let Some(id) = browser_id {
            // Check if this is the known main id.
            let main = self.main_id.load(Ordering::Relaxed);
            if main == id || main == -1 {
                // Set main_id lazily on first callback.
                if main == -1 {
                    self.main_id.store(id, Ordering::Relaxed);
                }
                return (
                    self.view.width.load(Ordering::Relaxed),
                    self.view.height.load(Ordering::Relaxed),
                );
            }
            // Check popup map.
            if let Ok(map) = self.popup_frames.lock()
                && let Some((_, popup_view)) = map.get(&id)
            {
                return (
                    popup_view.width.load(Ordering::Relaxed),
                    popup_view.height.load(Ordering::Relaxed),
                );
            }
        }
        // Fallback: use main view dims.
        (
            self.view.width.load(Ordering::Relaxed),
            self.view.height.load(Ordering::Relaxed),
        )
    }

    /// Resolve the (frame, view) pair for the given browser id.
    /// Returns `None` if the id is unknown (not main, not a popup).
    fn resolve_frame_view(
        &self,
        browser_id: Option<i32>,
    ) -> Option<(SharedOsrFrame, SharedOsrViewState)> {
        let id = browser_id?;
        let main = self.main_id.load(Ordering::Relaxed);
        // Lazily set main_id on first on_paint call.
        if main == -1 || main == id {
            if main == -1 {
                self.main_id.store(id, Ordering::Relaxed);
            }
            return Some((self.frame.clone(), self.view.clone()));
        }
        // Check popup map.
        if let Ok(map) = self.popup_frames.lock()
            && let Some((pf, pv)) = map.get(&id)
        {
            return Some((pf.clone(), pv.clone()));
        }
        None
    }
}

/// Construct a new [`OsrPaintHandler`] for a single main-tab browser.
///
/// `popup_frames` is shared with [`BrowserHost`] so popup entries can be
/// inserted/removed without rebuilding the handler.
pub fn make_osr_paint_handler(
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    popup_frames: PopupFrameMap,
) -> RenderHandler {
    OsrPaintHandler::new(Arc::new(AtomicI32::new(-1)), frame, view, popup_frames)
}
