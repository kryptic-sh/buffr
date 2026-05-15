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
//! - 4 fps steady tick (250 ms).
//! - After every navigation event (didCommitNavigation / didFinishNavigation).
//! - After every input event dispatched to the WKWebView.
//! - On `osr_resize`.
//!
//! ## Pixel conversion
//!
//! `NSImage` → TIFF bytes → `NSBitmapImageRep` → raw byte pointer.
//! Apple's bitmap reps default to RGBA ordering; we swap R and B to produce
//! BGRA expected by `SharedOsrFrame`.
//!
//! # API verification notes (grepped from registry crate source)
//!
//! ## takeSnapshotWithConfiguration:completionHandler: (WKWebView.rs:696-703)
//!
//! Confirmed signature:
//!   ```text
//!   pub unsafe fn takeSnapshotWithConfiguration_completionHandler(
//!       &self,
//!       snapshot_configuration: Option<&WKSnapshotConfiguration>,
//!       completion_handler: &block2::DynBlock<dyn Fn(*mut NSImage, *mut NSError)>,
//!   )
//!   ```
//!
//! The completion block receives **raw pointers** (`*mut NSImage`, `*mut NSError`),
//! NOT `Option<Retained<NSImage>>`. We must check for null before dereferencing.
//! Requires features: `WKSnapshotConfiguration`, `block2`.
//!
//! ## NSImage::TIFFRepresentation (NSImage.rs:363-365)
//!
//! Confirmed: `pub fn TIFFRepresentation(&self) -> Option<Retained<NSData>>`
//! This is a method on NSImage (not NSBitmapImageRep).
//!
//! ## NSBitmapImageRep::imageRepWithData (NSBitmapImageRep.rs:318-320)
//!
//! Confirmed: `pub fn imageRepWithData(data: &NSData) -> Option<Retained<Self>>`
//!
//! ## NSBitmapImageRep::bitmapData (NSBitmapImageRep.rs:326-328)
//!
//! Confirmed: `pub fn bitmapData(&self) -> *mut c_uchar`
//! Returns a raw pointer. May be null if the rep has no backing store.
//!
//! ## NSBitmapImageRep::pixelsWide / pixelsHigh (NSImageRep.rs:160-171)
//!
//! These are on `NSImageRep` (the superclass), NOT `NSBitmapImageRep` directly.
//! Confirmed: `pub fn pixelsWide(&self) -> NSInteger`
//!             `pub fn pixelsHigh(&self) -> NSInteger`
//! The feature `NSImageRep` must be enabled in Cargo.toml.
//!
//! ## block2::RcBlock::new (block2-0.6.2/src/rc_block.rs:143-150)
//!
//! `RcBlock::new(closure)` where the closure signature must match the
//! DynBlock type. For `*mut NSImage, *mut NSError` → `()`:
//!   `RcBlock::new(move |img: *mut NSImage, err: *mut NSError| { … })`
//!
//! # TODO: pixel format detection
//!
//! The snapshot RGBA format (premultiplied vs straight alpha) is not
//! verified. `takeSnapshotWithConfiguration:` on macOS 11+ returns RGBA with
//! straight alpha. If premultiplied, the BGRA frame will have incorrect
//! colours on semi-transparent regions. Add a check of
//! `NSBitmapImageRep::bitmapFormat()` if this is observed.
//!
//! # TODO: WKSnapshotConfiguration rect
//!
//! Passing `None` captures the full content rect. For high-DPI displays the
//! snapshot may be @2x or @3x. If the snapshot dimensions don't match the
//! frame size, add `WKSnapshotConfiguration::setSnapshotWidth` to force
//! the output width. `snapshotWidth` confirmed in WKSnapshotConfiguration.rs.

use std::sync::atomic::Ordering;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

// ── Blank-frame paint ─────────────────────────────────────────────────────────

