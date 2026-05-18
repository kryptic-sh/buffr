//! Rust bridge for the C-side `BuffrDisplay`/`BuffrView`/`BuffrToplevel`/
//! `BuffrScreen` GObject subclasses defined in `csrc/wpe_subclasses.c`.
//!
//! The C side handles GType registration + vmethod plumbing. The only
//! per-frame work in Rust is [`buffr_rust_render_buffer`], which converts
//! the WebKit-delivered [`WPEBuffer`] into ARGB pixels and copies them into
//! the shared [`OsrFrame`]. Per-view state (the OsrFrame, wake callback,
//! tracing tag) is associated via `g_object_set_data_full` keyed by the
//! quark returned by [`view_ctx_quark`].
//!
//! Lifetime: each [`ViewCtx`] is heap-allocated and handed to the WPEView
//! via `g_object_set_data_full`; GLib drops it (running our destroy
//! callback) when the view is finalised, which lets us free the box safely.

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Send wrapper around `*mut WPEBuffer`. The pointer is only ever
/// dereferenced on the GLib worker thread; the wrapper exists so that
/// [`ViewCtx`] can carry a held buffer behind a `Mutex` (which requires
/// `Send`). The buffer itself is a GLib-refcounted object owned by
/// WebKit's buffer pool — holding the pointer keeps it alive until we
/// call `wpe_view_buffer_rendered`, which releases our slot in the pool.
#[repr(transparent)]
pub(crate) struct WpeBufferPtr(pub(crate) *mut WPEBuffer);
unsafe impl Send for WpeBufferPtr {}

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

use super::ffi::*;

// ── extern bindings to the C side ────────────────────────────────────────────

