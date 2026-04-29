//! Shared cursor state pushed by CEF's `DisplayHandler::on_cursor_change`.
//!
//! CEF reports a semantic [`CursorType`] (pointer, hand, ibeam, …) every time
//! the page wants to change the system cursor (e.g. hovering an `<a>` or an
//! `<input>`). Those callbacks fire on the CEF IO thread; the embedder reads
//! the latest value from the UI thread and forwards it to winit's
//! `Window::set_cursor`.
//!
//! The state is one slot — last writer wins. CEF emits cursor changes
//! frequently enough that coalescing is desirable; we don't queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

/// Default cursor kind = `CursorType::POINTER` raw discriminant (0).
const DEFAULT_KIND: u32 = 0;

/// Latest cursor request from CEF, paired with the originating browser id so
/// the apps layer can route it to the right winit window (main tab vs popup).
pub struct CursorState {
    kind: AtomicU32,
    /// CEF `Browser::identifier()` of the browser that emitted the cursor
    /// change. `-1` until the first event lands.
    browser_id: AtomicI32,
    dirty: AtomicBool,
}

impl CursorState {
    pub fn new() -> Self {
        Self {
            kind: AtomicU32::new(DEFAULT_KIND),
            browser_id: AtomicI32::new(-1),
            dirty: AtomicBool::new(false),
        }
    }

    /// Called from CEF IO thread on every cursor change.
    pub fn store(&self, browser_id: i32, kind_raw: u32) {
        self.kind.store(kind_raw, Ordering::Relaxed);
        self.browser_id.store(browser_id, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
    }

    /// Called from UI thread; returns `Some((browser_id, kind_raw))` if a new
    /// cursor is pending, `None` otherwise. Clears the dirty flag on `Some`.
    pub fn take(&self) -> Option<(i32, u32)> {
        if self.dirty.swap(false, Ordering::Acquire) {
            Some((
                self.browser_id.load(Ordering::Relaxed),
                self.kind.load(Ordering::Relaxed),
            ))
        } else {
            None
        }
    }
}

impl Default for CursorState {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared cursor state.
pub type SharedCursorState = Arc<CursorState>;
