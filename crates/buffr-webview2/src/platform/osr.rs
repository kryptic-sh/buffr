//! OSR (off-screen rendering) pipeline for the WebView2 backend.
//!
//! # Phase B strategy — `ICoreWebView2::CapturePreview`
//!
//! WebView2 offers two OSR approaches:
//!
//! 1. **D3D11 staging texture readback** — attaches a composition visual to a
//!    `ICompositionGraphicsDevice` swap surface, then copies the back-buffer
//!    through a staging texture. Works everywhere but is ~50 LOC of D3D11
//!    boilerplate per frame and requires a full DXGISwapChain per tab.
//!
//! 2. **`ICoreWebView2::CapturePreview`** — single COM call that requests PNG
//!    bytes asynchronously into a caller-supplied `IStream`. Available since
//!    WebView2 SDK 1.0.1083 (runtime 94+). Simpler, lower coupling, no D3D11
//!    setup.
//!
//! Phase B uses **approach 2** (`CapturePreview` via `ICoreWebView2`).
//! The async callback fires on the STA thread and writes into `SharedOsrFrame`.
//! Phase C should switch to approach 1 for sub-16 ms frame latency.
//!
//! # IStream strategy
//!
//! `CreateStreamOnHGlobal(None, true)` allocates a growable HGLOBAL-backed
//! memory stream. `fDeleteOnRelease = true` means the HGLOBAL is freed when
//! the `IStream` COM ref drops to zero — no manual `GlobalFree` needed.
//! After the completion callback fires we `Seek(0, STREAM_SEEK_SET)` then
//! `Read` in 64 KiB chunks until EOF.
//!
//! # Pixel pipeline
//!
//! `CapturePreview(PNG, ...)` → `Vec<u8>` (PNG bytes) →
//! `image::load_from_memory` → `into_rgba8()` → swap R↔B per pixel → BGRA →
//! `SharedOsrFrame`.
//!
//! # In-flight flag
//!
//! A `bool` flag (passed as `Arc<Mutex<bool>>` wrapping via `capture_in_flight`
//! on `StaRuntime`) prevents overlapping captures. If a capture is already
//! pending the new trigger is silently dropped.
//!
//! # Trigger points (wired in worker.rs)
//!
//! - 250 ms timer tick on the STA thread (every 25 × 10 ms CMD_TIMER iterations).
//! - After `NavigationCompleted` event.
//! - After `Command::Resize`.
//! - After `Command::ForcePaint`.
//! - After `Command::SendInput` — TODO(#106-followup): not yet actionable.
//!
//! # Blank-frame placeholder
//!
//! Until a tab is loaded `paint_blank` writes a solid-white BGRA frame so the
//! compositor always has something to display.
//!
//! # Phase C TODO
//!
//! - TODO(osr-d3d): Phase C: swap to D3D11 staging readback for sub-16 ms
//!   frame latency at 60 fps.

use std::sync::atomic::Ordering;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

// ── Blank-frame paint ─────────────────────────────────────────────────────────

/// Write a solid-white BGRA frame into `frame` at the current view dimensions.
///
/// Used as the Phase-B OSR placeholder before the first real CapturePreview
/// result arrives, and as the fallback on capture failure.
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

// ── CapturePreview OSR pipeline (Windows only) ────────────────────────────────