unsafe extern "C" {
    /// Constructor for our WPEDisplay subclass. Stores `egl_display` for
    /// the `get_egl_display` vmethod; viewport / scale / refresh feed the
    /// single BuffrScreen the display reports.
    pub fn buffr_display_new(
        egl_display: *mut c_void,
        viewport_w: std::os::raw::c_int,
        viewport_h: std::os::raw::c_int,
        scale: std::os::raw::c_double,
        refresh_hz: std::os::raw::c_int,
    ) -> *mut WPEDisplay;

    /// Atomic getter for the most recently created `WPEView` (returned
    /// from our `create_view` vmethod). Used by Rust to grab a borrowed
    /// pointer after `webkit_web_view_new` runs through the platform path.
    /// Returns NULL if no view has been created since the last take.
    pub fn buffr_display_take_last_view() -> *mut WPEView;

    // ── BuffrDisplayWayland (#152) ────────────────────────────────────────

    /// Constructor for `BuffrDisplayWayland`.
    ///
    /// Takes borrowed Wayland object pointers from the host winit window and
    /// creates an EGL platform display from the host `wl_display`.
    /// Returns NULL when `eglInitialize` fails (caller falls back to OSR).
    pub fn buffr_display_wayland_new(
        wl_display: *mut c_void,
        wl_compositor: *mut c_void,
        wl_subcompositor: *mut c_void,
        parent_surface: *mut c_void,
        viewport_w: std::os::raw::c_int,
        viewport_h: std::os::raw::c_int,
        scale: std::os::raw::c_double,
        refresh_hz: std::os::raw::c_int,
    ) -> *mut WPEDisplay;

    // ── BuffrViewWayland (#153) ───────────────────────────────────────────

    /// Update the subsurface position and WebKit view render size.
    ///
    /// Must be called on the GLib worker thread (same thread that commits
    /// Wayland protocol messages). No-op when `view` is NULL or is not a
    /// `BuffrViewWayland` instance.
    pub fn buffr_view_wayland_set_rect(
        view: *mut WPEView,
        x: std::os::raw::c_int,
        y: std::os::raw::c_int,
        w: std::os::raw::c_int,
        h: std::os::raw::c_int,
    );

    // ── BuffrInputMethodContext (#IME) ────────────────────────────────────
    //
    // Declared here (not in the bindgen allowlist) because they live in our
    // own C code, not in the WebKit headers. The pointer type is opaque to
    // Rust — we only ever carry it as *mut c_void and cast to
    // *mut WebKitInputMethodContext for webkit_web_view_set_input_method_context.

    /// Constructor: returns a new BuffrInputMethodContext (GObject, ref=1).
    pub fn buffr_input_method_context_new() -> *mut super::ffi::WebKitInputMethodContext;

    /// Store the preedit string and byte-offset cursor position. Emits
    /// preedit-started / preedit-changed / preedit-finished as appropriate.
    pub fn buffr_input_method_context_set_preedit(
        ctx: *mut super::ffi::WebKitInputMethodContext,
        text: *const std::os::raw::c_char,
        cursor: std::os::raw::c_int,
    );

    /// Commit text to the focused editable. Emits committed + preedit-finished.
    pub fn buffr_input_method_context_commit(
        ctx: *mut super::ffi::WebKitInputMethodContext,
        text: *const std::os::raw::c_char,
    );

    /// Cancel any in-progress composition. Emits preedit-finished when active.
    pub fn buffr_input_method_context_cancel(ctx: *mut super::ffi::WebKitInputMethodContext);

    // Tiny GLib helpers we link against directly. The bindgeneration surface
    // already covers g_bytes_*, g_error_free, etc; we only redeclare
    // qdata-related helpers since bindgeneration's allowlist skipped them.
    fn g_object_set_data_full(
        object: *mut c_void,
        key: *const std::os::raw::c_char,
        data: *mut c_void,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn g_object_get_data(object: *mut c_void, key: *const std::os::raw::c_char) -> *mut c_void;
}

// ── ViewCtx: per-view Rust state attached via qdata ──────────────────────────

/// State the render callback needs: shared OsrFrame to write into and a
/// view state to wake the host UI.
pub(crate) struct ViewCtx {
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    /// Microseconds (since process start) at which we last ingested + acked
    /// a frame on this view. Read/written from the GLib worker thread; we
    /// use an atomic so future input dispatch paths can read it without a
    /// mutex.
    pub last_ingest_us: AtomicU64,
    /// Per-tab gate: when false, the most-recent inbound buffer is parked
    /// in `pending_buffer` (unacked) instead of being imported, which makes
    /// WebKit's render scheduler stop emitting new frames for this view —
    /// WPE waits for `wpe_view_buffer_rendered` before recycling pool slots.
    /// WpeRuntime flips the active tab's flag to true on `select_tab` and
    /// clears all others.
    pub is_active: Arc<AtomicBool>,
    /// Holds the last buffer WebKit emitted while this view was inactive.
    /// `buffr_rust_render_buffer` stashes the buffer here instead of acking;
    /// `ack_pending_buffer` (called from `TabEntry::show`) acks it later so
    /// WebKit can resume emission. At most one buffer is ever held — WPE's
    /// pool semantics only emit one buffer at a time and waits for ack
    /// before reusing the slot.
    pub pending_buffer: Mutex<Option<WpeBufferPtr>>,
}

/// Process-start instant — used as the epoch for `ViewCtx::last_ingest_us`.
fn process_epoch() -> Instant {
    static ONCE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *ONCE.get_or_init(Instant::now)
}

const VIEW_CTX_KEY: &[u8] = b"buffr-view-ctx\0";

/// Heap-allocate a [`ViewCtx`] and attach it to `view` via qdata. The Box
/// is freed when the view is finalised (or replaced).
pub(crate) fn attach_view_ctx(view: *mut WPEView, ctx: ViewCtx) {
    let boxed = Box::into_raw(Box::new(ctx));
    // SAFETY: view is a live WPEView; key string is static + null-terminated.
    // The destroy callback claims back ownership of the Box.
    unsafe {
        g_object_set_data_full(
            view as *mut c_void,
            VIEW_CTX_KEY.as_ptr() as *const _,
            boxed as *mut c_void,
            Some(drop_view_ctx),
        );
    }
}

unsafe extern "C" fn drop_view_ctx(ptr: *mut c_void) {
    if !ptr.is_null() {
        // SAFETY: ptr was set by Box::into_raw in attach_view_ctx.
        drop(unsafe { Box::from_raw(ptr as *mut ViewCtx) });
    }
}

/// Look up the [`ViewCtx`] attached to `view` (returns None if missing).
fn view_ctx<'a>(view: *mut WPEView) -> Option<&'a ViewCtx> {
    // SAFETY: view is a live WPEView; key string is static + null-terminated.
    let ptr = unsafe { g_object_get_data(view as *mut c_void, VIEW_CTX_KEY.as_ptr() as *const _) };
    if ptr.is_null() {
        None
    } else {
        // SAFETY: ptr is a Box<ViewCtx> we attached via attach_view_ctx; it
        // lives until drop_view_ctx fires on view finalise.
        unsafe { Some(&*(ptr as *const ViewCtx)) }
    }
}

/// Ack any buffer parked by `buffr_rust_render_buffer` while this view
/// was inactive. Called from `TabEntry::show` so WebKit can resume
/// emitting fresh frames once the tab is foregrounded.
///
/// Returns true if a buffer was actually held + acked. No-op (returns
/// false) if the view has no `ViewCtx` attached or no parked buffer.
pub(crate) fn ack_pending_buffer(view: *mut WPEView) -> bool {
    let Some(ctx) = view_ctx(view) else {
        return false;
    };
    let Ok(mut pending) = ctx.pending_buffer.lock() else {
        return false;
    };
    let Some(b) = pending.take() else {
        return false;
    };
    // SAFETY: view + buffer were valid when we parked the buffer; WPE keeps
    // the buffer alive until we ack via wpe_view_buffer_rendered.
    unsafe {
        wpe_view_buffer_rendered(view, b.0);
    }
    true
}

// ── Per-frame callback invoked by C ──────────────────────────────────────────

/// Called by `buffr_view_render_buffer_vfunc` in `wpe_subclasses.c` on every
/// rendered WebKit frame. Imports the buffer to CPU pixels, copies into the
/// shared OsrFrame, dispatches the wake callback, and tells WPE we're done.
///
/// # Safety
///
/// Called from C on the GLib worker thread. `view` must be a live WPEView
/// owned by our BuffrDisplay; `buffer` is a WPEBuffer whose lifetime is
/// governed by `wpe_view_buffer_rendered` (called below).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn buffr_rust_render_buffer(view: *mut WPEView, buffer: *mut WPEBuffer) {
    let Some(ctx) = view_ctx(view) else {
        // No ctx attached — release the buffer and bail so WebKit doesn't
        // stall waiting for our render-completion ack.
        unsafe {
            wpe_view_buffer_rendered(view, buffer);
        }
        return;
    };

    // Per-tab active gate. Inactive tabs park their newest buffer here
    // WITHOUT acking — WPE's pool semantics make WebKit wait for the ack
    // before emitting more frames, so the view goes truly idle (no CPU
    // wasted on layout / paint). On reactivation, `ack_pending_buffer`
    // releases the held buffer and WebKit resumes emission.
    //
    // If a previous unacked buffer is already parked (shouldn't happen in
    // practice — WebKit waits for ack before reissuing — but defensive),
    // ack it first to keep the pool balanced before stashing the new one.
    if !ctx.is_active.load(Ordering::Relaxed) {
        let mut pending = ctx.pending_buffer.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(prev) = pending.take() {
            unsafe {
                wpe_view_buffer_rendered(view, prev.0);
            }
        }
        *pending = Some(WpeBufferPtr(buffer));
        return;
    }

    // Throttle ingest to the view's target frame rate. WebKit will keep
    // re-rendering as fast as we ack, so without a gate the
    // queue.write_texture / staging-buffer pool builds up and wgpu hits
    // OOM after ~3 s. Acking is the only backpressure signal WPE has, so
    // we still ack every frame here — just skip the import + memcpy +
    // wake when the previous ingest was too recent.
    let now_us = process_epoch().elapsed().as_micros() as u64;
    let hz = ctx
        .view
        .frame_rate_hz
        .load(Ordering::Relaxed)
        .max(1)
        .min(240);
    let min_interval_us = 1_000_000u64 / hz as u64;
    let last_us = ctx.last_ingest_us.load(Ordering::Relaxed);
    // `last_us == 0` means we haven't ingested anything yet — let the first
    // frame through unconditionally so a static page still paints once.
    if last_us != 0 && now_us.saturating_sub(last_us) < min_interval_us {
        // SAFETY: view + buffer are valid for the rest of the vmethod call.
        unsafe {
            wpe_view_buffer_rendered(view, buffer);
        }
        return;
    }
    ctx.last_ingest_us.store(now_us, Ordering::Relaxed);

    // SAFETY: buffer is non-null per the vmethod contract.
    let (w, h) = unsafe {
        (
            wpe_buffer_get_width(buffer) as u32,
            wpe_buffer_get_height(buffer) as u32,
        )
    };

    // import_to_pixels gives us a freshly-owned GBytes of ARGB8888 data.
    // SAFETY: buffer is valid for the call duration.
    let mut error: *mut GError = std::ptr::null_mut();
    let bytes = unsafe { wpe_buffer_import_to_pixels(buffer, &mut error) };
    if !error.is_null() {
        // SAFETY: error was set by the C call; free it.
        unsafe {
            g_error_free(error);
        }
    }
    if bytes.is_null() {
        tracing::warn!("webkit: wpe_buffer_import_to_pixels returned NULL ({w}x{h})");
        // SAFETY: view + buffer are valid.
        unsafe {
            wpe_view_buffer_rendered(view, buffer);
        }
        return;
    }
    let _ = CStr::from_bytes_with_nul(b"\0"); // suppress unused CStr import warning when error message path is gone

    // Copy bytes into OsrFrame.
    let mut size: u64 = 0;
    // SAFETY: bytes is non-null; bindgen's g_bytes_get_data signature uses
    // `*mut u64`. Cast the returned gconstpointer to `*const u8`.
    let data_ptr = unsafe { g_bytes_get_data(bytes, &mut size as *mut u64) as *const u8 };
    let size_us = size as usize;
    let row_bytes = (w as usize) * 4;
    let need = row_bytes * (h as usize);
    if !data_ptr.is_null() && size_us >= need && h > 0 {
        // import_to_pixels returns ARGB8888 but each row may be padded to a
        // hardware-friendly stride (e.g. multiples of 64 / cache lines). Use
        // div_ceil so a padded buffer (size_us not evenly divisible by h)
        // doesn't truncate — truncation would cause each row copy to start
        // from the wrong offset, corrupting every row after the first.
        let src_stride = size_us.div_ceil(h as usize);
        if src_stride < row_bytes {
            tracing::warn!(
                w,
                h,
                size_us,
                src_stride,
                row_bytes,
                "webkit: import_to_pixels stride < row_bytes — skipping frame"
            );
            // SAFETY: view + buffer are valid.
            unsafe {
                wpe_view_buffer_rendered(view, buffer);
            }
            return;
        }
        let mut generation = 0u64;
        if let Ok(mut frame) = ctx.frame.lock() {
            // WebKit/WPE may deliver a buffer slightly smaller than the host
            // requested (block-aligned content area). Mirror the actual frame
            // dims into both the shared OsrFrame and the OsrViewState atomics
            // under the same lock so a concurrent main-thread reader never sees
            // the new view.width while frame.width still carries the old value.
            let view_w = ctx.view.width.load(Ordering::Relaxed);
            let view_h = ctx.view.height.load(Ordering::Relaxed);
            if view_w != w {
                ctx.view.width.store(w, Ordering::Relaxed);
            }
            if view_h != h {
                ctx.view.height.store(h, Ordering::Relaxed);
            }
            // Resize pixels whenever len doesn't match need, not just on
            // dim change. The consumer in buffr-app swaps `frame.pixels`
            // with its own `osr_scratch` (initially `Vec::new()`, len=0)
            // on every fresh-frame read. Once swap puts an empty Vec
            // into `frame.pixels`, dims still match w/h so the
            // dim-changed branch is skipped, but the copy below bails
            // on `frame.pixels.len() >= need` → generation never
            // increments → consumer's freshness gate rejects every
            // subsequent frame → viewport freezes on the first paint.
            if frame.width != w || frame.height != h || frame.pixels.len() < need {
                frame.width = w;
                frame.height = h;
                frame.pixels.resize(need, 0);
            }
            // Clear needs_fresh on every successful ingest, not only on
            // dim change. WpeRuntime::resize sets needs_fresh=true and
            // pre-sets frame.width/height to the new dims; the next WPE
            // frame arrives already matching those dims so the
            // dim-changed branch above is skipped. Without clearing here,
            // needs_fresh stays true forever and is_osr_frame_fresh
            // rejects every frame — UI freezes on the last accepted one.
            frame.needs_fresh = false;
            if frame.pixels.len() >= need {
                let dst = frame.pixels.as_mut_ptr();
                for row in 0..(h as usize) {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data_ptr.add(row * src_stride),
                            dst.add(row * row_bytes),
                            row_bytes,
                        );
                    }
                }
                frame.generation = frame.generation.wrapping_add(1);
                generation = frame.generation;
            }
        }
        tracing::debug!(
            w,
            h,
            src_stride,
            row_bytes,
            generation,
            "webkit: frame ingested"
        );
        if let Some(wake) = ctx.view.wake.get() {
            wake();
        }
    } else {
        tracing::warn!(
            w,
            h,
            size_us,
            need,
            "webkit: import_to_pixels returned undersized buffer"
        );
    }

    // Intentionally NOT calling g_bytes_unref here. WPE 2.52's
    // import_to_pixels appears to return a GBytes whose ref count is owned
    // by the WPEBuffer's internal cache (the buffer pool cycles a fixed
    // set of WPEBuffer objects, so the same GBytes is returned on each
    // delivery). Unreffing here drops the cache's only ref to zero after
    // ~28 frames, freeing memory still referenced by the next import,
    // and the worker SEGVs inside g_main_context_query_unlocked when GLib
    // touches the freed source state.
    let _ = bytes;

    // Acknowledge the frame so WebKit schedules the next one.
    // SAFETY: view + buffer are valid for the rest of the vmethod call.
    unsafe {
        wpe_view_buffer_rendered(view, buffer);
    }
}

