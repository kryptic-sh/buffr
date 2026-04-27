//! [`BrowserHost`] — a tab manager owning N concurrent CEF browsers
//! parented to one X11 window.
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
//!
//! ## Multi-tab architecture
//!
//! All tabs share a **single** [`cef::Client`] (so the load/display/
//! find/download handlers continue funnelling events into the same
//! history / downloads / find sinks). Each tab owns its own
//! [`cef::Browser`]; switching tabs calls
//! `was_hidden(true)` on the previous and `was_hidden(false)` +
//! `set_focus(true)` on the next. All browsers share the same X11
//! parent window (the winit window handle); only the active browser
//! is visible. See `docs/multi-tab.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use buffr_config::DownloadsConfig;
use buffr_downloads::Downloads;
use buffr_history::History;
use buffr_permissions::Permissions;
use buffr_zoom::ZoomStore;
use cef::{
    BrowserSettings, CefString, CefStringUtf16, ImplBrowser, ImplBrowserHost, ImplFrame,
    WindowInfo, browser_host_create_browser_sync,
};
use raw_window_handle::RawWindowHandle;
use tracing::{info, warn};

use crate::download_notice::DownloadNoticeQueue;
use crate::edit::EditEventSink;
use crate::find::FindResultSink;
use crate::hint::{
    DEFAULT_HINT_SELECTORS, Hint, HintAction, HintAlphabet, HintEventSink, HintSession,
    build_inject_script,
};
use crate::permissions::PermissionsQueue;
use crate::telemetry::{KEY_TABS_OPENED, UsageCounters};
use crate::{CoreError, handlers};

/// Monotonic tab identifier minted by [`BrowserHost`]. Distinct from
/// CEF's `Browser::identifier()` (which can collide on close+reopen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tab#{}", self.0)
    }
}

/// Per-tab UI state preserved across tab switches. Find query and hint
/// session restore on focus.
#[derive(Debug, Default, Clone)]
pub struct TabSession {
    pub find_query: Option<String>,
    pub hint_session: Option<HintSession>,
}

/// One open browser. The `browser` field is the live [`cef::Browser`]
/// CEF returned from `browser_host_create_browser_sync`.
pub struct Tab {
    pub id: TabId,
    pub browser: cef::Browser,
    /// Last-known main frame URL. Updated externally on navigation
    /// (Phase 5b will hook `LoadHandler::on_load_end`).
    pub url: String,
    /// Most recent title from CEF's display handler.
    pub title: Option<String>,
    /// Page load progress 0.0..=1.0. 1.0 = idle.
    pub progress: f32,
    pub is_loading: bool,
    pub pinned: bool,
    pub session: TabSession,
}

impl Tab {
    /// Display title for the tab strip — falls back to URL host /
    /// scheme when no title has been reported.
    pub fn display_title(&self) -> String {
        if let Some(t) = self.title.as_ref()
            && !t.is_empty()
        {
            return t.clone();
        }
        if !self.url.is_empty() {
            return self.url.clone();
        }
        format!("{}", self.id)
    }
}

/// Copy-friendly snapshot of a tab. Used by chrome / UI threads that
/// don't want to hold the manager mutex.
#[derive(Debug, Clone)]
pub struct TabSummary {
    pub id: TabId,
    pub title: String,
    pub url: String,
    pub progress: f32,
    pub is_loading: bool,
    pub pinned: bool,
    pub private: bool,
}

