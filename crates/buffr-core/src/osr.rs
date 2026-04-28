//! Off-screen rendering (OSR) host — native Wayland path.
//!
//! ## Status: scaffold
//!
//! Phase 3 work. Public surface here is intentionally a stub so the
//! shape is visible from the outside, but no entry point does real
//! work — every constructor panics with `unimplemented!()`.
//!
//! ## Architecture (planned)
//!
//! In windowed mode (default build, see [`crate::host::BrowserHost`])
//! CEF owns the embedded X11 child window itself and we hand it a
//! parent XID. That doesn't work on Wayland because:
//!
//! 1. Wayland has no concept of foreign-process subsurface embedding
//!    that CEF can target.
//! 2. CEF's Linux backend hard-codes X11 for windowed mode.
//!
//! OSR sidesteps both: CEF renders into a CPU/GPU buffer that we
//! receive in `CefRenderHandler::OnPaint`, and *we* composite it onto
//! a winit-owned Wayland surface. That decouples rendering from
//! windowing entirely.
//!
//! ```text
//!   +--------------+    OnPaint(rgba, w, h)     +----------------------+
//!   |  CEF (OSR)   | -------------------------> |  Compositor (wgpu /  |
//!   |  no window   |                            |  softbuffer fallback)|
//!   +--------------+                            +----------+-----------+
//!                                                          |
//!                                                          v
//!                                              +-----------+----------+
//!                                              |   winit Wayland      |
//!                                              |   surface            |
//!                                              +----------------------+
//! ```
//!
//! The fallback path (`softbuffer`) lets us ship Wayland support even
//! on systems without a wgpu-capable GPU; the fast path uses a wgpu
//! texture upload + a fullscreen quad blit.
//!
//! ## See also
//!
//! - `PLAN.md` — Phase 3 OSR roadmap.
//! - [`crate::host::HostMode`] — runtime mode selection.

use raw_window_handle::RawWindowHandle;

use crate::CoreError;

/// Off-screen rendering browser host.
///
/// Owns a CEF browser running in windowless mode plus the compositor
/// that blits its frames onto a Wayland surface. Fields are TBD until
/// Phase 3 wires real CEF + wgpu in.
pub struct OsrHost {
    // Intentionally empty for the scaffold. Real fields will land with
    // the Phase 3 implementation: a `cef::Browser` handle, a
    // `Box<dyn Compositor>`, and the parent window handle for
    // resize/DPI tracking.
    _phantom: (),
}

impl OsrHost {
    /// Construct an OSR host bound to `window_handle`, navigating to
    /// `url`, with frames forwarded to `compositor`.
    ///
    /// **Not yet implemented.** Panics with `unimplemented!()` — see
    /// `PLAN.md` (Phase 3).
    pub fn new(
        _window_handle: RawWindowHandle,
        url: &str,
        _compositor: Box<dyn Compositor>,
    ) -> Result<Self, CoreError> {
        tracing::error!(%url, "OsrHost::new called — OSR mode is not implemented yet");
        unimplemented!("Phase 3: wire CEF windowless mode + wgpu compositor")
    }
}

/// Sink for CEF paint events.
///
/// Implementations upload the RGBA `buffer` (size `w * h * 4`,
/// pre-multiplied BGRA per CEF's convention) into a GPU texture and
/// blit it onto the host window's surface.
///
/// The Phase 3 implementation will provide:
///
/// - `WgpuCompositor` — fast path. Uploads via `queue.write_texture`,
///   draws a fullscreen quad through a `wgpu::RenderPipeline`.
/// - `SoftbufferCompositor` — CPU fallback. Direct memcpy into a
///   `softbuffer::Buffer`.
pub trait Compositor: Send {
    /// Called once per CEF frame with a freshly painted buffer.
    ///
    /// `buffer` is borrowed for the duration of the call; implementors
    /// must copy/upload before returning.
    fn on_paint(&mut self, buffer: &[u8], w: u32, h: u32);
}
