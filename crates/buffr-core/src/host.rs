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

use std::sync::{Arc, Mutex};

use buffr_config::DownloadsConfig;
use buffr_downloads::Downloads;
use buffr_history::History;
use buffr_zoom::ZoomStore;
use cef::{
    BrowserSettings, CefString, CefStringUtf16, ImplBrowser, ImplBrowserHost, ImplFrame,
    WindowInfo, browser_host_create_browser_sync,
};
use raw_window_handle::RawWindowHandle;
use tracing::{info, warn};

use crate::find::FindResultSink;
use crate::hint::{
    DEFAULT_HINT_SELECTORS, Hint, HintAction, HintAlphabet, HintEventSink, HintSession,
    build_inject_script,
};
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
    /// Downloads store retained for `PageAction::ClearCompletedDownloads`
    /// dispatch. The CEF `DownloadHandler` already owns its own clone
    /// inside the `Client`; this is a separate `Arc` for direct
    /// mutations from the page-action dispatcher.
    downloads: Arc<Downloads>,
    /// Per-site zoom store. `ZoomIn` / `ZoomOut` / `ZoomReset` write
    /// through here; the CEF `LoadHandler` reads on load to restore.
    zoom: Arc<ZoomStore>,
    /// Last successful find query. `FindNext` / `FindPrev` reuse this
    /// — they are no-ops when no `start_find` has run yet.
    last_find_query: Mutex<Option<String>>,
    /// Mailbox for renderer-emitted hint events. The display handler
    /// writes; the host's hint API reads.
    hint_sink: HintEventSink,
    /// Hint session state. `None` outside of hint mode; `Some(...)`
    /// between `enter_hint_mode` and the eventual commit / cancel.
    hint_session: Mutex<Option<HintSession>>,
    /// User-configured alphabet, snapshotted at construction. The
    /// session derives labels from this on entry.
    hint_alphabet: HintAlphabet,
}

