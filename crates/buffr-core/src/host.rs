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

use std::sync::Arc;

use buffr_history::History;
use cef::{
    BrowserSettings, CefString, CefStringUtf16, ImplBrowser, ImplBrowserHost, ImplFrame,
    WindowInfo, browser_host_create_browser_sync,
};
use raw_window_handle::RawWindowHandle;
use tracing::{info, warn};

use crate::{CoreError, handlers};

/// Owns a CEF browser attached to a native window.
///
/// The host is created **after** `cef::initialize` succeeds. On Linux
/// (default build) we hand the X11 window XID to CEF via `WindowInfo` —
/// this works for both native X11 sessions and Wayland sessions running
/// XWayland, because we force winit to its X11 backend before creating
/// the event loop.
pub struct BrowserHost {
    /// Live `cef::Browser` handle. Phase 2 acquires this via the
    /// **synchronous** `browser_host_create_browser_sync` entry point
    /// so the host has something to dispatch against immediately —
    /// the async `browser_host_create_browser` returns only an `int`
    /// success code and surfaces the `Browser` later via the
    /// `LifeSpanHandler::on_after_created` callback, which is harder
    /// to plumb through to the page-action dispatcher.
    browser: cef::Browser,
}

impl BrowserHost {
    /// Create a browser attached to `window_handle`, navigating to `url`.
    ///
    /// `window_handle` is the platform window the CEF browser will be
    /// parented to. On Linux this must be the X11 XID of a winit
    /// window.
    pub fn new(
        window_handle: RawWindowHandle,
        url: &str,
        history: Arc<History>,
    ) -> Result<Self, CoreError> {
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

        // Phase 5: hand CEF a `Client` whose `get_load_handler` /
        // `get_display_handler` plumb visits into `buffr-history`.
        // Without a custom client, `on_load_end` never fires.
        let mut client = handlers::make_client(history);

        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&settings),
            None,
            None,
        )
        .ok_or(CoreError::CreateBrowserFailed)?;

        Ok(Self { browser })
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

    /// Dispatch a [`buffr_modal::PageAction`] to the live CEF
    /// browser.
    ///
    /// Variants that are wired into real CEF calls today:
    /// scrolls (via `executeJavaScript` on the main frame), history
    /// (`go_back` / `go_forward`), reload (`reload` /
    /// `reload_ignore_cache`), `stop_load`, zoom (`set_zoom_level` on
    /// the host), and `OpenDevTools` (`show_dev_tools` with default
    /// window-info / settings).
    ///
    /// Variants that are stubbed-with-log because they need UI chrome
    /// (Phase 3) or multi-tab plumbing (Phase 5): find/find-next,
    /// tab*, omnibar / command-line / hint mode, edit mode (blocked on
    /// hjkl `Host` trait), `YankUrl` (clipboard plumbing in Phase 5),
    /// and explicit `EnterMode` (the engine already tracks mode
    /// internally — this method just logs).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // After `cef::initialize` succeeded and a winit window exists:
    /// let host = buffr_core::BrowserHost::new(raw_handle, "https://example.com")?;
    /// host.dispatch(&buffr_modal::PageAction::ScrollDown(3));
    /// host.dispatch(&buffr_modal::PageAction::Reload);
    /// ```
    pub fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;
        match action {
            // -- scrolls ---------------------------------------------
            A::ScrollUp(n) => self.scroll_by(0, -(STEP_PX * (*n as i64))),
            A::ScrollDown(n) => self.scroll_by(0, STEP_PX * (*n as i64)),
            A::ScrollLeft(n) => self.scroll_by(-(STEP_PX * (*n as i64)), 0),
            A::ScrollRight(n) => self.scroll_by(STEP_PX * (*n as i64), 0),

            A::ScrollPageDown | A::ScrollFullPageDown => {
                self.run_js("window.scrollBy(0, window.innerHeight * 0.9);")
            }
            A::ScrollPageUp | A::ScrollFullPageUp => {
                self.run_js("window.scrollBy(0, -window.innerHeight * 0.9);")
            }
            A::ScrollHalfPageDown => self.run_js("window.scrollBy(0, window.innerHeight * 0.5);"),
            A::ScrollHalfPageUp => self.run_js("window.scrollBy(0, -window.innerHeight * 0.5);"),
            A::ScrollTop => self.run_js("window.scrollTo(0, 0);"),
            A::ScrollBottom => self.run_js("window.scrollTo(0, document.body.scrollHeight);"),

            // -- history ---------------------------------------------
            A::HistoryBack => self.browser.go_back(),
            A::HistoryForward => self.browser.go_forward(),
            A::Reload => self.browser.reload(),
            A::ReloadHard => self.browser.reload_ignore_cache(),
            A::StopLoading => self.browser.stop_load(),

            // -- zoom ------------------------------------------------
            A::ZoomIn => self.adjust_zoom(0.25),
            A::ZoomOut => self.adjust_zoom(-0.25),
            A::ZoomReset => self.set_zoom(0.0),

            // -- devtools --------------------------------------------
            A::OpenDevTools => {
                if let Some(host) = self.browser.host() {
                    let window_info = WindowInfo::default();
                    let settings = BrowserSettings::default();
                    host.show_dev_tools(Some(&window_info), None, Some(&settings), None);
                } else {
                    warn!("OpenDevTools: browser.host() returned None");
                }
            }

            // -- find: stubbed (Phase 3 owns the command-line UI) ---
            A::Find { forward } => {
                tracing::info!(
                    forward = *forward,
                    "find action — UI not yet implemented (Phase 3)"
                );
            }
            A::FindNext | A::FindPrev => {
                tracing::info!("find-next/prev — UI not yet implemented (Phase 3)");
            }

            // -- tabs: single-tab Phase 2 ---------------------------
            A::TabNext | A::TabPrev | A::TabClose | A::TabNew => {
                tracing::info!("tab action — multi-tab is Phase 5; single-tab buffr ignores this");
            }

            // -- chrome overlays: Phase 3 ---------------------------
            A::OpenOmnibar | A::OpenCommandLine | A::EnterHintMode | A::EnterHintModeBackground => {
                tracing::info!("UI action — overlay rendering is Phase 3 chrome work");
            }

            // -- mode transitions: engine owns mode -----------------
            A::EnterMode(mode) => {
                tracing::info!(?mode, "EnterMode — engine tracks mode internally");
            }
            A::EnterEditMode => {
                tracing::info!(
                    "edit-mode requested — hjkl-engine integration is Phase 2b \
                     (blocked on hjkl Host trait)"
                );
            }

            // -- yank-url: clipboard is Phase 5 ---------------------
            A::YankUrl => {
                if let Some(frame) = self.browser.main_frame() {
                    let url = CefStringUtf16::from(&frame.url()).to_string();
                    tracing::info!(url, "would copy to clipboard — clipboard is Phase 5");
                } else {
                    tracing::info!("would copy to clipboard — main frame unavailable");
                }
            }
        }
    }

    fn run_js(&self, code: &str) {
        let Some(frame) = self.browser.main_frame() else {
            warn!("run_js: main frame unavailable");
            return;
        };
        let code = CefString::from(code);
        // `script_url` is purely diagnostic — Chromium uses it in
        // stack traces. Use a fixed marker so devtools shows where
        // these came from.
        let script_url = CefString::from("buffr://page-action");
        frame.execute_java_script(Some(&code), Some(&script_url), 0);
    }

    fn scroll_by(&self, dx: i64, dy: i64) {
        // Format inline so we don't drag a `format!`-string allocator
        // into every key press. Numbers are cheap.
        let code = format!("window.scrollBy({dx}, {dy});");
        self.run_js(&code);
    }

    fn adjust_zoom(&self, delta: f64) {
        let Some(host) = self.browser.host() else {
            warn!("adjust_zoom: browser.host() returned None");
            return;
        };
        let new_level = host.zoom_level() + delta;
        host.set_zoom_level(new_level);
    }

    fn set_zoom(&self, level: f64) {
        let Some(host) = self.browser.host() else {
            warn!("set_zoom: browser.host() returned None");
            return;
        };
        host.set_zoom_level(level);
    }
}

/// Pixels per scroll-unit. `ScrollDown(3)` therefore moves 120px,
/// matching a typical "tap j three times" feel without making each
/// `j` feel laggy. Half/full-page scrolls go through their own
/// `window.innerHeight`-relative path so they're DPI-independent.
const STEP_PX: i64 = 40;
