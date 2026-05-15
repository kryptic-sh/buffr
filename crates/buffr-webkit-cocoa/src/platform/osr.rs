//! OSR (off-screen rendering) pipeline for the WebKit Cocoa backend.
//!
//! # Strategy
//!
//! WKWebView does not expose a public off-screen pixel readback path on macOS.
//! The cleanest option is `-[WKWebView takeSnapshotWithConfiguration:completionHandler:]`
//! which returns an `NSImage` asynchronously. This module converts the `NSImage`
//! to a BGRA byte array and writes it into the shared `SharedOsrFrame`.
//!
//! ## Snapshot loop
//!
//! The runtime triggers snapshots:
//! - 4 fps steady tick (250 ms), matching the Firefox-CDP pattern.
//! - After every navigation event (didCommitNavigation / didFinishNavigation).
//! - After every input event dispatched to the WKWebView.
//! - On `osr_resize`.
//!
//! ## Pixel conversion
//!
//! `NSImage` → `NSBitmapImageRep` → raw byte pointer. Apple's bitmap reps
//! default to RGBA ordering; we swap R and B to produce BGRA expected by
//! `SharedOsrFrame`.
//!
//! # TODO(verify-on-mac)
//!
//! Every public API referenced in this file is documented with a
//! `// TODO(verify-on-mac):` annotation. The annotations describe what needs
//! confirmation against the actual objc2-web-kit / objc2-foundation generated
//! bindings on a real macOS host.

use std::sync::atomic::Ordering;

use buffr_engine::{OsrFrame, SharedOsrFrame, SharedOsrViewState};

// ── Blank-frame paint ─────────────────────────────────────────────────────────

/// Write a solid white BGRA frame into `frame` at the current view dimensions.
///
/// Used as the Phase-B placeholder until snapshot extraction is wired. On a
/// real macOS host, `apply_snapshot_to_frame` replaces this with real pixels.
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
    tracing::debug!("webkit-cocoa osr: painted blank frame {w}×{h}");
}

// ── Real pixel extraction (macOS) ─────────────────────────────────────────────
//
// The functions below are the macOS-only pixel-readback path.  They call into
// the objc2-web-kit / objc2-foundation / objc2-app-kit crates. Every call site
// that touches a specific Objective-C API is tagged TODO(verify-on-mac).
//
// On Linux the whole `#[cfg(target_os = "macos")]` block is dead code;
// `paint_blank` above is the active path everywhere.

