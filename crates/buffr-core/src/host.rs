//! [`BrowserHost`] — a tab manager owning N concurrent CEF browsers.
//!
//! ## Linux backend matrix
//!
//! - **Off-screen rendering** (`HostMode::Osr`): CEF paints into a
//!   buffer; we composite it onto a Wayland surface via softbuffer.
//!   Linux is always OSR — X11/XWayland windowed embedding is not
//!   supported. RenderHandler wiring lands in step 2; compositing in
//!   step 4.
//!
//! ## Multi-tab architecture
//!
//! All tabs share a **single** [`cef::Client`] (so the load/display/
//! find/download handlers continue funnelling events into the same
//! history / downloads / find sinks). Each tab owns its own
//! [`cef::Browser`]; switching tabs calls
//! `was_hidden(true)` on the previous and `was_hidden(false)` +
//! `set_focus(true)` on the next. See `docs/multi-tab.md`.

use std::collections::VecDeque;
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
use crate::osr::{OsrFrame, OsrPaintHandler, OsrViewState, SharedOsrFrame, SharedOsrViewState};
use crate::permissions::PermissionsQueue;
use crate::telemetry::{KEY_TABS_OPENED, UsageCounters};
use crate::{CoreError, PopupQueue, handlers, new_popup_queue};

/// Rendering mode for a [`BrowserHost`].
///
/// Auto-detected from the `RawWindowHandle` variant passed to
/// [`BrowserHost::new_with_options`]:
/// - Linux: always [`HostMode::Osr`] (Wayland softbuffer composite;
///   X11/XWayland windowed embedding is not supported)
/// - macOS (`AppKit(_)`) → [`HostMode::Windowed`]
/// - Windows (`Win32(_)`) → [`HostMode::Windowed`]
/// - Any other handle → [`HostMode::Osr`] (safe fallback)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMode {
    /// Windowed embedding via OS-native child window (macOS / Windows).
    Windowed,
    /// Off-screen rendering — CEF paints to a buffer we composite ourselves.
    Osr,
}

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

/// Thread-safe queue of `(cef_browser_id, url)` pairs pushed by
/// `BuffrDisplayHandler::on_address_change` on the CEF IO thread and
/// drained by [`BrowserHost::pump_address_changes`] on the UI thread.
pub type AddressSink = Arc<Mutex<VecDeque<(i32, String)>>>;

/// Owns N concurrent CEF browsers.
///
/// The host is created **after** `cef::initialize` succeeds. On Linux
/// the host always uses OSR mode (softbuffer composite over Wayland).
/// On macOS/Windows the CEF child window is parented natively.
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
    /// Rendering mode — windowed embedding or off-screen rendering.
    /// Detected from the `RawWindowHandle` variant at construction time.
    mode: HostMode,
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
    /// Shared OSR frame buffer. Written by the CEF IO thread
    /// (`OsrPaintHandler::on_paint`), read by the compositor (step 4).
    /// Always allocated — cheap even in windowed mode.
    osr_frame: SharedOsrFrame,
    /// Shared OSR viewport dimensions and scale factor. Written from
    /// the UI thread via [`Self::osr_resize`]; read by the CEF IO
    /// thread inside `OsrPaintHandler::view_rect`.
    osr_view: SharedOsrViewState,
    /// Clipboard sink. Lazily constructed once; guarded by `Mutex` so
    /// `dispatch` (called from the UI thread) can reach it without
    /// requiring `&mut self`. `hjkl_clipboard::Clipboard::new` is
    /// infallible, so this is always `Some` after construction.
    clipboard: Mutex<hjkl_clipboard::Clipboard>,
    /// URLs queued by `LifeSpanHandler::on_before_popup`. The main loop
    /// drains these each tick and calls `open_tab` so popups/OAuth flows
    /// open as tabs rather than spawning a separate OS window.
    popup_queue: PopupQueue,
    /// Address changes pushed by `BuffrDisplayHandler::on_address_change`
    /// on the CEF IO thread. The UI thread drains via
    /// [`Self::pump_address_changes`] each tick and writes `Tab.url`.
    address_sink: AddressSink,
    /// Stack of recently closed tabs — `(url, original_index, pinned)`.
    /// Pushed in `close_index`; popped by `reopen_closed_tab`. Capped to
    /// keep memory bounded for long-running sessions.
    closed_stack: Mutex<Vec<ClosedTab>>,
}