// ── High-level constructor ───────────────────────────────────────────────────

/// Owned wrapper around a BuffrDisplay GObject — drops the reference on
/// drop. Marked `Send` because GObject reference counting is thread-safe;
/// actual GObject method calls still need to happen on the worker thread.
pub(crate) struct BuffrDisplayHandle {
    pub raw: *mut WPEDisplay,
}

unsafe impl Send for BuffrDisplayHandle {}

impl BuffrDisplayHandle {
    pub(crate) fn new(
        egl_display: *mut c_void,
        width: u32,
        height: u32,
        scale: f64,
        refresh_hz: u32,
    ) -> Option<Self> {
        // SAFETY: buffr_display_new is the C-side constructor; egl_display
        // is opaque, may be NULL during early bring-up.
        let raw = unsafe {
            buffr_display_new(
                egl_display,
                width as i32,
                height as i32,
                scale,
                refresh_hz as i32,
            )
        };
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }
}

impl Drop for BuffrDisplayHandle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: raw is a live GObject from g_object_new.
            unsafe {
                g_object_unref(self.raw as *mut c_void);
            }
        }
    }
}

// Re-export the GLib unref via the bindings allowlist.
unsafe extern "C" {
    pub(crate) fn g_object_unref(object: *mut c_void);
}

// ── BuffrDisplayWaylandHandle ─────────────────────────────────────────────────

