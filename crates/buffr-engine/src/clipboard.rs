//! Clipboard reader handle, engine-agnostic.
//!
//! [`ClipboardRead`] is the minimal blocking-read interface that backends
//! expose. [`ClipboardReader`] is the `Arc`-wrapped, `Send + Sync` handle
//! the apps layer clones into worker threads to avoid Wayland self-deadlocks
//! (the Wayland `wl_data_source.send` callback runs on the CEF UI thread,
//! which is the main thread; reading the clipboard on the same thread
//! deadlocks the pipe. Worker thread parks while main thread pumps CEF.)
//!
//! CEF backend: wraps `hjkl_clipboard::Clipboard`.
//! CDP backend: returns `None` from `BrowserEngine::clipboard_handle` —
//!   no system clipboard access in Phase 6d.

use std::sync::Arc;

/// Minimal blocking clipboard-read interface.
///
/// Implemented by `buffr-cef` (wrapping `hjkl_clipboard::Clipboard`) and
/// by any future backend that exposes clipboard access. The blanket `impl`
/// keeps the trait object-safe.
pub trait ClipboardRead: Send + Sync {
    /// Block until the system clipboard yields a non-empty UTF-8 text
    /// payload. Returns `None` when the clipboard is empty, non-text,
    /// or the read fails. Must be called **off the CEF UI thread** to
    /// avoid the Wayland self-deadlock when Chromium owns the selection.
    fn read_text(&self) -> Option<String>;
}

/// Thread-safe, `Clone`-able clipboard reader handed out by
/// [`BrowserEngine::clipboard_handle`].
///
/// Clone freely — the underlying handle is shared via `Arc`.
pub type ClipboardReader = Arc<dyn ClipboardRead>;