impl BrowserHost {
    /// Create a browser attached to `window_handle`, navigating to `url`.
    ///
    /// `window_handle` is the platform window the CEF browser will be
    /// parented to. On Linux this must be the X11 XID of a winit
    /// window.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window_handle: RawWindowHandle,
        url: &str,
        history: Arc<History>,
        downloads: Arc<Downloads>,
        downloads_config: Arc<DownloadsConfig>,
        zoom: Arc<ZoomStore>,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        hint_alphabet: HintAlphabet,
        initial_size: (u32, u32),
    ) -> Result<Self, CoreError> {
        info!(target: "buffr_core::host", %url, "creating CEF browser");

        // Phase 3 chrome: CEF's child window sits above a software-blit
        // statusline strip the host window owns. Caller passes the
        // already-trimmed width/height so the CEF child rect doesn't
        // overlap the strip; on resize, `BrowserHost::resize` updates
        // the rect and calls `was_resized()`.
        let (init_w, init_h) = initial_size;
        let mut window_info = WindowInfo {
            bounds: cef::Rect {
                x: 0,
                y: 0,
                width: init_w as i32,
                height: init_h as i32,
            },
            ..WindowInfo::default()
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
        // `get_display_handler` / `get_download_handler` plumb events
        // into `buffr-history` + `buffr-downloads`. Without a custom
        // client, `on_load_end` / `on_before_download` never fire.
        let mut client = handlers::make_client(
            history,
            downloads.clone(),
            downloads_config,
            zoom.clone(),
            find_sink,
            hint_sink.clone(),
        );

        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&settings),
            None,
            None,
        )
        .ok_or(CoreError::CreateBrowserFailed)?;

        Ok(Self {
            browser,
            downloads,
            zoom,
            last_find_query: Mutex::new(None),
            hint_sink,
            hint_session: Mutex::new(None),
            hint_alphabet,
        })
    }

    /// Reflow the CEF child window to a new size after the host
    /// `winit` window resized. Caller passes the *child* rect (i.e.
    /// the page area, not including the statusline strip).
    ///
    /// Whether `was_resized()` alone is enough depends on CEF's host
    /// platform: on Linux/X11 the embedded child is a real window
    /// owned by CEF and does *not* automatically follow the parent's
    /// new geometry — `was_resized` notifies the renderer of new
    /// content dimensions but the X11 child still needs to be
    /// repositioned/resized externally for some compositors. We don't
    /// touch the X11 geometry directly here; that requires linking
    /// `xlib` or going through `cef_window_handle_t`-typed APIs which
    /// the cef-rs 147 wrapper doesn't expose. For now we rely on the
    /// renderer-side `was_resized` and the X11 server's own size-hint
    /// propagation (XWayland honours this; pure Mutter / KWin may
    /// need `set_window_position` follow-up — punted to Phase 3b
    /// since the smoke test path runs at fixed size).
    pub fn resize(&self, _width: u32, _height: u32) {
        if let Some(host) = self.browser.host() {
            host.was_resized();
        }
    }

    /// Navigate the main frame to `url`. Used by the omnibar (Enter
    /// on a typed URL or selected suggestion) and by `:open <url>` in
    /// the command line.
    ///
    /// Empty / whitespace-only input is silently dropped — the caller
    /// is expected to short-circuit before calling. URLs that don't
    /// parse are still passed through to CEF, which will surface its
    /// own error page (`net::ERR_INVALID_URL`).
    pub fn navigate(&self, url: &str) -> Result<(), CoreError> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidUrl(String::new()));
        }
        let Some(frame) = self.browser.main_frame() else {
            warn!("navigate: main frame unavailable");
            return Err(CoreError::CreateBrowserFailed);
        };
        let cef_url = CefString::from(trimmed);
        frame.load_url(Some(&cef_url));
        info!(target: "buffr_core::host", url = %trimmed, "navigate");
        Ok(())
    }

    /// Begin a fresh find session. Stores the query so subsequent
    /// `FindNext`/`FindPrev` reuse it; CEF's first call with
    /// `find_next = false` resets the match list, then we drive
    /// forward steps via `find_next = true`.
    ///
    /// `forward` selects search direction. `match_case` is hardcoded
    /// to false for now — Phase 3b will surface it via the command
    /// bar.
    pub fn start_find(&self, query: &str, forward: bool) {
        if query.is_empty() {
            self.stop_find();
            return;
        }
        let Some(host) = self.browser.host() else {
            warn!("start_find: browser.host() returned None");
            return;
        };
        let cef_query = CefString::from(query);
        // `find_next = 0` for the initial call — CEF treats a fresh
        // query as a new search.
        host.find(Some(&cef_query), forward as i32, 0, 0);
        if let Ok(mut slot) = self.last_find_query.lock() {
            *slot = Some(query.to_string());
        }
    }

    /// Cancel the active find. Clears CEF's selection and forgets the
    /// last query so `FindNext` becomes a no-op.
    pub fn stop_find(&self) {
        if let Some(host) = self.browser.host() {
            // `clear_selection = 1` — drop the highlight so the page
            // doesn't keep pointing at a stale match.
            host.stop_finding(1);
        }
        if let Ok(mut slot) = self.last_find_query.lock() {
            *slot = None;
        }
    }

    /// Step to the next/previous match using the last successful
    /// query. No-op when `start_find` has not been called.
    fn find_step(&self, forward: bool) {
        let query = match self.last_find_query.lock() {
            Ok(g) => g.clone(),
            Err(_) => None,
        };
        let Some(query) = query else {
            tracing::info!("find_step: no active query — call start_find first");
            return;
        };
        let Some(host) = self.browser.host() else {
            warn!("find_step: browser.host() returned None");
            return;
        };
        let cef_query = CefString::from(query.as_str());
        // `find_next = 1` continues the existing match list.
        host.find(Some(&cef_query), forward as i32, 0, 1);
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
            //
            // Each variant writes through to `ZoomStore` so the next
            // load on this domain restores the persisted level. The
            // domain comes from the live main-frame URL via
            // `buffr_zoom::domain_for_url` — `about:`/`data:`/`file:`
            // collapse to the global key.
            A::ZoomIn => self.adjust_zoom(0.25),
            A::ZoomOut => self.adjust_zoom(-0.25),
            A::ZoomReset => self.reset_zoom(),

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

            // -- find ------------------------------------------------
            //
            // `Find { forward }` opens a fresh find session, but the
            // command bar that prompts the user for a query is Phase
            // 3b. `start_find` / `stop_find` are exposed on the host
            // so the future command bar wires straight in. For now,
            // `Find` is a tracing breadcrumb. `FindNext` / `FindPrev`
            // *do* work — they reuse the last query stashed by
            // `start_find` (the smoke flag `--find` exercises this).
            A::Find { forward } => {
                tracing::warn!(
                    forward = *forward,
                    "Find requires command line — Phase 3b. Use BrowserHost::start_find() directly."
                );
            }
            A::FindNext => self.find_step(true),
            A::FindPrev => self.find_step(false),

            // -- tabs: single-tab Phase 2 ---------------------------
            A::TabNext | A::TabPrev | A::TabClose | A::TabNew => {
                tracing::info!("tab action — multi-tab is Phase 5; single-tab buffr ignores this");
            }

            // -- chrome overlays: Phase 3 ---------------------------
            A::OpenOmnibar | A::OpenCommandLine => {
                tracing::info!("UI action — overlay rendering is Phase 3 chrome work");
            }
            A::EnterHintMode => self.enter_hint_mode(false),
            A::EnterHintModeBackground => self.enter_hint_mode(true),

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

            // -- downloads: Phase 5 ---------------------------------
            A::ClearCompletedDownloads => match self.downloads.clear_completed() {
                Ok(n) => tracing::info!(removed = n, "downloads: cleared completed"),
                Err(err) => tracing::warn!(error = %err, "downloads: clear_completed failed"),
            },

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

    /// Status snapshot of the active hint session. `None` outside of
    /// hint mode. UI threads pull this each tick to refresh the
    /// statusline indicator.
    pub fn hint_status(&self) -> Option<HintStatus> {
        let guard = self.hint_session.lock().ok()?;
        let s = guard.as_ref()?;
        Some(HintStatus {
            typed: s.typed.clone(),
            match_count: s.match_count(),
            background: s.background,
        })
    }

    /// Whether a hint session is currently active.
    pub fn is_hint_mode(&self) -> bool {
        self.hint_session.lock().ok().is_some_and(|g| g.is_some())
    }

    /// Inject `hint.js` into the active main frame. Generates labels
    /// for the configured alphabet sized to a generous 256 — the JS
    /// truncates to the actual visible-element count.
    ///
    /// Construction of the in-memory [`HintSession`] happens lazily
    /// when the renderer reports back via [`HintConsoleEvent::Ready`]
    /// — see [`Self::pump_hint_events`].
    ///
    /// `background = true` toggles the F-key variant; the host
    /// currently logs a warning on commit and falls back to a same-tab
    /// click (multi-tab is Phase 5).
    pub fn enter_hint_mode(&self, background: bool) {
        // Reserve a generous label budget. The JS asset truncates to
        // the actual visible-element count, so over-allocating here
        // costs only a small string-array allocation in JS.
        const LABEL_BUDGET: usize = 256;
        let labels = self.hint_alphabet.labels_for(LABEL_BUDGET);
        let alphabet_str = self.hint_alphabet.as_string();
        let script = build_inject_script(&alphabet_str, &labels, DEFAULT_HINT_SELECTORS);

        // Stash a placeholder session so the UI can render
        // "HINT (waiting)" until the renderer reports back.
        if let Ok(mut slot) = self.hint_session.lock() {
            *slot = Some(HintSession::new(
                self.hint_alphabet.clone(),
                Vec::new(),
                background,
            ));
        }

        let Some(frame) = self.browser.main_frame() else {
            warn!("enter_hint_mode: main frame unavailable");
            self.cancel_hint();
            return;
        };
        let url = CefStringUtf16::from(&frame.url()).to_string();
        let cef_script = CefString::from(script.as_str());
        let cef_url = CefString::from(url.as_str());
        // `start_line = 1`: cef-rs forwards this to the V8 source-map
        // line offset; only matters for stack traces. Use 1 so traces
        // line up with the asset's first line.
        frame.execute_java_script(Some(&cef_script), Some(&cef_url), 1);
        info!(
            background,
            label_budget = LABEL_BUDGET,
            "hint mode: injected"
        );
    }

    /// Drain any renderer-side hint events the display handler has
    /// posted since the last tick and finalise the active session.
    /// Returns `true` if the session changed (so callers can request
    /// a redraw).
    pub fn pump_hint_events(&self) -> bool {
        let Some(event) = crate::hint::take_hint_event(&self.hint_sink) else {
            return false;
        };
        match event {
            crate::hint::HintConsoleEvent::Ready { hints, alphabet: _ } => {
                if let Ok(mut slot) = self.hint_session.lock()
                    && let Some(existing) = slot.as_mut()
                {
                    let background = existing.background;
                    *existing = HintSession::new(self.hint_alphabet.clone(), hints, background);
                }
                true
            }
            crate::hint::HintConsoleEvent::Error { message } => {
                warn!(message, "hint mode: renderer reported error");
                self.cancel_hint();
                true
            }
        }
    }

    /// Feed a printable character to the active hint session. Returns
    /// `Some(action)` whose semantics match [`HintAction`]; the caller
    /// is expected to act on `Click` / `OpenInBackground` (run the
    /// commit JS) and clear the session on `Cancel`. Returns `None`
    /// when no session is active.
    pub fn feed_hint_key(&self, ch: char) -> Option<HintAction> {
        let mut slot = self.hint_session.lock().ok()?;
        let session = slot.as_mut()?;
        let action = session.feed(ch);
        // Always tell the JS overlay to filter, even on Cancel — a
        // Cancel removes overlays via `__buffrHintCancel`, but
        // intermediate Filter ticks need an immediate visual update.
        let typed = session.typed.clone();
        drop(slot);
        match &action {
            HintAction::Filter => self.run_hint_js(&format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                json_string_literal(&typed)
            )),
            HintAction::Click(id) | HintAction::OpenInBackground(id) => {
                if matches!(action, HintAction::OpenInBackground(_)) {
                    tracing::warn!(
                        element_id = *id,
                        "hint background commit: multi-tab not implemented; falling back to same-tab click"
                    );
                }
                self.run_hint_js(&format!(
                    "if (window.__buffrHintCommit) window.__buffrHintCommit({id})"
                ));
                self.clear_hint_session();
            }
            HintAction::Cancel => {
                self.cancel_hint();
            }
        }
        Some(action)
    }

    /// Backspace the typed buffer. Mirrors [`Self::feed_hint_key`]
    /// but pops a char from the session and re-issues a filter call.
    pub fn backspace_hint(&self) -> Option<HintAction> {
        let mut slot = self.hint_session.lock().ok()?;
        let session = slot.as_mut()?;
        let action = session.backspace();
        let typed = session.typed.clone();
        drop(slot);
        match &action {
            HintAction::Filter => self.run_hint_js(&format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                json_string_literal(&typed)
            )),
            HintAction::Cancel => self.cancel_hint(),
            // `backspace` only emits Filter / Cancel — defensive.
            _ => {}
        }
        Some(action)
    }

    /// Cancel the active hint session — invokes the JS cleanup hook
    /// and drops the session state. Idempotent.
    pub fn cancel_hint(&self) {
        self.run_hint_js("if (window.__buffrHintCancel) window.__buffrHintCancel()");
        self.clear_hint_session();
    }

    fn clear_hint_session(&self) {
        if let Ok(mut slot) = self.hint_session.lock() {
            *slot = None;
        }
    }

    /// Helper: run a hint-related JS snippet against the main frame.
    /// `script_url` is set to `buffr://hint` so DevTools attribution
    /// matches the source.
    fn run_hint_js(&self, code: &str) {
        let Some(frame) = self.browser.main_frame() else {
            return;
        };
        let cef_code = CefString::from(code);
        let cef_url = CefString::from("buffr://hint");
        frame.execute_java_script(Some(&cef_code), Some(&cef_url), 1);
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
        let domain = self.current_domain();
        if let Err(err) = self.zoom.set(&domain, new_level) {
            warn!(error = %err, %domain, "zoom: persist failed");
        }
    }

    /// `ZoomReset`: clear both the live zoom and the persisted row so
    /// the domain reverts to CEF's default on next load.
    fn reset_zoom(&self) {
        let Some(host) = self.browser.host() else {
            warn!("reset_zoom: browser.host() returned None");
            return;
        };
        host.set_zoom_level(0.0);
        let domain = self.current_domain();
        if let Err(err) = self.zoom.remove(&domain) {
            warn!(error = %err, %domain, "zoom: remove failed");
        }
    }

    /// Extract the current main-frame URL's zoom-key. Returns the
    /// global sentinel when the frame is unavailable so `set` /
    /// `remove` still target a well-known row.
    fn current_domain(&self) -> String {
        match self.browser.main_frame() {
            Some(frame) => {
                let url = CefStringUtf16::from(&frame.url()).to_string();
                buffr_zoom::domain_for_url(&url)
            }
            None => buffr_zoom::GLOBAL_KEY.to_string(),
        }
    }
}

/// Snapshot of hint mode state for the statusline indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintStatus {
    pub typed: String,
    pub match_count: usize,
    pub background: bool,
}

/// Format a string as a JS double-quoted literal, escaping every
/// non-ASCII codepoint to `\uXXXX`. Used for the inline filter call so
/// the splice survives any input the user might type.
fn json_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf).iter() {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// Trivial unused-import suppressor: `Hint` is referenced via the
/// `crate::hint::Hint` re-export from `enter_hint_mode`'s docs but the
/// binding is otherwise local.
#[allow(dead_code)]
fn _hint_used(_: Hint) {}

/// Pixels per scroll-unit. `ScrollDown(3)` therefore moves 120px,
/// matching a typical "tap j three times" feel without making each
/// `j` feel laggy. Half/full-page scrolls go through their own
/// `window.innerHeight`-relative path so they're DPI-independent.
const STEP_PX: i64 = 40;