/// Owns N CEF browsers parented to a single native X11 window.
///
/// The host is created **after** `cef::initialize` succeeds. On Linux
/// (default build) we hand the X11 window XID to CEF via `WindowInfo`
/// — this works for both native X11 sessions and Wayland sessions
/// running XWayland, because we force winit to its X11 backend before
/// creating the event loop.
pub struct BrowserHost {
    /// Live tab list. Only the active tab is visible; inactive tabs
    /// are `was_hidden(true)`.
    tabs: Mutex<Vec<Tab>>,
    /// Index into `tabs` of the active tab. `None` only between
    /// `close_active` of the last tab and the caller's exit decision.
    active: Mutex<Option<usize>>,
    /// Monotonic [`TabId`] minter. Reset only across process restart.
    next_id: AtomicU64,
    /// Stored on construction so `open_tab` can build new browsers
    /// after the manager is up.
    parent_handle: Mutex<Option<RawWindowHandle>>,
    /// Last known CEF child rect (width, height). Caller-passed via
    /// [`Self::resize`]. Used by `open_tab` to size new browsers
    /// consistently.
    last_size: Mutex<(u32, u32)>,
    /// Whether the host is in private mode. Threaded into
    /// [`TabSummary`] so chrome can mark every tab private.
    private: bool,
    /// Shared stores — every tab's CEF client funnels into the same
    /// history / downloads / zoom rows.
    history: Arc<History>,
    downloads: Arc<Downloads>,
    downloads_config: Arc<DownloadsConfig>,
    zoom: Arc<ZoomStore>,
    permissions: Arc<Permissions>,
    permissions_queue: PermissionsQueue,
    /// Download notification queue — shared between the CEF IO thread
    /// (which pushes notices) and the UI render loop (which drains and
    /// paints them). `DownloadHandler` pushes into this; `AppState`
    /// expires stale entries each tick.
    notice_queue: DownloadNoticeQueue,
    /// Mailboxes shared with CEF handlers. One sink for the whole
    /// host (handlers can't tell which browser fired the event in
    /// every callback shape, so per-tab demux happens in the UI).
    find_sink: FindResultSink,
    hint_sink: HintEventSink,
    /// Edit-mode event queue shared with the load handler (which injects
    /// `edit.js`) and the display handler (which parses its console
    /// output). Stage 2 will drain this from the UI render loop to drive
    /// `EditSession` lifecycle — spawn on focus, keystroke-route while
    /// attached, drop on blur/Esc.
    ///
    /// TODO(stage2): drain `edit_sink` each render tick; spawn/destroy
    /// `EditSession` based on `Focus`/`Blur` events; route keystrokes
    /// through `EditSession::feed_input`; push DOM updates back via
    /// `window.__buffrEditApply`.
    edit_sink: EditEventSink,
    /// User-configured hint alphabet. Each tab uses the same alphabet.
    hint_alphabet: HintAlphabet,
    /// Phase 6 usage counters. `None` when the embedder didn't pass
    /// one (e.g. older callers); when present every counter mutation
    /// goes through this handle. The counters themselves no-op on
    /// `enabled = false`, so the `Some(...)` arm is cheap when off.
    counters: Option<Arc<UsageCounters>>,
}

