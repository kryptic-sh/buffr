//! OSR (off-screen rendering) pipeline for the WebKitGTK backend.
//!
//! # Phase C strategy
//!
//! WebKitGTK exposes `WebView::snapshot()` which delivers a `gdk::Texture` via
//! an async callback. The callback fires on the GTK main thread.
//!
//! ## `gdk_texture_download` pixel format
//!
//! `gdk::TextureExtManual::download()` writes Cairo ARGB32 premultiplied
//! format. On little-endian systems (x86-64, aarch64-LE) the byte order is:
//!   byte[0] = B, byte[1] = G, byte[2] = R, byte[3] = A
//! This is exactly BGRA, which is what `SharedOsrFrame` expects.
//! **No channel swap needed.**
//!
//! ## Stride handling
//!
//! `gdk_texture_download` writes `stride` bytes per row. The stride MUST be at
//! least `width * 4` and MAY be padded for SIMD alignment. We always allocate
//! exactly `stride * height` bytes and copy row by row into the frame pixel
//! buffer (which uses `width * 4` bytes per row, no padding).
//!
//! # Snapshot triggers
//!
//! - 4 fps steady tick via `glib::timeout_add_local` (250 ms) — wired in
//!   worker.rs. If a snapshot is already in-flight the tick is dropped.
//! - After every `LoadEvent::Finished` signal — wired via
//!   `connect_load_changed` in runtime.rs.
//! - On `Command::Resize` / `Command::ForcePaint` — both call `request_snapshot`.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use webkit6::gdk::prelude::{TextureExt, TextureExtManual};
use webkit6::prelude::WebViewExt;
use webkit6::{SnapshotOptions, WebView};

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

// ── Blank-frame paint (fallback / initial fill) ───────────────────────────────

/// Write a solid white BGRA frame into `frame` at the current view dimensions.
///
/// Used as the initial placeholder before the first real snapshot arrives.
pub(crate) fn paint_blank(frame: &SharedOsrFrame, view: &SharedOsrViewState) {
    let w = view.width.load(Ordering::Relaxed);
    let h = view.height.load(Ordering::Relaxed);
    if let Ok(mut guard) = frame.lock() {
        let len = (w as usize) * (h as usize) * 4;
        if guard.width != w || guard.height != h || guard.pixels.len() != len {
            guard.width = w;
            guard.height = h;
            guard.pixels = vec![0xff_u8; len]; // solid white BGRA
        } else {
            guard.pixels.fill(0xff);
        }
        guard.generation = guard.generation.wrapping_add(1);
        guard.needs_fresh = false;
    }
    if let Some(wake) = view.wake.get() {
        wake();
    }
    tracing::debug!("webkitgtk osr: painted blank frame {w}×{h}");
}

// ── Real snapshot pipeline ────────────────────────────────────────────────────

/// Request a WebKitGTK snapshot from `web_view` and write the result into
/// `frame` when the callback fires on the GTK main thread.
///
/// `in_flight` is a shared flag. If it is already `true` a snapshot is
/// pending — the new request is dropped to avoid stacking up callbacks.
/// The flag is cleared inside the callback regardless of success/failure.
///
/// # Pixel format
///
/// `gdk_texture_download` delivers Cairo ARGB32. On LE hosts this is BGRA
/// byte order — exactly what `SharedOsrFrame` stores. No channel swap.
///
/// # Stride
///
/// We pass `stride = width * 4` to `download()`. The GDK contract guarantees
/// the call is valid as long as `stride >= width * 4` — which holds exactly.
/// The destination frame buffer uses `width * 4` bytes per row (no padding),
/// so the same stride value serves both the download buffer and the frame.
pub(crate) fn request_snapshot(
    web_view: &WebView,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    in_flight: Rc<Cell<bool>>,
) {
    // Drop if a snapshot is already queued.
    if in_flight.get() {
        tracing::debug!("webkitgtk osr: snapshot already in-flight — dropping tick");
        return;
    }
    in_flight.set(true);

    let opts = SnapshotOptions::NONE;

    web_view.snapshot(
        webkit6::SnapshotRegion::Visible,
        opts,
        None::<&webkit6::gio::Cancellable>,
        move |result| {
            // Always clear the flag, even on error.
            in_flight.set(false);

            let texture = match result {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("webkitgtk osr: snapshot failed: {e}");
                    return;
                }
            };

            let tex_w = texture.width();
            let tex_h = texture.height();
            if tex_w <= 0 || tex_h <= 0 {
                tracing::warn!(
                    "webkitgtk osr: snapshot returned zero-size texture ({tex_w}×{tex_h})"
                );
                return;
            }
            let w = tex_w as u32;
            let h = tex_h as u32;

            // stride = width * 4 (no extra padding required by our buffer).
            // GDK guarantees this is valid as long as stride >= width * 4.
            let stride = (w as usize) * 4;
            let mut pixels = vec![0u8; stride * (h as usize)];

            // Download BGRA pixels into buffer.
            texture.download(&mut pixels, stride);

            // Update the shared OSR frame.
            if let Ok(mut guard) = frame.lock() {
                guard.width = w;
                guard.height = h;
                guard.pixels = pixels;
                guard.generation = guard.generation.wrapping_add(1);
                guard.needs_fresh = false;
            }

            // Update view dimensions to match actual texture size.
            view.width.store(w, Ordering::Relaxed);
            view.height.store(h, Ordering::Relaxed);

            // Wake the consumer (e.g. compositor thread).
            if let Some(wake) = view.wake.get() {
                wake();
            }

            tracing::debug!("webkitgtk osr: snapshot written {w}×{h}");
        },
    );
}
