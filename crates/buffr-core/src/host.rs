//! [`BrowserHost`] — owns a single CEF browser attached to a native
//! window via the `cef` crate's windowed-rendering path.
//!
//! Phase 1: windowed mode (CEF child-window inside a winit window).
//! Phase 3 will switch to off-screen rendering (OSR) so we can
//! composite native chrome on top of the CEF surface.

use cef::{BrowserSettings, CefString, WindowInfo, browser_host_create_browser};
use raw_window_handle::RawWindowHandle;
use tracing::info;

use crate::CoreError;

/// Owns a CEF browser attached to a native window.
///
/// The host is created **after** `cef::initialize` succeeds. On
/// Linux/X11 we hand the X11 window XID to CEF via `WindowInfo`. On
/// Wayland CEF currently expects an X11 window (XWayland fallback) —
/// future phases will swap in OSR.
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
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(handle) => {
                window_info.parent_window = handle.window as _;
            }
            // Wayland needs XWayland — winit returns Wayland by
            // default, so on most Linux desktops users will hit this.
            // TODO: switch to OSR in Phase 3 to support Wayland natively.
            #[cfg(target_os = "linux")]
            RawWindowHandle::Wayland(_) => {
                tracing::warn!(
                    "Wayland window handle received; CEF needs an X11 window. \
                     Phase 1 only supports X11 (`WINIT_UNIX_BACKEND=x11`)."
                );
                return Err(CoreError::CreateBrowserFailed);
            }
            other => {
                tracing::warn!(?other, "unsupported window handle for Phase 1");
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
}