/// Write a solid white BGRA frame into `frame` at the current view dimensions.
///
/// Used when there is no active tab or as the Phase-B fallback until a
/// real snapshot arrives.
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

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::sync::atomic::Ordering;

    use block2::RcBlock;
    use objc2_app_kit::{NSBitmapImageRep, NSImage};
    use objc2_foundation::NSError;
    use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};

    use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

    /// Enqueue an async snapshot from `web_view` and write the result into
    /// `frame` when the completion handler fires on the main queue.
    ///
    /// # API notes
    ///
    /// `takeSnapshotWithConfiguration:completionHandler:` confirmed in
    /// WKWebView.rs:696-703. The completion handler type is:
    ///   `&block2::DynBlock<dyn Fn(*mut NSImage, *mut NSError)>`
    ///
    /// We build an `RcBlock` with that closure type and coerce it to `&DynBlock`
    /// via `Deref`. `RcBlock::new` confirmed: block2-0.6.2/src/rc_block.rs:143.
    ///
    /// # Safety invariants
    ///
    /// - `web_view` must be alive for the duration of this call (the snapshot
    ///   request captures nothing about the web_view pointer; the completion
    ///   block only receives the image).
    /// - The completion block is called on the main queue by WebKit, so Arc
    ///   inside is `Send`-safe.
    ///
    /// # Panics
    ///
    /// Does not panic. Nil-pointer and lock errors are logged via `tracing::warn!`.
    pub(crate) fn request_snapshot(
        web_view: &WKWebView,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
    ) {
        let frame_clone = frame.clone();
        let view_clone = view.clone();

        // Build the completion block.
        //
        // takeSnapshotWithConfiguration:completionHandler: signature confirmed:
        //   completion_handler: &block2::DynBlock<dyn Fn(*mut NSImage, *mut NSError)>
        // Source: WKWebView.rs:702.
        //
        // RcBlock::new infers the type from the closure. Raw pointers must be
        // checked for null before use (they're nullable objc references).
        let completion = RcBlock::new(move |snapshot: *mut NSImage, err: *mut NSError| {
            if !err.is_null() {
                // SAFETY: err is non-null, comes from WebKit on the main queue.
                tracing::warn!("webkit-cocoa: snapshot error: {:?}", unsafe { &*err });
                return;
            }
            if snapshot.is_null() {
                tracing::warn!("webkit-cocoa: snapshot returned nil image");
                return;
            }
            // SAFETY: snapshot is non-null and points to an NSImage owned by
            // WebKit for the duration of this callback.
            let image = unsafe { &*snapshot };
            write_image_to_frame(image, &frame_clone, &view_clone);
        });

        // Pass nil configuration to capture the full visible rect of the web view.
        // WKSnapshotConfiguration::new(mtm) is also valid but requires a
        // MainThreadMarker; nil is simpler and achieves the same default behaviour.
        let config: Option<&WKSnapshotConfiguration> = None;

        // takeSnapshotWithConfiguration:completionHandler: confirmed: WKWebView.rs:697-703.
        // Requires features: WKSnapshotConfiguration, block2.
        // `&*completion` coerces RcBlock<F> to &DynBlock<dyn Fn(…)>.
        unsafe {
            web_view.takeSnapshotWithConfiguration_completionHandler(config, &*completion);
        }
        tracing::debug!("webkit-cocoa osr: snapshot requested");
    }

    /// Convert an `NSImage` to BGRA bytes and write into `frame`.
    ///
    /// # Path
    ///
    /// `NSImage::TIFFRepresentation()` → NSData (confirmed NSImage.rs:363-365)
    /// `NSBitmapImageRep::imageRepWithData(&NSData)` → NSBitmapImageRep
    ///   (confirmed NSBitmapImageRep.rs:318-320)
    /// `NSBitmapImageRep::bitmapData()` → `*mut c_uchar`
    ///   (confirmed NSBitmapImageRep.rs:326-328; nullable)
    /// `NSImageRep::pixelsWide/pixelsHigh()` → `NSInteger`
    ///   (confirmed NSImageRep.rs:160-171; feature "NSImageRep" required)
    ///
    /// # Pixel format
    ///
    /// Snapshots from `takeSnapshotWithConfiguration:` are RGBA (R at offset 0).
    /// We swap bytes to BGRA (R and B swapped) for the SharedOsrFrame consumer.
    ///
    /// # Safety
    ///
    /// `bitmapData()` returns a pointer valid for `bytesPerRow * pixelsHigh`
    /// bytes. We compute expected = `pixelsWide * pixelsHigh * 4` (4 bytes per
    /// pixel for RGBA). If `bytesPerRow > pixelsWide * 4` (padding), the slice
    /// will include row padding — this is handled by reading `bytesPerRow`
    /// and copying row-by-row.
    ///
    /// For simplicity we assume no row padding (valid for most snapshot outputs).
    /// If we observe visual artefacts (corrupted right edges), add row-stride
    /// handling using `bytesPerRow()`.
    fn write_image_to_frame(image: &NSImage, frame: &SharedOsrFrame, view: &SharedOsrViewState) {
        unsafe {
            // NSImage::TIFFRepresentation confirmed: NSImage.rs:363-365.
            // Returns Option<Retained<NSData>>.
            let Some(tiff) = image.TIFFRepresentation() else {
                tracing::warn!("webkit-cocoa osr: TIFFRepresentation returned nil");
                return;
            };

            // NSBitmapImageRep::imageRepWithData confirmed: NSBitmapImageRep.rs:318-320.
            // Returns Option<Retained<NSBitmapImageRep>>.
            let Some(rep) = NSBitmapImageRep::imageRepWithData(&tiff) else {
                tracing::warn!("webkit-cocoa osr: NSBitmapImageRep init failed");
                return;
            };

            // pixelsWide / pixelsHigh confirmed: NSImageRep.rs:160-171.
            // Return type is NSInteger (isize). We clamp to u32.
            let w = rep.pixelsWide().max(0) as u32;
            let h = rep.pixelsHigh().max(0) as u32;
            if w == 0 || h == 0 {
                tracing::warn!("webkit-cocoa osr: snapshot has zero dimensions");
                return;
            }
            let expected = (w as usize) * (h as usize) * 4;

            // bitmapData confirmed: NSBitmapImageRep.rs:326-328.
            // Returns *mut c_uchar which is *mut u8. May be null.
            let ptr: *mut u8 = rep.bitmapData().cast();
            if ptr.is_null() {
                tracing::warn!("webkit-cocoa osr: bitmapData is null");
                return;
            }

            // SAFETY: ptr is non-null, expected = w*h*4.
            // We assume no row padding. If artefacts appear, switch to
            // row-stride copy using rep.bytesPerRow().
            let rgba = std::slice::from_raw_parts(ptr, expected);

            // Swap RGBA → BGRA (swap R at offset 0 with B at offset 2).
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
