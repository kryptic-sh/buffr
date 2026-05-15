//! OSR (off-screen rendering) pipeline for the WebKitGTK backend.
//!
//! # Phase B strategy
//!
//! WebKitGTK exposes `WebView::snapshot()` which returns a `gdk::Texture` via
//! an async callback. The callback fires on the GTK main thread.
//!
//! ## `gdk_texture_download` pixel format
//!
//! `gdk::TextureExtManual::download()` writes Cairo ARGB32 premultiplied
//! format. On little-endian systems (x86-64, aarch64-LE) the byte order is:
//!   byte[0] = B, byte[1] = G, byte[2] = R, byte[3] = A
//! This is exactly BGRA, which is what `SharedOsrFrame` expects.
//!
//! # Snapshot loop
//!
//! - 4 fps steady tick via `glib::timeout_add_local` (wired in worker.rs).
//! - After every navigation/signal event (triggered by signal handlers).
//! - On `osr_resize` (triggered by the resize command handler).
//!
//! # TODO markers
//!
//! - TODO(snapshot): Phase C should call `WebView::snapshot()` and write real
//!   pixels. The Phase B blank-frame path is sufficient for build verification.

use std::sync::atomic::Ordering;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

// ── Blank-frame paint ─────────────────────────────────────────────────────────

/// Write a solid white BGRA frame into `frame` at the current view dimensions.
///
/// Used as the Phase-B OSR placeholder. Phase C will replace this with a call
/// to `WebView::snapshot()` + `gdk::TextureExtManual::download()`.
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

// ── Real snapshot (Phase C placeholder) ──────────────────────────────────────
//
// Phase C will wire WebView::snapshot() here. The pipeline will be:
//
//   1. Call `web_view.snapshot(SnapshotRegion::Visible, SnapshotOptions::empty(),
//         None::<&gio::Cancellable>, move |result| { ... })`
//   2. In the callback:
//      a. `let texture: gdk::Texture = result.expect("snapshot failed");`
//      b. `let w = texture.width() as u32;`
//      c. `let h = texture.height() as u32;`
//      d. `let stride = w as usize * 4;`
//      e. `let mut pixels = vec![0u8; stride * h as usize];`
//      f. `gdk::TextureExtManual::download(&texture, &mut pixels, stride);`
//         → pixels are now BGRA (Cairo ARGB32 on little-endian LE = BGRA).
//      g. Write pixels into SharedOsrFrame.
//
// TODO(snapshot): implement Phase C snapshot pipeline.
