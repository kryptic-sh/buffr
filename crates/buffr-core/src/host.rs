//! [`BrowserHost`] — owns a single CEF browser attached to a native
//! window via the `cef` crate's windowed-rendering path.
//!
//! ## Linux backend matrix
//!
//! - **Default build (no `osr` feature)**: windowed embedding only.
//!   CEF on Linux only supports embedding into an X11 window. We force
//!   winit to its X11 backend in `apps/buffr/src/main.rs`, so on
//!   Wayland sessions we transparently run via XWayland and CEF still
//!   gets an X11 XID.
//! - **`osr` feature (Phase 3)**: native Wayland via off-screen
//!   rendering. CEF paints into a buffer, we composite it onto a
//!   winit/wgpu Wayland surface ourselves. Scaffolded in
//!   [`crate::osr`] but not yet implemented.

use cef::{BrowserSettings, CefString, WindowInfo, browser_host_create_browser};
use raw_window_handle::RawWindowHandle;
use tracing::info;

use crate::CoreError;

/// Owns a CEF browser attached to a native window.
///
/// The host is created **after** `cef::initialize` succeeds. On Linux
/// (default build) we hand the X11 window XID to CEF via `WindowInfo` —
/// this works for both native X11 sessions and Wayland sessions running
/// XWayland, because we force winit to its X11 backend before creating
/// the event loop.
pub struct BrowserHost {
    /// CEF returns the [`Browser`](cef::Browser) handle asynchronously
    /// via the client callback; for Phase 1 we don't track it yet.
    _placeholder: (),
}

impl BrowserHost {
    /// Create a browser attached to `window_handle`, navigating to `url`.
    ///
    /// `window_handle` is the platform window the CEF browser will be
    /// parented to. On Linux this must be the X11 XID of a winit
    /// window.
    pub fn new(window_handle: RawWindowHandle, url: &str) -> Result<Self, CoreError> {
        info!(target: "buffr_core::host", %url, "creating CEF browser");

        let mut window_info = WindowInfo {
            // Phase 1: 1280x800 client area; Phase 3 will resize from
            // the parent winit window's inner size each frame.
            ..WindowInfo::default()
        };
        window_info.bounds = cef::Rect {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };

        match window_handle {
            // XWayland: winit gives us an Xlib handle even on Wayland
            // sessions when the event loop is built with `with_x11()`,
            // because the compositor proxies an X11 server (Xwayland)
            // for legacy clients. CEF only supports windowed embedding
            // into X11 on Linux, so this is the one supported arm in
            // the default build.
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(handle) => {
                window_info.parent_window = handle.window as _;
            }
            other => {
                tracing::warn!(
                    ?other,
                    "unsupported window handle for windowed embedding; \
                     native Wayland requires the `osr` feature (Phase 3)"
                );
                return Err(CoreError::CreateBrowserFailed);
            }
        }

        let url = CefString::from(url);
        let settings = BrowserSettings::default();

        let created = browser_host_create_browser(
            Some(&window_info),
            None, // no custom Client impl in Phase 1
            Some(&url),
            Some(&settings),
            None,
            None,
        );

        if created != 1 {
            return Err(CoreError::CreateBrowserFailed);
        }

        Ok(Self { _placeholder: () })
    }

    /// Construct a browser in **off-screen rendering** mode for native
    /// Wayland support.
    ///
    /// **Not yet implemented** — currently panics with
    /// `unimplemented!()`. See [`crate::osr`] and `PLAN.md` (Phase 3)
    /// for the planned architecture.
    ///
    /// The signature is intentionally plausible so future work has a
    /// concrete target shape: callers will pass the host window handle
    /// (so we can resize / track DPI), the initial URL, and a paint
    /// callback that receives RGBA pixel buffers from CEF's `OnPaint`
    /// every frame for compositing into a wgpu/softbuffer surface.
    #[cfg(feature = "osr")]
    pub fn new_osr(
        _window_handle: RawWindowHandle,
        url: &str,
        _paint_callback: impl Fn(&[u8], u32, u32) + Send + 'static,
    ) -> Result<Self, CoreError> {
        tracing::error!(
            %url,
            "BrowserHost::new_osr called but OSR mode is not implemented yet"
        );
        unimplemented!("OSR mode coming in Phase 3 — see PLAN.md")
    }
}