/// Owned wrapper around a `BuffrDisplayWayland` GObject (the custom WPEDisplay
/// subclass that reuses the host Wayland connection). Drops the GObject ref on
/// drop. Marked `Send` — GObject reference counting is thread-safe; actual
/// vmethod calls still happen on the GLib worker thread.
pub(crate) struct BuffrDisplayWaylandHandle {
    pub raw: *mut WPEDisplay,
}

unsafe impl Send for BuffrDisplayWaylandHandle {}

impl Drop for BuffrDisplayWaylandHandle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: raw is a live GObject from g_object_new.
            unsafe {
                g_object_unref(self.raw as *mut c_void);
            }
        }
    }
}

// ── WpeDisplayKind ────────────────────────────────────────────────────────────

/// Which WPEDisplay variant the runtime is using.
///
/// - `Osr(BuffrDisplayHandle)` — our custom BuffrDisplay subclass that feeds the
///   OSR pixel-copy path (`buffr_rust_render_buffer`). Used in headless / CI, or
///   when `prefer_native` is false.
/// - `Wayland(*mut WPEDisplay)` — stock `WPEDisplayWayland` connected to the
///   host compositor. Available as env-gated fallback via
///   `BUFFR_WEBKIT_STOCK_WAYLAND=1`. Used when `prefer_native=true` but Wayland
///   handles are not yet available.
/// - `BuffrWayland(BuffrDisplayWaylandHandle)` — our custom WPEDisplay subclass
///   that reuses the host's `wl_display`. Preferred Phase 3 path (#152).
///   Used when `prefer_native=true` and all four Wayland handle pointers are
///   non-null.
pub(crate) enum WpeDisplayKind {
    Osr(BuffrDisplayHandle),
    /// Raw `*mut WPEDisplay` (actually `*mut WPEDisplayWayland`, upcast).
    /// Owned: we hold one GObject ref and release it on drop.
    /// Kept as env-gated fallback (`BUFFR_WEBKIT_STOCK_WAYLAND=1`).
    Wayland(*mut WPEDisplay),
    /// Custom BuffrDisplayWayland subclass — preferred Phase 3 path (#152).
    BuffrWayland(BuffrDisplayWaylandHandle),
}

