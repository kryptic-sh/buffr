//! Favicon download bridge.
//!
//! CEF emits `DisplayHandler::on_favicon_urlchange(browser, urls)` whenever a
//! page's `<link rel="icon">` set changes. We pick the first URL and feed it
//! to `BrowserHost::download_image(url, is_favicon=1, max_size=32, ...)`,
//! which fetches and decodes asynchronously. The decoded `Image` is converted
//! to BGRA `u32` pixels and pushed into a [`FaviconSink`] tagged with the
//! originating CEF browser id; the apps layer drains the sink each tick and
//! caches the bitmap by browser id so the tab strip can blit it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One decoded favicon, ready to blit. `pixels` is BGRA `u32` packed
/// (`0xAA_RR_GG_BB` after the BGRA → ARGB swap during decode), `len ==
/// width * height`.
#[derive(Debug, Clone)]
pub struct FaviconUpdate {
    pub browser_id: i32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// Cross-thread sink for favicon decode results. The CEF callback thread
/// pushes; the apps UI thread drains.
pub type FaviconSink = Arc<Mutex<VecDeque<FaviconUpdate>>>;

pub fn new_favicon_sink() -> FaviconSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Drain every pending update. Returns oldest-first.
pub fn drain_favicon_updates(sink: &FaviconSink) -> Vec<FaviconUpdate> {
    if let Ok(mut g) = sink.lock() {
        g.drain(..).collect()
    } else {
        Vec::new()
    }
}

/// Master on/off switch shared by the host and display handler. When `false`
/// `on_favicon_urlchange` skips the `download_image` round-trip and the apps
/// layer treats the cache as empty.
pub type FaviconEnabled = Arc<AtomicBool>;

pub fn new_favicon_enabled(initial: bool) -> FaviconEnabled {
    Arc::new(AtomicBool::new(initial))
}

pub fn favicon_is_enabled(flag: &FaviconEnabled) -> bool {
    flag.load(Ordering::Relaxed)
}

pub fn set_favicon_enabled(flag: &FaviconEnabled, value: bool) {
    flag.store(value, Ordering::Relaxed);
}
