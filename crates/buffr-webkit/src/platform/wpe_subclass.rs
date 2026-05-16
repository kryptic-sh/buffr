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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

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

    /// GType of `BuffrView` (the WPEView subclass our display creates).
    pub fn buffr_display_get_view_type() -> GType;

    /// GType of `BuffrToplevel`.
    pub fn buffr_display_get_toplevel_type() -> GType;

    /// Atomic getter for the most recently created `WPEView` (returned
    /// from our `create_view` vmethod). Used by Rust to grab a borrowed
    /// pointer after `webkit_web_view_new` runs through the platform path.
    /// Returns NULL if no view has been created since the last take.
    pub fn buffr_display_take_last_view() -> *mut WPEView;

    // Tiny GLib helpers we link against directly. The bindgeneration surface
    // already covers g_bytes_*, g_error_free, etc; we only redeclare
    // qdata-related helpers since bindgeneration's allowlist skipped them.
    fn g_object_set_data_full(
        object: *mut c_void,
        key: *const std::os::raw::c_char,
        data: *mut c_void,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn g_object_get_data(object: *mut c_void, key: *const std::os::raw::c_char)
        -> *mut c_void;
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
    /// Per-tab gate: when false, this view's paints are dropped (ack only)
    /// so an inactive tab can't overwrite the shared OsrFrame with its own
    /// pixels. WpeRuntime flips the active tab's flag to true on
    /// select_tab and clears all others.
    pub is_active: Arc<AtomicBool>,
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

    // Per-tab active gate. Inactive tabs still produce paints (WebKit
    // doesn't stop rendering when a view goes off-screen unless we call
    // was_hidden); ack the buffer so WPE doesn't stall, but don't write
    // pixels into the shared OsrFrame — otherwise a background tab's
    // content would clobber the active tab's display.
    if !ctx.is_active.load(Ordering::Relaxed) {
        unsafe {
            wpe_view_buffer_rendered(view, buffer);
        }
        return;
    }

    // Throttle ingest to the view's target frame rate. WebKit will keep
    // re-rendering as fast as we ack, so without a gate the
    // queue.write_texture / staging-buffer pool builds up and wgpu hits
    // OOM after ~3 s. Acking is the only backpressure signal WPE has, so
    // we still ack every frame here — just skip the import + memcpy +
    // wake when the previous ingest was too recent.
    let now_us = process_epoch().elapsed().as_micros() as u64;
    let hz = ctx.view.frame_rate_hz.load(Ordering::Relaxed).max(1).min(240);
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
        // the buffer's total size / height to recover the stride and copy
        // each row tight into OsrFrame.
        let src_stride = size_us / (h as usize);
        let mut generation = 0u64;
        // WebKit/WPE may deliver a buffer slightly smaller than the host
        // requested (block-aligned content area). Mirror the actual frame
        // dims into the shared OsrViewState so buffr-app's
        // is_osr_frame_fresh gate accepts the frame.
        let view_w = ctx.view.width.load(Ordering::Relaxed);
        let view_h = ctx.view.height.load(Ordering::Relaxed);
        if view_w != w {
            ctx.view.width.store(w, Ordering::Relaxed);
        }
        if view_h != h {
            ctx.view.height.store(h, Ordering::Relaxed);
        }
        if let Ok(mut frame) = ctx.frame.lock() {
            if frame.width != w || frame.height != h {
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
        tracing::debug!(w, h, src_stride, row_bytes, generation, "webkit: frame ingested");
        if let Some(wake) = ctx.view.wake.get() {
            wake();
        }
    } else {
        tracing::warn!(w, h, size_us, need, "webkit: import_to_pixels returned undersized buffer");
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
        if raw.is_null() { None } else { Some(Self { raw }) }
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
    fn g_object_unref(object: *mut c_void);
}

// ── No-op constructors that publish the symbols Rust uses ────────────────────

/// Force-link the C bridge functions so the linker keeps them in even if
/// Rust never calls them in this crate (e.g. when only the render callback
/// is exercised). Called once from worker init.
pub(crate) fn force_link_bridge() {
    // SAFETY: just probes the getters; we discard the return.
    unsafe {
        let _ = buffr_display_get_view_type();
        let _ = buffr_display_get_toplevel_type();
    }
}

/// Convenience: build a `ViewCtx` from the shared frame + view state.
#[allow(dead_code)]
pub(crate) fn make_view_ctx(frame: SharedOsrFrame, view: SharedOsrViewState) -> ViewCtx {
    ViewCtx {
        frame,
        view,
        last_ingest_us: AtomicU64::new(0),
        is_active: Arc::new(AtomicBool::new(true)),
    }
}

/// Used by the test compilation path to verify Arc construction; not part
/// of the public surface yet.
#[allow(dead_code)]
fn _phantom_arc_use<T>(_: Arc<T>) {}