/// Stashed live tab for `reopen_closed_tab`. The CEF browser is kept
/// alive (just hidden via `was_hidden(1)`) so re-opening preserves
/// the back/forward history, scroll position, form state, and any
/// in-flight JS — closing-and-recreating a fresh browser would lose
/// all of it. Stack overflow drops the oldest entry, which Drops the
/// `Tab` and tears down its browser.
struct ClosedTab {
    tab: Tab,
    index: usize,
}

// Each stashed entry keeps a live CEF browser hidden in memory so
// reopen preserves history. Cap kept tight to bound the resident-set
// cost — Chromium browsers are expensive even hidden.
const CLOSED_STACK_CAP: usize = 8;

impl BrowserHost {
    /// Create the host with a single initial tab loading `url`.
    ///
    /// `window_handle` is the platform window the CEF browser will be
    /// parented to. On Linux the host always uses OSR mode regardless
    /// of the handle variant. All later tabs created via
    /// [`Self::open_tab`] re-use this handle.
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
        // Linux is always OSR (Wayland softbuffer composite). X11/XWayland
        // windowed embedding is not supported. macOS and Windows use their
        // native child-window paths.
        let mode = match window_handle {
            #[cfg(target_os = "macos")]
            RawWindowHandle::AppKit(_) => HostMode::Windowed,
            #[cfg(target_os = "windows")]
            RawWindowHandle::Win32(_) => HostMode::Windowed,
            _ => HostMode::Osr,
        };
        info!(target: "buffr_core::host", ?mode, "creating CEF browser (initial tab)");
        tracing::debug!(target: "buffr_core::host", %url, "creating CEF browser (initial tab) — url");
        let (osr_w, osr_h) = initial_size;
        let osr_view = Arc::new(OsrViewState::new());
        osr_view
            .width
            .store(osr_w, std::sync::atomic::Ordering::Relaxed);
        osr_view
            .height
            .store(osr_h, std::sync::atomic::Ordering::Relaxed);
        let osr_frame = Arc::new(Mutex::new(OsrFrame::new(osr_w, osr_h)));

        let popup_queue = new_popup_queue();
        let address_sink: AddressSink = Arc::new(Mutex::new(VecDeque::new()));
        let host = Self {
            tabs: Mutex::new(Vec::new()),
            active: Mutex::new(None),
            next_id: AtomicU64::new(0),
            parent_handle: Mutex::new(Some(window_handle)),
            last_size: Mutex::new(initial_size),
            private,
            mode,
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
            osr_frame,
            osr_view,
            clipboard: Mutex::new(hjkl_clipboard::Clipboard::new()),
            popup_queue,
            address_sink,
            closed_stack: Mutex::new(Vec::new()),
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

    /// Clone the popup URL queue. The main loop drains this each tick and
    /// calls [`Self::open_tab`] for each entry so CEF popups (OAuth flows,
    /// `window.open()`, `target="_blank"`) open as tabs.
    pub fn popup_queue(&self) -> PopupQueue {
        self.popup_queue.clone()
    }

    /// Current rendering mode (windowed embedding or OSR).
    pub fn mode(&self) -> HostMode {
        self.mode
    }

    /// Returns the cached main-frame URL of the active tab. Updated by
    /// `pump_address_changes` whenever CEF fires `on_address_change`.
    /// Empty string if no active tab.
    pub fn active_tab_live_url(&self) -> String {
        let Ok(tabs) = self.tabs.lock() else {
            return String::new();
        };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
        {
            t.url.clone()
        } else {
            String::new()
        }
    }

    /// Drain all queued `on_address_change` events and apply them to the
    /// matching tab's `url` field. Returns `true` when at least one tab
    /// URL changed so the caller can request a redraw and mark the
    /// session dirty.
    pub fn pump_address_changes(&self) -> bool {
        let changes: Vec<(i32, String)> = {
            let Ok(mut guard) = self.address_sink.lock() else {
                return false;
            };
            guard.drain(..).collect()
        };
        if changes.is_empty() {
            return false;
        }
        let Ok(mut tabs) = self.tabs.lock() else {
            return false;
        };
        let mut changed = false;
        for (browser_id, url) in changes {
            for t in tabs.iter_mut() {
                if t.browser.identifier() == browser_id {
                    t.url = url;
                    changed = true;
                    break;
                }
            }
        }
        changed
    }

    /// Active tab's CEF zoom level. Returns 0.0 when no active tab —
    /// CEF's "default" baseline. Each integer step is roughly a 20%
    /// change (1.0 ≈ 120%, -1.0 ≈ 83%).
    pub fn active_zoom_level(&self) -> f64 {
        let Ok(tabs) = self.tabs.lock() else {
            return 0.0;
        };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            host.zoom_level()
        } else {
            0.0
        }
    }