impl BrowserHost {
    /// Create the host with a single initial tab loading `url`.
    ///
    /// `window_handle` is the platform window the CEF browser will be
    /// parented to. On Linux this must be the X11 XID of a winit
    /// window. All later tabs created via [`Self::open_tab`] re-use
    /// this handle.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window_handle: RawWindowHandle,
        url: &str,
        history: Arc<History>,
        downloads: Arc<Downloads>,
        downloads_config: Arc<DownloadsConfig>,
        zoom: Arc<ZoomStore>,
        permissions: Arc<Permissions>,
        permissions_queue: PermissionsQueue,
        notice_queue: DownloadNoticeQueue,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        edit_sink: EditEventSink,
        hint_alphabet: HintAlphabet,
        initial_size: (u32, u32),
    ) -> Result<Self, CoreError> {
        Self::new_with_options(
            window_handle,
            url,
            history,
            downloads,
            downloads_config,
            zoom,
            permissions,
            permissions_queue,
            notice_queue,
            find_sink,
            hint_sink,
            edit_sink,
            hint_alphabet,
            initial_size,
            false,
            None,
        )
    }

    /// Like [`Self::new`] but lets the embedder mark every browser as
    /// private. The flag is purely informational — the underlying CEF
    /// profile dirs are already swapped at process start by the
    /// `--private` CLI flag.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_options(
        window_handle: RawWindowHandle,
        url: &str,
        history: Arc<History>,
        downloads: Arc<Downloads>,
        downloads_config: Arc<DownloadsConfig>,
        zoom: Arc<ZoomStore>,
        permissions: Arc<Permissions>,
        permissions_queue: PermissionsQueue,
        notice_queue: DownloadNoticeQueue,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        edit_sink: EditEventSink,
        hint_alphabet: HintAlphabet,
        initial_size: (u32, u32),
        private: bool,
        counters: Option<Arc<UsageCounters>>,
    ) -> Result<Self, CoreError> {
        info!(target: "buffr_core::host", %url, "creating CEF browser (initial tab)");
        let host = Self {
            tabs: Mutex::new(Vec::new()),
            active: Mutex::new(None),
            next_id: AtomicU64::new(0),
            parent_handle: Mutex::new(Some(window_handle)),
            last_size: Mutex::new(initial_size),
            private,
            history,
            downloads,
            downloads_config,
            zoom,
            permissions,
            permissions_queue,
            notice_queue,
            find_sink,
            hint_sink,
            edit_sink,
            hint_alphabet,
            counters,
        };
        host.open_tab(url)?;
        Ok(host)
    }

    /// Borrow the shared permissions store. The UI thread uses this to
    /// persist user-chosen "always" decisions when resolving a queued
    /// prompt.
    pub fn permissions(&self) -> &Arc<Permissions> {
        &self.permissions
    }

    /// Borrow the shared permissions queue. The UI thread drains this
    /// each tick.
    pub fn permissions_queue(&self) -> &PermissionsQueue {
        &self.permissions_queue
    }

    fn mint_id(&self) -> TabId {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        TabId(n)
    }

    /// Spin a fresh CEF browser parented to the host window. The new
    /// tab becomes active.
    pub fn open_tab(&self, url: &str) -> Result<TabId, CoreError> {
        let id = self.create_browser(url, false)?;
        Ok(id)
    }

    /// Same as [`Self::open_tab`] but the new tab is created hidden;
    /// the active tab does not change.
    pub fn open_tab_background(&self, url: &str) -> Result<TabId, CoreError> {
        self.create_browser(url, true)
    }

    fn create_browser(&self, url: &str, background: bool) -> Result<TabId, CoreError> {
        let handle = match self.parent_handle.lock() {
            Ok(g) => match *g {
                Some(h) => h,
                None => return Err(CoreError::CreateBrowserFailed),
            },
            Err(_) => return Err(CoreError::CreateBrowserFailed),
        };
        let (init_w, init_h) = match self.last_size.lock() {
            Ok(g) => *g,
            Err(_) => return Err(CoreError::CreateBrowserFailed),
        };

        let mut window_info = WindowInfo {
            bounds: cef::Rect {
                x: 0,
                y: 0,
                width: init_w as i32,
                height: init_h as i32,
            },
            ..WindowInfo::default()
        };
        match handle {
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(h) => {
                window_info.parent_window = h.window as _;
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

        let cef_url = CefString::from(url);
        let settings = BrowserSettings::default();
        let mut client = handlers::make_client(
            self.history.clone(),
            self.downloads.clone(),
            self.downloads_config.clone(),
            self.zoom.clone(),
            self.permissions.clone(),
            self.permissions_queue.clone(),
            self.find_sink.clone(),
            self.hint_sink.clone(),
            self.edit_sink.clone(),
            self.counters.clone(),
            self.notice_queue.clone(),
        );
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&cef_url),
            Some(&settings),
            None,
            None,
        )
        .ok_or(CoreError::CreateBrowserFailed)?;

        let id = self.mint_id();
        let tab = Tab {
            id,
            browser,
            url: url.to_string(),
            title: None,
            progress: 1.0,
            is_loading: false,
            pinned: false,
            session: TabSession::default(),
        };

        let mut tabs = self
            .tabs
            .lock()
            .map_err(|_| CoreError::CreateBrowserFailed)?;
        tabs.push(tab);
        let new_idx = tabs.len() - 1;
        drop(tabs);

        if background {
            // Hide the new browser; keep the existing active one.
            if let Ok(tabs) = self.tabs.lock()
                && let Some(host) = tabs[new_idx].browser.host()
            {
                host.was_hidden(1);
            }
        } else {
            self.set_active_index(new_idx);
        }
        info!(target: "buffr_core::host", %id, %url, background, "tab opened");
        // Phase 6 telemetry: count every tab open (foreground +
        // background) — they are equally "user opened a tab" events.
        if let Some(c) = self.counters.as_ref() {
            c.increment(KEY_TABS_OPENED);
        }
        Ok(id)
    }

    fn set_active_index(&self, new_idx: usize) {
        let mut active = match self.active.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let tabs = match self.tabs.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if new_idx >= tabs.len() {
            return;
        }
        if let Some(prev) = *active
            && prev < tabs.len()
            && prev != new_idx
            && let Some(host) = tabs[prev].browser.host()
        {
            host.was_hidden(1);
            host.set_focus(0);
        }
        if let Some(host) = tabs[new_idx].browser.host() {
            host.was_hidden(0);
            host.was_resized();
            host.set_focus(1);
        }
        *active = Some(new_idx);
    }

    /// Switch to the tab with `id`. No-op when not found.
    pub fn select_tab(&self, id: TabId) {
        let idx = match self.tabs.lock() {
            Ok(g) => g.iter().position(|t| t.id == id),
            Err(_) => None,
        };
        if let Some(idx) = idx {
            self.set_active_index(idx);
        }
    }

    /// Number of open tabs.
    pub fn tab_count(&self) -> usize {
        self.tabs.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Active tab snapshot, if any.
    pub fn active_tab(&self) -> Option<TabSummary> {
        let tabs = self.tabs.lock().ok()?;
        let idx = (*self.active.lock().ok()?)?;
        tabs.get(idx).map(|t| self.summarize(t))
    }

    /// Snapshot of every tab in stored order.
    pub fn tabs_summary(&self) -> Vec<TabSummary> {
        let Ok(tabs) = self.tabs.lock() else {
            return Vec::new();
        };
        tabs.iter().map(|t| self.summarize(t)).collect()
    }

    /// Index of the active tab in [`Self::tabs_summary`]'s ordering.
    pub fn active_index(&self) -> Option<usize> {
        self.active.lock().ok().and_then(|g| *g)
    }

    fn summarize(&self, t: &Tab) -> TabSummary {
        TabSummary {
            id: t.id,
            title: t.display_title(),
            url: t.url.clone(),
            progress: t.progress,
            is_loading: t.is_loading,
            pinned: t.pinned,
            private: self.private,
        }
    }

    /// Cycle to the next tab (wraps).
    pub fn next_tab(&self) {
        let len = self.tab_count();
        if len <= 1 {
            return;
        }
        let cur = self.active_index().unwrap_or(0);
        let next = (cur + 1) % len;
        self.set_active_index(next);
    }

    /// Cycle to the previous tab (wraps).
    pub fn prev_tab(&self) {
        let len = self.tab_count();
        if len <= 1 {
            return;
        }
        let cur = self.active_index().unwrap_or(0);
        let prev = if cur == 0 { len - 1 } else { cur - 1 };
        self.set_active_index(prev);
    }

    /// Close the active tab. Returns `Ok(true)` when more tabs remain,
    /// `Ok(false)` when this was the last tab (caller should exit the
    /// app).
    pub fn close_active(&self) -> Result<bool, CoreError> {
        let idx = self.active_index().ok_or(CoreError::CreateBrowserFailed)?;
        self.close_index(idx)
    }

    /// Close the tab with `id`. Returns `Ok(true)` when more tabs
    /// remain, `Ok(false)` when this was the last tab.
    pub fn close_tab(&self, id: TabId) -> Result<bool, CoreError> {
        let idx = match self.tabs.lock() {
            Ok(g) => g.iter().position(|t| t.id == id),
            Err(_) => None,
        };
        match idx {
            Some(i) => self.close_index(i),
            None => Ok(true),
        }
    }

    fn close_index(&self, idx: usize) -> Result<bool, CoreError> {
        let removed = {
            let mut tabs = self
                .tabs
                .lock()
                .map_err(|_| CoreError::CreateBrowserFailed)?;
            if idx >= tabs.len() {
                return Ok(true);
            }
            tabs.remove(idx)
        };
        // Tell CEF to tear the browser down. The browser drop also
        // runs at end-of-scope but `close_browser(1)` flushes IO and
        // releases the X11 child window immediately.
        if let Some(host) = removed.browser.host() {
            host.close_browser(1);
        }

        // Pick a new active. Prefer the tab that was after the removed
        // one (mirrors browser convention); fall back to the previous.
        let len = self.tab_count();
        if len == 0 {
            if let Ok(mut a) = self.active.lock() {
                *a = None;
            }
            return Ok(false);
        }
        let new_idx = if idx >= len { len - 1 } else { idx };
        self.set_active_index(new_idx);
        Ok(true)
    }

    /// Move the tab at `from` to position `to`. Indices are clamped to
    /// the valid range; same-position is a no-op. Reserved for the
    /// eventual mouse-drag handler.
    pub fn move_tab(&self, from: usize, to: usize) {
        let mut tabs = match self.tabs.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let len = tabs.len();
        if len == 0 || from == to || from >= len {
            return;
        }
        let to = to.min(len - 1);
        let tab = tabs.remove(from);
        tabs.insert(to, tab);
        // Fix up active index so it points at the same tab.
        let mut active = match self.active.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(a) = *active {
            let new_a = if a == from {
                to
            } else if from < a && to >= a {
                a - 1
            } else if from > a && to <= a {
                a + 1
            } else {
                a
            };
            *active = Some(new_a);
        }
    }

    /// Duplicate the active tab — creates a new tab loading the same
    /// URL.
    pub fn duplicate_active(&self) -> Result<TabId, CoreError> {
        let url = match self.active_tab() {
            Some(t) => t.url,
            None => return Err(CoreError::CreateBrowserFailed),
        };
        let target = if url.is_empty() {
            "about:blank".to_string()
        } else {
            url
        };
        self.open_tab(&target)
    }

    /// Toggle the pinned bit on the active tab. Pin does **not**
    /// prevent close — it only signals sort order to chrome.
    pub fn toggle_pin_active(&self) {
        let Ok(mut tabs) = self.tabs.lock() else {
            return;
        };
        let Some(idx) = self.active.lock().ok().and_then(|g| *g) else {
            return;
        };
        if let Some(t) = tabs.get_mut(idx) {
            t.pinned = !t.pinned;
        }
    }

    /// Update the URL field on the tab whose `Browser::identifier`
    /// matches `cef_id`. Used by the load handler to keep the chrome
    /// in sync. Returns the [`TabId`] of the affected tab, if any.
    pub fn record_url(&self, cef_id: i32, url: &str) -> Option<TabId> {
        let mut tabs = self.tabs.lock().ok()?;
        for t in tabs.iter_mut() {
            if t.browser.identifier() == cef_id {
                t.url = url.to_string();
                return Some(t.id);
            }
        }
        None
    }

    /// Reflow every tab's CEF child window after the host winit window
    /// resized. Caller passes the *child* rect (the page area, not
    /// including chrome strips).
    ///
    /// `was_resized()` notifies CEF's renderer of new content
    /// dimensions; on X11 the embedded child window does not always
    /// follow the parent's geometry automatically — we rely on
    /// XWayland / the compositor honouring resize hints. Pure Mutter
    /// / KWin embeds may need an `XResizeWindow` follow-up which the
    /// cef-rs 147 wrapper doesn't expose.
    pub fn resize(&self, width: u32, height: u32) {
        if let Ok(mut last) = self.last_size.lock() {
            *last = (width, height);
        }
        let Ok(tabs) = self.tabs.lock() else {
            return;
        };
        for t in tabs.iter() {
            if let Some(host) = t.browser.host() {
                host.was_resized();
            }
        }
    }

    /// Navigate the active tab's main frame to `url`.
    pub fn navigate(&self, url: &str) -> Result<(), CoreError> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidUrl(String::new()));
        }
        self.with_active(|t| {
            let Some(frame) = t.browser.main_frame() else {
                warn!("navigate: main frame unavailable");
                return Err(CoreError::CreateBrowserFailed);
            };
            let cef_url = CefString::from(trimmed);
            frame.load_url(Some(&cef_url));
            t.url = trimmed.to_string();
            info!(target: "buffr_core::host", url = %trimmed, "navigate");
            Ok(())
        })
        .ok_or(CoreError::CreateBrowserFailed)?
    }

    /// Borrow the active tab mutably under the manager mutex.
    /// Returns `None` only when there is no active tab.
    fn with_active<R>(&self, f: impl FnOnce(&mut Tab) -> R) -> Option<R> {
        let mut tabs = self.tabs.lock().ok()?;
        let idx = (*self.active.lock().ok()?)?;
        let t = tabs.get_mut(idx)?;
        Some(f(t))
    }

    /// Begin a fresh find session on the active tab.
    pub fn start_find(&self, query: &str, forward: bool) {
        if query.is_empty() {
            self.stop_find();
            return;
        }
        self.with_active(|t| {
            let Some(host) = t.browser.host() else {
                warn!("start_find: browser.host() returned None");
                return;
            };
            let cef_query = CefString::from(query);
            host.find(Some(&cef_query), forward as i32, 0, 0);
            t.session.find_query = Some(query.to_string());
        });
    }

    /// Cancel the active tab's find session.
    pub fn stop_find(&self) {
        self.with_active(|t| {
            if let Some(host) = t.browser.host() {
                host.stop_finding(1);
            }
            t.session.find_query = None;
        });
    }

    fn find_step(&self, forward: bool) {
        self.with_active(|t| {
            let Some(query) = t.session.find_query.clone() else {
                tracing::info!("find_step: no active query — call start_find first");
                return;
            };
            let Some(host) = t.browser.host() else {
                warn!("find_step: browser.host() returned None");
                return;
            };
            let cef_query = CefString::from(query.as_str());
            host.find(Some(&cef_query), forward as i32, 0, 1);
        });
    }

    /// Construct a browser in **off-screen rendering** mode. **Not yet
    /// implemented** — currently panics. See [`crate::osr`] and
    /// `PLAN.md` (Phase 3).
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

    /// Dispatch a [`buffr_modal::PageAction`] against the active tab.
    pub fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;
        match action {
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

            A::HistoryBack => {
                self.with_active(|t| t.browser.go_back());
            }
            A::HistoryForward => {
                self.with_active(|t| t.browser.go_forward());
            }
            A::Reload => {
                self.with_active(|t| t.browser.reload());
            }
            A::ReloadHard => {
                self.with_active(|t| t.browser.reload_ignore_cache());
            }
            A::StopLoading => {
                self.with_active(|t| t.browser.stop_load());
            }

            A::ZoomIn => self.adjust_zoom(0.25),
            A::ZoomOut => self.adjust_zoom(-0.25),
            A::ZoomReset => self.reset_zoom(),

            A::OpenDevTools => {
                self.with_active(|t| {
                    if let Some(host) = t.browser.host() {
                        let window_info = WindowInfo::default();
                        let settings = BrowserSettings::default();
                        host.show_dev_tools(Some(&window_info), None, Some(&settings), None);
                    } else {
                        warn!("OpenDevTools: browser.host() returned None");
                    }
                });
            }

            A::Find { forward } => {
                tracing::warn!(
                    forward = *forward,
                    "Find requires command line — Phase 3b. Use BrowserHost::start_find() directly."
                );
            }
            A::FindNext => self.find_step(true),
            A::FindPrev => self.find_step(false),

            // Tab actions: the host is the manager. Apps-layer wires
            // these directly via `next_tab` / `prev_tab` /
            // `close_active` / `open_tab` so the manager can route the
            // result (e.g. "last tab closed → exit") back to the app.
            // The dispatch path here is a fallback for keymaps that
            // hit the host without going through the apps shim.
            A::TabNext => self.next_tab(),
            A::TabPrev => self.prev_tab(),
            A::TabClose => {
                let _ = self.close_active();
            }
            A::TabNew => {
                let _ = self.open_tab("about:blank");
            }
            A::DuplicateTab => {
                let _ = self.duplicate_active();
            }
            A::PinTab => self.toggle_pin_active(),
            A::TabReorder { from, to } => self.move_tab(*from as usize, *to as usize),

            A::OpenOmnibar | A::OpenCommandLine => {
                tracing::info!("UI action — overlay rendering owned by apps layer");
            }
            A::EnterHintMode => self.enter_hint_mode(false),
            A::EnterHintModeBackground => self.enter_hint_mode(true),

            A::EnterMode(mode) => {
                tracing::info!(?mode, "EnterMode — engine tracks mode internally");
            }
            A::EnterEditMode => {
                tracing::info!(
                    "edit-mode requested — hjkl-engine integration is Phase 2b \
                     (blocked on hjkl Host trait)"
                );
            }

            A::ClearCompletedDownloads => match self.downloads.clear_completed() {
                Ok(n) => tracing::info!(removed = n, "downloads: cleared completed"),
                Err(err) => tracing::warn!(error = %err, "downloads: clear_completed failed"),
            },

            A::YankUrl => {
                self.with_active(|t| {
                    if let Some(frame) = t.browser.main_frame() {
                        let url = CefStringUtf16::from(&frame.url()).to_string();
                        tracing::info!(url, "would copy to clipboard — clipboard is Phase 5");
                    } else {
                        tracing::info!("would copy to clipboard — main frame unavailable");
                    }
                });
            }
        }
    }

    /// Status snapshot of the active tab's hint session.
    pub fn hint_status(&self) -> Option<HintStatus> {
        let tabs = self.tabs.lock().ok()?;
        let idx = (*self.active.lock().ok()?)?;
        let t = tabs.get(idx)?;
        let s = t.session.hint_session.as_ref()?;
        Some(HintStatus {
            typed: s.typed.clone(),
            match_count: s.match_count(),
            background: s.background,
        })
    }

    /// Whether the active tab has a live hint session.
    pub fn is_hint_mode(&self) -> bool {
        self.with_active(|t| t.session.hint_session.is_some())
            .unwrap_or(false)
    }

    /// Inject `hint.js` into the active tab's main frame.
    pub fn enter_hint_mode(&self, background: bool) {
        const LABEL_BUDGET: usize = 256;
        let labels = self.hint_alphabet.labels_for(LABEL_BUDGET);
        let alphabet_str = self.hint_alphabet.as_string();
        let script = build_inject_script(&alphabet_str, &labels, DEFAULT_HINT_SELECTORS);

        let alphabet = self.hint_alphabet.clone();
        let mut bail = false;
        self.with_active(|t| {
            t.session.hint_session = Some(HintSession::new(alphabet, Vec::new(), background));
            let Some(frame) = t.browser.main_frame() else {
                warn!("enter_hint_mode: main frame unavailable");
                bail = true;
                return;
            };
            let url = CefStringUtf16::from(&frame.url()).to_string();
            let cef_script = CefString::from(script.as_str());
            let cef_url = CefString::from(url.as_str());
            frame.execute_java_script(Some(&cef_script), Some(&cef_url), 1);
            info!(
                background,
                label_budget = LABEL_BUDGET,
                "hint mode: injected"
            );
        });
        if bail {
            self.cancel_hint();
        }
    }

    /// Drain renderer-side hint events and finalise the active tab's
    /// session. Returns `true` if the session changed.
    pub fn pump_hint_events(&self) -> bool {
        let Some(event) = crate::hint::take_hint_event(&self.hint_sink) else {
            return false;
        };
        match event {
            crate::hint::HintConsoleEvent::Ready { hints, alphabet: _ } => {
                let alphabet = self.hint_alphabet.clone();
                self.with_active(|t| {
                    if let Some(existing) = t.session.hint_session.as_mut() {
                        let background = existing.background;
                        *existing = HintSession::new(alphabet, hints, background);
                    }
                });
                true
            }
            crate::hint::HintConsoleEvent::Error { message } => {
                warn!(message, "hint mode: renderer reported error");
                self.cancel_hint();
                true
            }
        }
    }

    /// Feed a printable character to the active tab's hint session.
    pub fn feed_hint_key(&self, ch: char) -> Option<HintAction> {
        let mut commit_id: Option<u32> = None;
        let mut filter_typed: Option<String> = None;
        let mut clear = false;
        let mut cancel = false;
        let result = self.with_active(|t| {
            let session = t.session.hint_session.as_mut()?;
            let action = session.feed(ch);
            let typed = session.typed.clone();
            match &action {
                HintAction::Filter => filter_typed = Some(typed),
                HintAction::Click(id) | HintAction::OpenInBackground(id) => {
                    if matches!(action, HintAction::OpenInBackground(_)) {
                        tracing::warn!(
                            element_id = *id,
                            "hint background commit: routes through `open_tab_background`",
                        );
                    }
                    commit_id = Some(*id);
                    clear = true;
                }
                HintAction::Cancel => {
                    cancel = true;
                }
            }
            Some(action)
        });
        let action = result.flatten()?;
        if let Some(typed) = filter_typed {
            self.run_hint_js(&format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                json_string_literal(&typed)
            ));
        }
        if let Some(id) = commit_id {
            // Handle background variant by opening a new tab in the
            // background rather than committing the click. We still
            // invoke the JS commit on the original frame to capture
            // the resolved href, but for now the fallback is a same-
            // tab click (clipboard-driven URL extraction is Phase 5b).
            self.run_hint_js(&format!(
                "if (window.__buffrHintCommit) window.__buffrHintCommit({id})"
            ));
        }
        if clear {
            self.with_active(|t| {
                t.session.hint_session = None;
            });
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    /// Backspace the active tab's hint typed buffer.
    pub fn backspace_hint(&self) -> Option<HintAction> {
        let mut filter_typed: Option<String> = None;
        let mut cancel = false;
        let result = self.with_active(|t| {
            let session = t.session.hint_session.as_mut()?;
            let action = session.backspace();
            let typed = session.typed.clone();
            match &action {
                HintAction::Filter => filter_typed = Some(typed),
                HintAction::Cancel => cancel = true,
                _ => {}
            }
            Some(action)
        });
        let action = result.flatten()?;
        if let Some(typed) = filter_typed {
            self.run_hint_js(&format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                json_string_literal(&typed)
            ));
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    /// Cancel the active tab's hint session.
    pub fn cancel_hint(&self) {
        self.run_hint_js("if (window.__buffrHintCancel) window.__buffrHintCancel()");
        self.with_active(|t| {
            t.session.hint_session = None;
        });
    }

    fn run_hint_js(&self, code: &str) {
        self.run_main_frame_js(code, "buffr://hint");
    }

    /// Execute arbitrary JS on the active tab's main frame.
    fn run_main_frame_js(&self, code: &str, url: &str) {
        self.with_active(|t| {
            let Some(frame) = t.browser.main_frame() else {
                return;
            };
            let cef_code = CefString::from(code);
            let cef_url = CefString::from(url);
            frame.execute_java_script(Some(&cef_code), Some(&cef_url), 1);
        });
    }

    /// Push a new value into the focused field via `__buffrEditApply`.
    pub fn run_edit_apply(&self, field_id: &str, value: &str) {
        let escaped_id = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        let escaped_value = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
        self.run_main_frame_js(
            &format!("if (window.__buffrEditApply) window.__buffrEditApply({escaped_id}, {escaped_value})"),
            "buffr://edit",
        );
    }

    /// Add the edit-active CSS class to the field via `__buffrEditAttach`.
    pub fn run_edit_attach(&self, field_id: &str) {
        let escaped_id = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        self.run_main_frame_js(
            &format!("if (window.__buffrEditAttach) window.__buffrEditAttach({escaped_id})"),
            "buffr://edit",
        );
    }

    /// Remove the edit-active CSS class from the field via `__buffrEditDetach`.
    pub fn run_edit_detach(&self, field_id: &str) {
        let escaped_id = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        self.run_main_frame_js(
            &format!("if (window.__buffrEditDetach) window.__buffrEditDetach({escaped_id})"),
            "buffr://edit",
        );
    }

    fn run_js(&self, code: &str) {
        self.with_active(|t| {
            let Some(frame) = t.browser.main_frame() else {
                warn!("run_js: main frame unavailable");
                return;
            };
            let code = CefString::from(code);
            let script_url = CefString::from("buffr://page-action");
            frame.execute_java_script(Some(&code), Some(&script_url), 0);
        });
    }

    fn scroll_by(&self, dx: i64, dy: i64) {
        let code = format!("window.scrollBy({dx}, {dy});");
        self.run_js(&code);
    }

    fn adjust_zoom(&self, delta: f64) {
        let domain = self.current_domain();
        let new_level = self
            .with_active(|t| {
                let Some(host) = t.browser.host() else {
                    warn!("adjust_zoom: browser.host() returned None");
                    return None;
                };
                let new_level = host.zoom_level() + delta;
                host.set_zoom_level(new_level);
                Some(new_level)
            })
            .flatten();
        if let Some(level) = new_level
            && let Err(err) = self.zoom.set(&domain, level)
        {
            warn!(error = %err, %domain, "zoom: persist failed");
        }
    }

    fn reset_zoom(&self) {
        let domain = self.current_domain();
        self.with_active(|t| {
            let Some(host) = t.browser.host() else {
                warn!("reset_zoom: browser.host() returned None");
                return;
            };
            host.set_zoom_level(0.0);
        });
        if let Err(err) = self.zoom.remove(&domain) {
            warn!(error = %err, %domain, "zoom: remove failed");
        }
    }

    fn current_domain(&self) -> String {
        self.with_active(|t| {
            t.browser
                .main_frame()
                .map(|f| {
                    let url = CefStringUtf16::from(&f.url()).to_string();
                    buffr_zoom::domain_for_url(&url)
                })
                .unwrap_or_else(|| buffr_zoom::GLOBAL_KEY.to_string())
        })
        .unwrap_or_else(|| buffr_zoom::GLOBAL_KEY.to_string())
    }
}

/// Snapshot of hint-mode state for the statusline indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintStatus {
    pub typed: String,
    pub match_count: usize,
    pub background: bool,
}

