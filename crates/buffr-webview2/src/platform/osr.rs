//! OSR (off-screen rendering) pipeline for the WebView2 backend.
//!
//! # Phase B strategy — `ICoreWebView2CompositionController3` screenshot capture
//!
//! WebView2 offers two OSR approaches:
//!
//! 1. **D3D11 staging texture readback** — attaches a composition visual to a
//!    `ICompositionGraphicsDevice` swap surface, then copies the back-buffer
//!    through a staging texture. Works everywhere but is ~50 LOC of D3D11
//!    boilerplate per frame and requires a full DXGISwapChain per tab.
//!
//! 2. **`ICoreWebView2CompositionController3::CapturePreview` /
//!    `ICoreWebView2::CapturePreview`** — single COM call that requests
//!    BGRA/PNG/JPEG bytes asynchronously. Available since WebView2 SDK 1.0.1083
//!    (runtime 94+). Returns BGRA if you ask for the `BGRA` format (SDK ≥ 1.0.1829).
//!    Simpler, lower coupling, no D3D11 setup.
//!
//! Phase B uses **approach 2** (`CapturePreview` via `ICoreWebView2_6`).
//! The async callback is bridged to the `SharedOsrFrame` via the STA message
//! pump. Phase C should switch to approach 1 for lower latency.
//!
//! # Blank-frame placeholder
//!
//! Until a tab is loaded `paint_blank` writes a solid-white BGRA frame so
//! the compositor always has something to display.
//!
//! # TODO markers
//!
//! - TODO(osr-capture): replace `paint_blank` with `ICoreWebView2_6::CapturePreview`
//!   call on the STA thread after a tab is ready. Wire up to the 250 ms tick
//!   in worker.rs.
//! - TODO(osr-d3d): Phase C: swap to D3D11 staging readback for sub-16 ms
//!   frame latency at 60 fps.

use std::sync::atomic::Ordering;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

// ── Blank-frame paint ─────────────────────────────────────────────────────────

/// Write a solid-white BGRA frame into `frame` at the current view dimensions.
///
/// Used as the Phase-B OSR placeholder until `CapturePreview` wiring lands.
/// BGRA solid white = `[0xff, 0xff, 0xff, 0xff]` repeated.
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
    tracing::debug!("webview2 osr: painted blank frame {w}×{h}");
}

// ── Real capture (Phase C placeholder) ───────────────────────────────────────
//
// Phase C will call ICoreWebView2_6::CapturePreview here. The pipeline:
//
//  1. Cast `ICoreWebView2` to `ICoreWebView2_6`.
//  2. Create an `IStream` (SHCreateMemStream or CreateStreamOnHGlobal).
//  3. Call `CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
//         stream_ptr, completion_handler)`.
//  4. In the completion handler (runs on STA thread):
//     a. Seek stream to 0.
//     b. Read all bytes into a Vec<u8>.
//     c. Decode PNG → raw BGRA via the `image` crate (or `zune-png`).
//     d. Write pixels into SharedOsrFrame.
//
// For BGRA (not PNG) use ICoreWebView2CompositionController3::CapturePreview
// with format = COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG and decode, or
// wait for SDK ≥ 1.0.1829 which exposes a raw BGRA stream API.
//
// TODO(osr-capture): implement Phase C capture pipeline.
