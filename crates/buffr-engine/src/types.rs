//! Neutral types surfaced by the engine to the apps layer.

use std::ffi::c_void;

// ── WaylandNativeHandles ─────────────────────────────────────────────────────

/// Raw Wayland and EGL handles extracted from winit and the host display.
///
/// Populated by `buffr-app` on Wayland sessions after the winit window is
/// created.  Stored on `WebKitEngine` by `set_native_wayland_handles`; consumed
/// by the `BuffrDisplayWayland` C subclass that lands in #152.  All pointers
/// are valid for the lifetime of the host process.
///
/// # Safety invariants
///
/// - `wl_display` and `parent_wl_surface` point into winit's Wayland
///   connection.  They remain alive as long as the `Arc<Window>` lives, which
///   is at least as long as `AppState` — i.e. the whole process lifetime.
/// - `wl_compositor` / `wl_subcompositor` are bound globals from the same
///   `wl_registry` roundtrip; their lifetimes are tied to `wl_display`.
/// - `egl_display` (`EGLDisplay*`) is valid for the lifetime of the
///   `EglWorker` inside `WpeRuntime`.  Set to `null` when EGL setup is
///   deferred to #152.
#[derive(Debug, Clone, Copy)]
pub struct WaylandNativeHandles {
    /// `wl_display*` — the host Wayland display connection.
    pub wl_display: *mut c_void,
    /// `wl_surface*` — the parent/host `wl_surface` (the winit window's surface).
    pub parent_wl_surface: *mut c_void,
    /// `wl_compositor*` bound from `wl_registry`.  `null` if binding failed.
    pub wl_compositor: *mut c_void,
    /// `wl_subcompositor*` bound from `wl_registry`.  `null` if binding failed.
    pub wl_subcompositor: *mut c_void,
    /// `EGLDisplay` for the Wayland platform display.  `null` when deferred.
    pub egl_display: *mut c_void,
}

// SAFETY: the pointers are valid for the process lifetime (see doc above).
// They are never written to after construction; the only mutation is the
// atomic swap inside `WebKitEngine::set_native_wayland_handles`, which stores
// into a `Mutex<Option<_>>`.  No aliased mutable access.
unsafe impl Send for WaylandNativeHandles {}
unsafe impl Sync for WaylandNativeHandles {}

/// Rectangle in physical pixels relative to a host native surface origin.
///
/// Used by [`BrowserEngine::set_native_parent`] to tell the engine where its
/// rendered content should appear within the host's window. Phase 3 of the
/// WPE WebKit umbrella (#109) uses this to place a Wayland subsurface; future
/// Cocoa native backends will read the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Audio stream activity change for one browser.
#[derive(Debug, Clone, Copy)]
pub struct AudioEvent {
    /// Engine-internal browser id.
    pub browser_id: i32,
    pub active: bool,
}

/// Context-menu request. Emitted by the engine when the user right-clicks.
///
/// `x` / `y` are browser-local pixel coordinates (CSS pixels × device scale
/// only if the backend normalises them — Phase 1 inherits CEF's convention
/// which is CSS pixels before device scaling).
#[derive(Debug, Clone)]
pub struct ContextMenuRequest {
    /// Engine-internal browser id.
    pub browser_id: i32,
    pub x: i32,
    pub y: i32,
    pub page_url: String,
    pub frame_url: String,
    pub link_url: Option<String>,
    pub image_url: Option<String>,
    pub media_url: Option<String>,
    pub selection_text: Option<String>,
    pub is_editable: bool,
    pub has_image_contents: bool,
    pub media_type: MediaType,
}

/// What kind of content is under the context-menu cursor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    None,
    Image,
    Video,
    Audio,
    Canvas,
    File,
    Plugin,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_event_carries_browser_id() {
        let ev = AudioEvent {
            browser_id: 7,
            active: true,
        };
        assert_eq!(ev.browser_id, 7);
        assert!(ev.active);
    }
}