unsafe impl Send for WpeDisplayKind {}

impl WpeDisplayKind {
    /// Return the raw `*mut WPEDisplay` regardless of variant.
    ///
    /// Used by `TabEntry::new` to pass the display as the `display`
    /// construct-property on `WebKitWebView`.
    pub(crate) fn raw_display(&self) -> *mut WPEDisplay {
        match self {
            WpeDisplayKind::Osr(h) => h.raw,
            WpeDisplayKind::Wayland(p) => *p,
            WpeDisplayKind::BuffrWayland(h) => h.raw,
        }
    }

    /// Returns `true` when this is a native Wayland path (stock or custom).
    pub(crate) fn is_native(&self) -> bool {
        matches!(
            self,
            WpeDisplayKind::Wayland(_) | WpeDisplayKind::BuffrWayland(_)
        )
    }
}

impl Drop for WpeDisplayKind {
    fn drop(&mut self) {
        if let WpeDisplayKind::Wayland(p) = self {
            if !p.is_null() {
                // SAFETY: *p is a live WPEDisplayWayland GObject we own a ref on.
                unsafe { g_object_unref(*p as *mut c_void) };
            }
        }
        // Osr variant: BuffrDisplayHandle's own Drop handles the unref.
        // BuffrWayland variant: BuffrDisplayWaylandHandle's Drop handles it.
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Verify that `div_ceil` correctly computes the stride for a padded
    /// buffer, addressing fix #7 (src_stride integer division truncation).
    ///
    /// A padded buffer whose total size is not evenly divisible by `h` must
    /// use `div_ceil` so each row copy starts at the right offset. Truncating
    /// division produces a stride that is too small, causing every row after
    /// the first to be read from 1–(stride_pad−1) bytes too early.
    #[test]
    fn stride_div_ceil_does_not_truncate() {
        // h=2, w=3 → row_bytes=12. Total size 25 (padded, not divisible by 2).
        let h: usize = 2;
        let row_bytes: usize = 3 * 4; // 12
        let size_us: usize = 25;

        let trunc_stride = size_us / h; // 12
        let ceil_stride = size_us.div_ceil(h); // 13

        assert!(
            ceil_stride >= trunc_stride,
            "div_ceil must not be smaller than truncating division"
        );
        assert!(
            ceil_stride >= row_bytes,
            "stride must be at least row_bytes wide"
        );

        // The critical broken case: size=23, h=2 → trunc gives 11 < row_bytes=12.
        // The new guard (warn + bail) catches this. div_ceil gives 12 == row_bytes.
        let bad_size: usize = 23;
        let bad_trunc = bad_size / h; // 11 — under row_bytes, would corrupt memory
        let bad_ceil = bad_size.div_ceil(h); // 12 — minimal valid stride
        assert!(
            bad_trunc < row_bytes,
            "trunc stride under row_bytes is the bug this fix targets"
        );
        assert_eq!(
            bad_ceil, row_bytes,
            "div_ceil recovers the minimum valid stride"
        );
    }

    #[test]
    fn stride_ceil_equals_trunc_when_evenly_divisible() {
        // When size_us is exactly divisible by h, div_ceil == truncating.
        let h: usize = 4;
        let size_us: usize = 64;
        assert_eq!(size_us / h, size_us.div_ceil(h));
    }
}