#[cfg(target_os = "macos")]
pub(crate) use macos::request_snapshot;

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::sync::atomic::Ordering;

    use objc2::rc::Retained;
    use objc2_app_kit::NSImage; // TODO(verify-on-mac): NSImage is in objc2-app-kit 0.3
    use objc2_foundation::NSData; // TODO(verify-on-mac): NSData byte layout on macOS
    use objc2_web_kit::{WKSnapshotConfiguration, WKWebView}; // TODO(verify-on-mac): WKSnapshotConfiguration in objc2-web-kit 0.3

    use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

    /// Enqueue an async snapshot from `web_view` and write the result into
    /// `frame` when the completion handler fires on the main queue.
    ///
    /// # macOS API notes
    ///
    /// `-[WKWebView takeSnapshotWithConfiguration:completionHandler:]` was
    /// added in macOS 10.13. The completion handler fires on the main queue
    /// with an optional `NSImage` and an optional `NSError`. We convert the
    /// `NSImage` to BGRA via `NSBitmapImageRep`.
    ///
    /// # TODO(verify-on-mac)
    ///
    /// - Confirm that `WKWebView::takeSnapshotWithConfiguration_completionHandler`
    ///   is exposed in `objc2-web-kit 0.3`. The binding name in Rust uses
    ///   snake_case with underscores between selector segments — the exact
    ///   generated name may differ.
    /// - Confirm that `WKSnapshotConfiguration::new()` is a valid constructor
    ///   (may need `WKSnapshotConfiguration::init()` pattern from objc2).
    /// - Confirm block2 closure signature: `block2::RcBlock<(Option<&NSImage>, Option<&NSError>), ()>`.
    ///
    /// # Panics
    ///
    /// Does not panic. Logs errors via `tracing::warn!`.
    pub(crate) fn request_snapshot(
        web_view: &WKWebView,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
    ) {
        use block2::RcBlock;
        use objc2_app_kit::NSBitmapImageRep; // TODO(verify-on-mac): NSBitmapImageRep in objc2-app-kit 0.3

        let frame_clone = frame.clone();
        let view_clone = view.clone();

        // TODO(verify-on-mac): Confirm WKSnapshotConfiguration::new() API.
        // objc2 pattern is typically `WKSnapshotConfiguration::alloc().init()`.
        // Using `None` passes nil config which captures the full visible rect.
        let config: Option<&WKSnapshotConfiguration> = None;

        // TODO(verify-on-mac): Confirm the exact block signature for
        // takeSnapshotWithConfiguration:completionHandler:. Apple docs say:
        //   void (^)(NSImage *snapshotImage, NSError *error)
        // In objc2 this becomes something like:
        //   RcBlock<dyn Fn(Option<Retained<NSImage>>, Option<Retained<NSError>>)>
        // The exact import path and type depends on objc2-web-kit 0.3 codegen.
        let completion = RcBlock::new(
            move |snapshot: Option<Retained<NSImage>>,
                  err: Option<Retained<objc2_foundation::NSError>>| {
                if let Some(err) = err {
                    // TODO(verify-on-mac): Confirm NSError localizedDescription path.
                    tracing::warn!("webkit-cocoa: snapshot error: {:?}", err);
                    return;
                }
                let Some(image) = snapshot else {
                    tracing::warn!("webkit-cocoa: snapshot returned nil image");
                    return;
                };
                write_image_to_frame(&image, &frame_clone, &view_clone);
            },
        );

        // TODO(verify-on-mac): Confirm the exact Rust binding name for
        //   -[WKWebView takeSnapshotWithConfiguration:completionHandler:]
        // In objc2-web-kit 0.3 the method may be named:
        //   take_snapshot_with_configuration_completion_handler
        // or similar. If the method is not yet bound, use a fallback:
        //   web_view.layer().contents() for a synchronous CALayer grab.
        unsafe {
            web_view.takeSnapshotWithConfiguration_completionHandler(config, &completion);
        }
        tracing::debug!("webkit-cocoa osr: snapshot requested");
    }

    /// Convert an `NSImage` to BGRA bytes and write into `frame`.
    ///
    /// # TODO(verify-on-mac)
    ///
    /// - `NSBitmapImageRep::initWithData` vs `NSBitmapImageRep::imageRepWithData`
    ///   — the objc2-app-kit bindings may expose only one of these.
    /// - `NSBitmapImageRep::bitmapData()` returns a raw `*mut u8` pointer; we
    ///   must verify that the pointer is valid for `width * height * 4` bytes
    ///   and that the default colour space is sRGB (not linear-light).
    /// - The default pixel format from `takeSnapshotWithConfiguration:` is
    ///   RGBA (not premultiplied). Confirm this is always the case, or add a
    ///   premultiplication check.
    fn write_image_to_frame(image: &NSImage, frame: &SharedOsrFrame, view: &SharedOsrViewState) {
        // TODO(verify-on-mac): Confirm NSImage::TIFFRepresentation and
        //   NSBitmapImageRep::imageRepWithData exist in objc2-app-kit 0.3.
        // The code below follows Apple sample patterns and is not yet
        // compile-verified.
        unsafe {
            let tiff: Option<Retained<NSData>> = image.TIFFRepresentation(); // TODO(verify-on-mac)
            let Some(tiff) = tiff else {
                tracing::warn!("webkit-cocoa osr: TIFFRepresentation returned nil");
                return;
            };

            let rep: Option<Retained<NSBitmapImageRep>> = NSBitmapImageRep::imageRepWithData(&tiff); // TODO(verify-on-mac)
            let Some(rep) = rep else {
                tracing::warn!("webkit-cocoa osr: NSBitmapImageRep init failed");
                return;
            };

            // TODO(verify-on-mac): Confirm pixelsWide / pixelsHigh selector names.
            let w = rep.pixelsWide() as u32; // TODO(verify-on-mac)
            let h = rep.pixelsHigh() as u32; // TODO(verify-on-mac)
            let expected = (w as usize) * (h as usize) * 4;
            if w == 0 || h == 0 {
                tracing::warn!("webkit-cocoa osr: snapshot has zero dimensions");
                return;
            }

            // TODO(verify-on-mac): Confirm bitmapData() returns a *mut u8 valid
            //   for `w * h * 4` bytes. The return type in objc2 may be
            //   `NonNull<u8>` or `*mut u8` depending on nullability annotation.
            let ptr: *mut u8 = rep.bitmapData(); // TODO(verify-on-mac)
            if ptr.is_null() {
                tracing::warn!("webkit-cocoa osr: bitmapData is null");
                return;
            }
            let rgba = std::slice::from_raw_parts(ptr, expected);

            // Swap RGBA → BGRA in-place into a fresh Vec.
            let mut bgra = vec![0u8; expected];
            for i in 0..(w as usize * h as usize) {
                let o = i * 4;
                bgra[o] = rgba[o + 2]; // B ← R
                bgra[o + 1] = rgba[o + 1]; // G stays
                bgra[o + 2] = rgba[o]; // R ← B
                bgra[o + 3] = rgba[o + 3]; // A stays
            }

            if let Ok(mut guard) = frame.lock() {
                guard.width = w;
                guard.height = h;
                guard.pixels = bgra;
                guard.generation = guard.generation.wrapping_add(1);
                guard.needs_fresh = false;
            }
            view.width.store(w, Ordering::Relaxed);
            view.height.store(h, Ordering::Relaxed);
            if let Some(wake) = view.wake.get() {
                wake();
            }
            tracing::debug!("webkit-cocoa osr: frame {w}×{h} written (gen+1)");
        }
    }
}