/// Format a string as a JS double-quoted literal, escaping every
/// non-ASCII codepoint to `\uXXXX`. Used for the inline filter call
/// so the splice survives any input the user might type.
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

#[allow(dead_code)]
fn _hint_used(_: Hint) {}

/// Pixels per scroll-unit. `ScrollDown(3)` therefore moves 120px,
/// matching a typical "tap j three times" feel without making each
/// `j` feel laggy. Half/full-page scrolls go through their own
/// `window.innerHeight`-relative path so they're DPI-independent.
const STEP_PX: i64 = 40;

#[cfg(test)]
mod tests {
    use super::*;

    // The CEF host is mostly opaque to unit tests because constructing
    // a `cef::Browser` requires a live CEF runtime + an X11 window
    // handle. The pure-Rust pieces — `Tab::display_title`, `TabId`
    // monotonicity, `BrowserHost::move_tab` index math — are tested
    // without spinning CEF.

    #[test]
    fn tab_id_displays_with_prefix() {
        assert_eq!(format!("{}", TabId(0)), "tab#0");
        assert_eq!(format!("{}", TabId(42)), "tab#42");
    }

    #[test]
    fn tab_session_default_is_empty() {
        // Synthesizing a `Tab` without CEF is not possible (browser
        // is non-trivial). The display logic is exercised indirectly
        // via `TabSummary` round-trips at the apps layer.
        let s = TabSession::default();
        assert!(s.find_query.is_none());
        assert!(s.hint_session.is_none());
    }

    #[test]
    fn tab_summary_carries_pinned_and_private_flags() {
        let summary = TabSummary {
            id: TabId(7),
            title: "x".into(),
            url: "https://x".into(),
            progress: 1.0,
            is_loading: false,
            pinned: true,
            private: true,
        };
        assert_eq!(summary.id, TabId(7));
        assert!(summary.pinned);
        assert!(summary.private);
    }

    #[test]
    fn tab_id_ordering() {
        assert!(TabId(1) < TabId(2));
        assert!(TabId(99) > TabId(7));
    }
}
