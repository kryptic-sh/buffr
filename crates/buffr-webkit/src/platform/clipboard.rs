//! System-clipboard reader backed by [`hjkl_clipboard::Clipboard`].
//!
//! [`WebKitClipboardReader`] implements [`buffr_engine::ClipboardRead`] and is
//! stored on [`super::engine::WebKitEngine`] as
//! `Option<Arc<WebKitClipboardReader>>`. `None` when clipboard initialisation
//! failed at startup (no Wayland/X11/OSC-52 display available).

use std::sync::{Arc, OnceLock};

use buffr_engine::ClipboardRead;
use hjkl_clipboard::{Clipboard, MimeType, Selection};

/// Process-wide clipboard handle, probed at most once.
///
/// W10: a `Clipboard` used to be constructed per tab, per
/// `buffr-clipboard:read` request, *and* once per engine — each opening its
/// own Wayland connection. `Clipboard` holds a `Box<dyn Backend>` and
/// `Backend: Send + Sync + 'static`, so a single `Arc<Clipboard>` is safe to
/// share across the engine thread, the GLib worker thread, and the
/// short-lived scheme-request threads.
///
/// `OnceLock<Option<_>>` rather than `OnceLock<Arc<_>>` so a failed probe is
/// cached too — a headless session should not re-probe on every copy event.
static SHARED_CLIPBOARD: OnceLock<Option<Arc<Clipboard>>> = OnceLock::new();

/// The shared system clipboard handle, or `None` when no backend is available
/// (headless / SSH without OSC-52). Cheap after the first call.
pub(crate) fn shared_clipboard() -> Option<Arc<Clipboard>> {
    SHARED_CLIPBOARD
        .get_or_init(|| match Clipboard::new() {
            Ok(cb) => Some(Arc::new(cb)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "webkit: clipboard init failed; clipboard access is disabled for this process"
                );
                None
            }
        })
        .clone()
}

/// [`ClipboardRead`] implementation backed by `hjkl_clipboard`.
///
/// `Clipboard` wraps a `Box<dyn Backend: Send + Sync>`, so it is itself
/// `Send + Sync` — no `Mutex` needed.
pub(crate) struct WebKitClipboardReader {
    pub(super) inner: Arc<Clipboard>,
}

impl WebKitClipboardReader {
    /// Wrap the process-wide clipboard handle (W10).
    ///
    /// Returns `None` if no backend is available (headless environments, etc.).
    pub(crate) fn new() -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            inner: shared_clipboard()?,
        }))
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
