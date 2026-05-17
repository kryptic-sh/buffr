//! System-clipboard reader backed by [`hjkl_clipboard::Clipboard`].
//!
//! [`WebKitClipboardReader`] implements [`buffr_engine::ClipboardRead`] and is
//! stored on [`super::engine::WebKitEngine`] as
//! `Option<Arc<WebKitClipboardReader>>`. `None` when clipboard initialisation
//! failed at startup (no Wayland/X11/OSC-52 display available).

use std::sync::Arc;

use buffr_engine::ClipboardRead;
use hjkl_clipboard::{Clipboard, MimeType, Selection};

/// [`ClipboardRead`] implementation backed by `hjkl_clipboard`.
///
/// `Clipboard` wraps a `Box<dyn Backend: Send + Sync>`, so it is itself
/// `Send + Sync` — no `Mutex` needed.
pub(crate) struct WebKitClipboardReader {
    pub(super) inner: Clipboard,
}

impl WebKitClipboardReader {
    /// Probe for the best available system clipboard backend and wrap it.
    ///
    /// Returns `None` if no backend is available (headless environments, etc.).
    pub(crate) fn new() -> Option<Arc<Self>> {
        match Clipboard::new() {
            Ok(cb) => Some(Arc::new(Self { inner: cb })),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "webkit: clipboard init failed; clipboard_handle will return None"
                );
                None
            }
        }
    }
}

impl ClipboardRead for WebKitClipboardReader {
    fn read_text(&self) -> Option<String> {
        match self.inner.get(Selection::Clipboard, MimeType::Text) {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            Err(e) => {
                tracing::warn!(error = %e, "webkit: clipboard read_text failed");
                None
            }
        }
    }
}