    /// Clone the shared OSR frame buffer handle.
    ///
    /// The compositor (step 4) holds this to read the latest painted frame
    /// each vsync without holding any CEF locks.
    pub fn osr_frame(&self) -> SharedOsrFrame {
        self.osr_frame.clone()
    }

    /// Clone the shared OSR viewport state handle.
    ///
    /// The UI thread calls [`Self::osr_resize`] to update the dimensions;
    /// the CEF IO thread reads them inside `view_rect`.
    pub fn osr_view(&self) -> SharedOsrViewState {
        self.osr_view.clone()
    }

    // ---- OSR input forwarding -------------------------------------------

    /// Forward a mouse-move to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode (native child window routes input).
    pub fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        if self.mode != HostMode::Osr {
            return;
        }
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_move_event(Some(&event), 0);
        }
    }

    /// Forward a mouse-click to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: cef::MouseButtonType,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        if self.mode != HostMode::Osr {
            return;
        }
        tracing::debug!(
            target: "buffr_core::host",
            x, y, ?button, mouse_up, click_count, modifiers,
            "osr_mouse_click"
        );
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_click_event(Some(&event), button, mouse_up as i32, click_count);
        } else {
            warn!(target: "buffr_core::host", "osr_mouse_click: no active browser host — click dropped");
        }
    }

    /// Notify CEF the mouse left the window.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_mouse_leave(&self, modifiers: u32) {
        if self.mode != HostMode::Osr {
            return;
        }
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            let event = cef::MouseEvent {
                x: 0,
                y: 0,
                modifiers,
            };
            host.send_mouse_move_event(Some(&event), 1);
        }
    }

    /// Forward a mouse-wheel event to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        if self.mode != HostMode::Osr {
            return;
        }
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
        }
    }

    /// Forward a keyboard event to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_key_event(&self, event: cef::KeyEvent) {
        if self.mode != HostMode::Osr {
            return;
        }
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            host.send_key_event(Some(&event));
        }
    }

    /// Notify CEF of focus changes.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_focus(&self, focused: bool) {
        if self.mode != HostMode::Osr {
            return;
        }
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            host.set_focus(if focused { 1 } else { 0 });
        }
    }

    /// Notify CEF that the viewport has been resized.
    ///
    /// Updates the atomic viewport dimensions so the next `view_rect` call
    /// from CEF returns the correct size, then calls `was_resized()` on the
    /// active browser so CEF schedules a repaint at the new dimensions.
    pub fn osr_resize(&self, width: u32, height: u32) {
        use std::sync::atomic::Ordering;
        self.osr_view.width.store(width, Ordering::Relaxed);
        self.osr_view.height.store(height, Ordering::Relaxed);
        if let Ok(mut last) = self.last_size.lock() {
            *last = (width, height);
        }
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            host.was_resized();
        }
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

    /// Pop the most recently closed tab off the undo stack and put
    /// it back at its original position. The CEF browser was kept
    /// alive while stashed, so back/forward history, scroll position,
    /// and any in-flight JS state are preserved. Returns `Ok(None)`
    /// when the stack is empty so the caller can no-op silently.
    pub fn reopen_closed_tab(&self) -> Result<Option<TabId>, CoreError> {
        let entry = match self.closed_stack.lock() {
            Ok(mut s) => s.pop(),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        let id = entry.tab.id;
        // Insert the live Tab back into the strip at its original
        // index (clamped). Doing the insert before un-hiding ensures
        // tab_count / set_active_index see the right slot.
        let final_idx = {
            let mut tabs = self
                .tabs
                .lock()
                .map_err(|_| CoreError::CreateBrowserFailed)?;
            let clamped = entry.index.min(tabs.len());
            tabs.insert(clamped, entry.tab);
            clamped
        };
        // Un-hide and focus the restored browser.
        if let Ok(tabs) = self.tabs.lock()
            && let Some(t) = tabs.get(final_idx)
            && let Some(host) = t.browser.host()
        {
            host.was_hidden(0);
            host.was_resized();
        }
        self.set_active_index(final_idx);
        Ok(Some(id))
    }

    /// Number of tabs currently sitting on the closed-tab undo stack.
    /// Cheap; use to gate the apps-layer "no closed tabs" feedback.
    pub fn closed_stack_len(&self) -> usize {
        self.closed_stack.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Open a new tab and place it at `insert_idx` in the tab list.
    /// Out-of-bounds indices clamp to the end. Returns the new tab's
    /// [`TabId`]. The new tab becomes active and the active index is
    /// adjusted if the insertion pushed the previous active tab down.
    pub fn open_tab_at(&self, url: &str, insert_idx: usize) -> Result<TabId, CoreError> {
        // Create the browser appended (background=true so focus stays
        // on the current tab while we reorder).
        let id = self.create_browser(url, true)?;

        // Move the freshly-appended tab to the requested position.
        {
            let mut tabs = self
                .tabs
                .lock()
                .map_err(|_| CoreError::CreateBrowserFailed)?;
            let appended_idx = tabs.len() - 1;
            let clamped = insert_idx.min(appended_idx);
            if clamped != appended_idx {
                let tab = tabs.remove(appended_idx);
                tabs.insert(clamped, tab);
                // Fix up active index: the removal + re-insert shifts
                // any active tab that was at or after `clamped`.
                let mut active = self
                    .active
                    .lock()
                    .map_err(|_| CoreError::CreateBrowserFailed)?;
                if let Some(a) = *active {
                    // After removing from appended_idx and inserting at
                    // clamped, indices in [clamped, appended_idx) shift +1.
                    if a >= clamped && a < appended_idx {
                        *active = Some(a + 1);
                    }
                }
            }
        }

        // Now make the new tab active at the clamped position.
        let final_idx = {
            let tabs = self
                .tabs
                .lock()
                .map_err(|_| CoreError::CreateBrowserFailed)?;
            tabs.iter().position(|t| t.id == id)
        };
        if let Some(idx) = final_idx {
            self.set_active_index(idx);
        }
        Ok(id)
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

        // Force Alloy runtime style. CEF 147's `default` is Chrome style,
        // which spawns its own top-level window with the full Chrome UI even
        // when `parent_window` is set — that's why an unmodified `default()`
        // renders as a separate window instead of embedding into the winit
        // surface. Alloy honours `parent_window` for windowed embedding and
        // is the right pick for a custom-chrome browser like buffr.
        let mut window_info = WindowInfo {
            bounds: cef::Rect {
                x: 0,
                y: 0,
                width: init_w as i32,
                height: init_h as i32,
            },
            runtime_style: cef::sys::cef_runtime_style_t::CEF_RUNTIME_STYLE_ALLOY.into(),
            ..WindowInfo::default()
        };
        match self.mode {
            HostMode::Windowed => {
                // Capture bounds before moving window_info into set_as_child.
                // Named with underscore so Linux (OSR-only) doesn't warn on
                // the dead binding; macOS/Windows arms below reference it.
                #[allow(unused_variables)]
                let bounds = window_info.bounds.clone();
                match handle {
                    #[cfg(target_os = "macos")]
                    RawWindowHandle::AppKit(h) => {
                        window_info = window_info.set_as_child(h.ns_view.as_ptr() as _, &bounds);
                    }
                    #[cfg(target_os = "windows")]
                    RawWindowHandle::Win32(h) => {
                        // raw_window_handle gives us `isize`; CEF's HWND is `*mut HWND__`.
                        window_info = window_info
                            .set_as_child(cef::sys::HWND(h.hwnd.get() as *mut _), &bounds);
                    }
                    other => {
                        tracing::warn!(
                            ?other,
                            "windowed mode but unrecognised handle variant — \
                                 cannot embed CEF child window"
                        );
                        return Err(CoreError::CreateBrowserFailed);
                    }
                }
            }
            HostMode::Osr => {
                // Off-screen rendering: no parent_window; CEF will call the
                // RenderHandler instead of creating a child window.
                // windowless_rendering_enabled is set on the WindowInfo so CEF
                // takes the OSR path. RenderHandler wiring comes in step 2.
                window_info.windowless_rendering_enabled = 1;
                tracing::info!("creating CEF browser in OSR mode");
            }
        }
        tracing::info!(
            bounds_w = window_info.bounds.width,
            bounds_h = window_info.bounds.height,
            windowless_rendering_enabled = window_info.windowless_rendering_enabled,
            runtime_style = ?window_info.runtime_style,
            "create_browser: window_info"
        );

        let cef_url = CefString::from(url);
        let settings = BrowserSettings::default();

        // Build the render handler for OSR mode; None for windowed.
        let render_handler = if self.mode == HostMode::Osr {
            Some(OsrPaintHandler::new(
                self.osr_frame.clone(),
                self.osr_view.clone(),
            ))
        } else {
            None
        };

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
            render_handler,
            self.popup_queue.clone(),
            self.address_sink.clone(),
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
        info!(target: "buffr_core::host", %id, background, "tab opened");
        tracing::debug!(target: "buffr_core::host", %url, "tab opened — url");
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

        // Decide whether this tab is worth stashing on the closed-tabs
        // undo stack: blank pages aren't (re-opening them is the same
        // as `:tabnew`).
        let stashable = !removed.url.is_empty() && removed.url != "about:blank";

        if stashable {
            // Hide the browser but keep it alive so a future
            // `reopen_closed_tab` preserves history, scroll, and form
            // state. `close_browser` is only called when the entry
            // ages out of the stack (see eviction below).
            if let Some(host) = removed.browser.host() {
                host.was_hidden(1);
                host.set_focus(0);
            }
            let evicted: Vec<Tab> = if let Ok(mut stack) = self.closed_stack.lock() {
                stack.push(ClosedTab {
                    tab: removed,
                    index: idx,
                });
                let extra = stack.len().saturating_sub(CLOSED_STACK_CAP);
                if extra > 0 {
                    stack.drain(0..extra).map(|c| c.tab).collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            // Tear down any stack-evicted browsers outside the lock.
            for t in evicted {
                if let Some(host) = t.browser.host() {
                    host.close_browser(1);
                }
            }
        } else {
            // Not stashable — close immediately.
            if let Some(host) = removed.browser.host() {
                host.close_browser(1);
            }
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

    /// Set the pinned bit on the tab with `id`. Used by session
    /// restore to apply the saved pin state without depending on
    /// which tab is currently active.
    pub fn set_pinned(&self, id: TabId, pinned: bool) {
        let Ok(mut tabs) = self.tabs.lock() else {
            return;
        };
        if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
            t.pinned = pinned;
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
    /// dimensions. In windowed mode (macOS/Windows) the native child
    /// window may need a separate resize call; in OSR mode CEF
    /// re-paints at the new dimensions reported by `view_rect`.
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
            tracing::debug!(target: "buffr_core::host", url = %trimmed, "navigate");
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
                tracing::trace!(
                    forward = *forward,
                    "Find: intercepted at apps layer; host dispatch is a no-op."
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
            A::TabNewRight | A::TabNewLeft => {
                // Adjacent-tab opens are handled at the apps layer (which
                // also opens the omnibar). The host fallback just appends.
                let _ = self.open_tab("about:blank");
            }
            A::PinTab => self.toggle_pin_active(),
            A::ReopenClosedTab => match self.reopen_closed_tab() {
                Ok(Some(_)) => {}
                Ok(None) => tracing::debug!("reopen_closed_tab: stack empty"),
                Err(err) => tracing::warn!(error = %err, "reopen_closed_tab: failed"),
            },
            A::PasteUrl { .. } => {
                // Paste-as-tab needs both clipboard read and search-config
                // URL classification, which the apps layer owns. The host
                // dispatch fallback is a no-op so a stray dispatch on a
                // private host (no apps wiring) doesn't open a junk tab.
                tracing::debug!("PasteUrl reached host dispatch — apps layer should handle it");
            }
            A::TabReorder { from, to } => self.move_tab(*from as usize, *to as usize),
            A::MoveTabLeft => {
                if let Some(idx) = self.active_index()
                    && idx > 0
                {
                    self.move_tab(idx, idx - 1);
                }
            }
            A::MoveTabRight => {
                if let Some(idx) = self.active_index() {
                    let last = self.tab_count().saturating_sub(1);
                    if idx < last {
                        self.move_tab(idx, idx + 1);
                    }
                }
            }

            A::OpenOmnibar | A::OpenCommandLine => {
                tracing::info!("UI action — overlay rendering owned by apps layer");
            }
            A::EnterHintMode => self.enter_hint_mode(false),
            A::EnterHintModeBackground => self.enter_hint_mode(true),

            A::EnterMode(mode) => {
                tracing::info!(?mode, "EnterMode — engine tracks mode internally");
            }
            A::EnterInsertMode => {
                tracing::info!(
                    "insert-mode requested — hjkl-engine integration is Phase 2b \
                     (blocked on hjkl Host trait)"
                );
            }

            A::FocusFirstInput => {
                self.run_js(crate::scripts::FOCUS_FIRST_INPUT);
            }

            A::ExitInsertMode => {
                // Blur whatever the page has focused. The DOM blur event will
                // propagate to edit.js, which posts a `blur` console event;
                // the main loop drains it and resets EditFocus. As a defensive
                // measure, the caller (apps/buffr/src/main.rs's dispatch_action
                // path) should ALSO clear local state synchronously.
                self.run_js(crate::scripts::EXIT_INSERT);
            }

            A::ClearCompletedDownloads => match self.downloads.clear_completed() {
                Ok(n) => tracing::info!(removed = n, "downloads: cleared completed"),
                Err(err) => tracing::warn!(error = %err, "downloads: clear_completed failed"),
            },

            A::YankUrl => {
                self.with_active(|t| {
                    if let Some(frame) = t.browser.main_frame() {
                        let url = CefStringUtf16::from(&frame.url()).to_string();
                        if let Ok(mut cb) = self.clipboard.lock() {
                            if cb.set_text(&url) {
                                tracing::debug!(url, "yanked URL to clipboard");
                            } else {
                                tracing::warn!(
                                    url,
                                    "yank failed: clipboard set_text returned false"
                                );
                            }
                        }
                    } else {
                        tracing::warn!("YankUrl: main frame unavailable");
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

    /// Re-focus a field by its buffr-assigned ID via `__buffrEditFocus`.
    pub fn run_edit_focus(&self, field_id: &str) {
        let escaped_id = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        self.run_main_frame_js(
            &format!("if (window.__buffrEditFocus) window.__buffrEditFocus({escaped_id})"),
            "buffr://edit",
        );
    }

    /// Move focus to the next (or previous) visible input via
    /// `__buffrCycleInput`. Used to override Tab/Shift+Tab in Insert
    /// mode so cycling skips links/buttons.
    /// Read text off the system clipboard. Returns `None` when the
    /// clipboard is empty, holds non-text content (image, files, …),
    /// or the platform read fails. Used by the apps layer's paste-as
    /// -tab dispatch before classifying the contents as a URL.
    pub fn clipboard_text(&self) -> Option<String> {
        self.clipboard.lock().ok().and_then(|mut cb| cb.get_text())
    }

    pub fn run_edit_cycle(&self, forward: bool) {
        let arg = if forward { "true" } else { "false" };
        self.run_main_frame_js(
            &format!("if (window.__buffrCycleInput) window.__buffrCycleInput({arg})"),
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

    pub fn run_js(&self, code: &str) {
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
    // a `cef::Browser` requires a live CEF runtime + a native window
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