/// Request a `CapturePreview` screenshot from `webview` and write the decoded
/// BGRA pixels into `frame` when the async completion handler fires on the STA
/// thread.
///
/// `capture_in_flight` is a `bool` stored behind an `Arc<std::sync::Mutex<bool>>`
/// that the worker passes in. If already `true` a capture is pending and this
/// call is a no-op.
///
/// # Pixel pipeline
///
/// `CapturePreview(PNG)` → `IStream` → `Vec<u8>` (PNG bytes) →
/// `image::load_from_memory` → `into_rgba8()` → swap R↔B per pixel → BGRA →
/// written into `SharedOsrFrame`.
///
/// # Safety
///
/// All COM calls in this function are in `unsafe` blocks. The invariants are
/// documented inline.
#[cfg(target_os = "windows")]
pub(crate) fn request_capture(
    webview: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    capture_in_flight: std::sync::Arc<std::sync::Mutex<bool>>,
) {
    use webview2_com::{
        CapturePreviewCompletedHandler,
        Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
    };
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};

    // ── In-flight guard ───────────────────────────────────────────────────────
    {
        let mut flag = capture_in_flight.lock().unwrap();
        if *flag {
            tracing::debug!("webview2 osr: capture already in-flight — dropping tick");
            return;
        }
        *flag = true;
    }

    // ── Allocate IStream backed by HGLOBAL ────────────────────────────────────
    //
    // SAFETY: CreateStreamOnHGlobal is an OLE32 API. Passing a null (zeroed)
    // HGLOBAL lets OLE allocate the backing HGLOBAL internally.
    // `fDeleteOnRelease = true` ties the HGLOBAL lifetime to the IStream COM
    // ref — no manual GlobalFree needed.
    use windows::Win32::Foundation::HGLOBAL;
    let stream: IStream = match unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) } {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("webview2 osr: CreateStreamOnHGlobal failed: {e}");
            *capture_in_flight.lock().unwrap() = false;
            return;
        }
    };

    let in_flight_cb = std::sync::Arc::clone(&capture_in_flight);
    // Clone the IStream COM interface pointer so the completion handler closure
    // owns a ref to the same underlying stream that CapturePreview writes into.
    // Both `stream` and `stream_cb` point to the same COM object.
    let stream_cb = stream.clone();

    // ── Fire CapturePreview ───────────────────────────────────────────────────
    //
    // SAFETY: CapturePreview is an STA COM method on a valid ICoreWebView2.
    // `stream` is a valid IStream created above on this thread; `stream_cb` is
    // a clone of the same COM interface pointer moved into the handler closure.
    // The handler closure is invoked by WebView2 on the STA thread.
    let result = unsafe {
        webview.CapturePreview(
            COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
            &stream,
            &CapturePreviewCompletedHandler::create(Box::new(move |hr| {
                // Always clear the in-flight flag, even on error.
                *in_flight_cb.lock().unwrap() = false;

                if let Err(e) = hr {
                    tracing::warn!(
                        "webview2 osr: CapturePreview failed: HRESULT {:#010x}",
                        e.code().0 as u32
                    );
                    return Ok(());
                }

                // ── Read PNG bytes from IStream ───────────────────────────────
                //
                // SAFETY: Seek + Read are STA COM methods on `stream_cb`.
                // The IStream was created on this STA thread and is only
                // accessed here inside the STA completion callback.
                // Seek(0, STREAM_SEEK_SET) rewinds to the start of the buffer.
                if let Err(e) = stream_cb.Seek(0, STREAM_SEEK_SET, None) {
                    tracing::warn!("webview2 osr: IStream::Seek failed: {e}");
                    return Ok(());
                }

                let mut png_bytes: Vec<u8> = Vec::new();
                let mut buf = [0u8; 65536];
                loop {
                    let mut bytes_read: u32 = 0;
                    // IStream::Read writes into `buf` and sets `bytes_read`.
                    // Calling Read until bytes_read == 0 is the EOF pattern.
                    let read_result = stream_cb.Read(
                        buf.as_mut_ptr().cast(),
                        buf.len() as u32,
                        Some(&mut bytes_read),
                    );
                    if bytes_read == 0 || read_result.is_err() {
                        break;
                    }
                    png_bytes.extend_from_slice(&buf[..bytes_read as usize]);
                }

                if png_bytes.is_empty() {
                    tracing::warn!("webview2 osr: CapturePreview returned empty stream");
                    return Ok(());
                }

                // ── Decode PNG → RGBA → BGRA ──────────────────────────────────
                let img = match image::load_from_memory(&png_bytes) {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!("webview2 osr: PNG decode failed: {e}");
                        return Ok(());
                    }
                };
                let rgba = img.into_rgba8();
                let img_w = rgba.width();
                let img_h = rgba.height();

                if img_w == 0 || img_h == 0 {
                    tracing::warn!("webview2 osr: decoded image is zero-size ({img_w}×{img_h})");
                    return Ok(());
                }

                // Swap R↔B to convert RGBA → BGRA in place.
                let mut pixels: Vec<u8> = rgba.into_raw();
                for chunk in pixels.chunks_exact_mut(4) {
                    chunk.swap(0, 2); // R↔B; G and A stay in place
                }

                // ── Write into SharedOsrFrame ─────────────────────────────────
                if let Ok(mut guard) = frame.lock() {
                    guard.width = img_w;
                    guard.height = img_h;
                    guard.pixels = pixels;
                    guard.generation = guard.generation.wrapping_add(1);
                    guard.needs_fresh = false;
                }

                // Update view dimensions to match actual captured size.
                view.width.store(img_w, Ordering::Relaxed);
                view.height.store(img_h, Ordering::Relaxed);

                // Wake the consumer (e.g. compositor thread).
                if let Some(wake) = view.wake.get() {
                    wake();
                }

                tracing::debug!("webview2 osr: CapturePreview written {img_w}×{img_h}");
                Ok(())
            })),
        )
    };

    if let Err(e) = result {
        tracing::warn!(
            "webview2 osr: CapturePreview dispatch failed: HRESULT {:#010x}",
            e.code().0 as u32
        );
        *capture_in_flight.lock().unwrap() = false;
    }
}
