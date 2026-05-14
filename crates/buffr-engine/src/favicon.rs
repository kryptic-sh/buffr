//! Favicon sink — engine-agnostic.
//!
//! CEF backends push [`FaviconUpdate`] entries from `download_image`
//! callbacks. The apps layer drains them each tick via
//! [`BrowserEngine::drain_favicon_updates`] and caches the bitmaps by
//! browser id so the tab strip can blit them.
//!
//! The type is structurally identical to `buffr_core::favicon::FaviconUpdate`
//! so no conversion is needed when `buffr-cef` forwards the values.

/// One decoded favicon, ready to blit.
///
/// `pixels` is BGRA `u32` packed (`0xAA_RR_GG_BB` after the BGRA → ARGB
/// swap during decode), `len == width * height`.
#[derive(Debug, Clone)]
pub struct FaviconUpdate {
    /// CEF `Browser::identifier()` of the browser that delivered this icon.
    pub browser_id: i32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}
