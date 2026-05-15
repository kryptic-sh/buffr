//! CPU rasterisation pipeline for buffr-blitz.
//!
//! Paints a laid-out [`blitz_html::HtmlDocument`] into a [`SharedOsrFrame`]
//! using the pure-CPU Vello renderer — no wgpu required.
//!
//! # Pixel pipeline
//!
//! 1. [`anyrender::ImageRenderer::render_to_vec`] calls the draw closure with
//!    a [`anyrender_vello_cpu::VelloCpuScenePainter`].
//! 2. Inside the closure we call [`blitz_paint::paint_scene`] which pushes
//!    draw commands for the entire DOM into the painter.
//! 3. `render_to_vec` flushes and rasterises, writing **premultiplied RGBA8**
//!    pixels into the provided `Vec<u8>`.  The layout is R, G, B, A per pixel.
//! 4. We swap R↔B on every pixel to produce BGRA8, which is what
//!    [`SharedOsrFrame`] expects (identical to the CEF on-paint contract).
//!    Alpha is kept premultiplied — the OSR consumer expects that.

use anyrender::ImageRenderer as _;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_html::HtmlDocument;

use buffr_engine::SharedOsrFrame;

/// Render `doc` into `frame` at the given pixel dimensions.
///
/// Calls `paint_scene` on the fully-resolved [`HtmlDocument`] and writes the
/// resulting BGRA pixels into `frame`.  On any error, falls back to a solid
/// white fill and logs at `warn`.
pub(crate) fn render_doc_into_frame(
    doc: &HtmlDocument,
    frame: &SharedOsrFrame,
    width: u32,
    height: u32,
    scale: f64,
) {
    // Zero dimensions — nothing to render.
    if width == 0 || height == 0 {
        return;
    }

    // Rasterise into a temporary RGBA buffer.
    let mut renderer = VelloCpuImageRenderer::new(width, height);

    let mut rgba_buf: Vec<u8> = Vec::new();
    renderer.render_to_vec(
        |painter| {
            // `HtmlDocument` derefs to `&BaseDocument`, which is what
            // `paint_scene` requires.  scale=1.0 matches the viewport set in
            // `BlitzTab::resize`.  x_offset/y_offset are zero (no scroll).
            blitz_paint::paint_scene(painter, doc, scale, width, height, 0, 0);
        },
        &mut rgba_buf,
    );

    let expected_len = (width as usize) * (height as usize) * 4;
    if rgba_buf.len() != expected_len {
        tracing::warn!(
            "blitz render: unexpected buffer length {} (expected {}); falling back to white fill",
            rgba_buf.len(),
            expected_len
        );
        write_white_fill(frame, width, height);
        return;
    }

    // Swap R↔B: premultiplied RGBA8 → premultiplied BGRA8.
    // Chunk layout: [R, G, B, A] → [B, G, R, A]
    for chunk in rgba_buf.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    match frame.lock() {
        Ok(mut guard) => {
            guard.width = width;
            guard.height = height;
            guard.pixels = rgba_buf;
            guard.generation = guard.generation.wrapping_add(1);
            guard.needs_fresh = false;
            tracing::debug!(
                "blitz render: wrote BGRA frame {width}×{height} gen={}",
                guard.generation
            );
        }
        Err(e) => {
            tracing::warn!("blitz render: frame lock poisoned: {e}");
        }
    }
}

/// Write a solid white BGRA fill into `frame` (fallback path).
pub(crate) fn write_white_fill(frame: &SharedOsrFrame, width: u32, height: u32) {
    if let Ok(mut guard) = frame.lock() {
        let len = (width as usize) * (height as usize) * 4;
        if guard.width != width || guard.height != height || guard.pixels.len() != len {
            guard.width = width;
            guard.height = height;
            guard.pixels = vec![0xffu8; len];
        } else {
            guard.pixels.fill(0xff);
        }
        guard.generation = guard.generation.wrapping_add(1);
        guard.needs_fresh = false;
        tracing::debug!("blitz render: white fill {width}×{height}");
    }
}
