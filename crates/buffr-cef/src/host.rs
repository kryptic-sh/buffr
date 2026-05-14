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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use tracing::{info, warn};

use crate::audio::{
    AudioEventQueue, AudioStateSink, any_audio_active as audio_any_active,
    drain_audio_events as audio_drain_events, new_audio_event_queue, new_audio_state_sink,
};
use crate::handlers;
use crate::osr::{
    OsrFrame, OsrViewState, PopupFrameMap, SharedOsrFrame, SharedOsrViewState,
    make_osr_paint_handler,
};
use crate::permissions::PermissionsQueue;
use crate::{
    PendingPopupAlloc, PopupCloseSink, PopupCreateSink, PopupQueue, new_pending_popup_alloc,
    new_popup_close_sink, new_popup_create_sink, new_popup_queue,
};
use buffr_core::CoreError;
use buffr_core::context_menu::{ContextMenuRequest, ContextMenuSink, new_context_menu_sink};
use buffr_core::cursor::{CursorState, SharedCursorState};
use buffr_core::download_notice::DownloadNoticeQueue;
use buffr_core::edit::EditEventSink;
use buffr_core::favicon::favicon_is_enabled;
use buffr_core::favicon::{FaviconEnabled, FaviconSink, new_favicon_enabled, new_favicon_sink};
use buffr_core::find::FindResultSink;
use buffr_core::hint::{
    DEFAULT_HINT_SELECTORS, Hint, HintAction, HintAlphabet, HintEventSink, HintSession,
    build_inject_script,
};
use buffr_core::telemetry::{KEY_TABS_OPENED, UsageCounters};

// `HostMode` removed: every platform now runs OSR. CEF paints into a
// shared bitmap, the wgpu present layer composites it under buffr's
// chrome strips. Windows previously embedded a CEF child HWND;
// migrated to OSR so platform behavior is uniform (cursor sync, IME
// integration, scale-factor changes, MSI footprint).

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

/// User-facing prefix for "view source" navigations. `buffr-src:` URLs
/// are now served by the custom CEF scheme handler in
/// `view_source_scheme.rs` — no rewrite to `view-source:` at the
/// navigation boundary. This const is kept public because
/// `apps/buffr-app/src/main.rs` constructs `buffr-src:<url>` literals
/// when dispatching "view page source" from the context menu.
pub const BUFFR_SRC_PREFIX: &str = "buffr-src:";

/// Translate a user-facing URL into the form CEF should actually
/// navigate to. `buffr-src:` URLs reach CEF unmodified — the custom
/// scheme handler registered in `view_source_scheme.rs` intercepts
/// them. Identity for all URLs.
fn to_cef_navigation_url(url: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(url)
}

/// Translate a CEF-emitted URL back into the user-facing form.
///
/// Rewrites `view-source:` → `buffr-src:` as a safety net for the rare
/// case where a `view-source:` URL leaks into a chrome surface (e.g.
/// via the context menu, a session restore from an old session file, or
/// a Chromium-internal redirect). Normal navigation through our custom
/// scheme never produces a `view-source:` address-change event, so this
/// fires only in defensive edge cases.
fn to_display_url(url: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = url.strip_prefix("view-source:") {
        std::borrow::Cow::Owned(format!("{BUFFR_SRC_PREFIX}{rest}"))
    } else {
        std::borrow::Cow::Borrowed(url)
    }
}

/// Apply an incoming navigation URL (typically from CEF's
/// `on_address_change` or `on_load_end`) to `current`. Returns
/// `Some(new_url)` if the value should change, `None` to leave it
/// untouched.
///
/// With the `buffr-src:` custom scheme handler wired up, CEF emits
/// address-change events with the `buffr-src:` URL intact — the old
/// suffix-preservation heuristic (needed when we translated to
/// `view-source:` at the navigation boundary) is no longer required.
///
/// The incoming URL is still normalized through [`to_display_url`] as a
/// safety net for sessions restored from old session files or any other
/// code path that leaks a raw `view-source:` URL into the address bar.
fn merge_navigation_url(current: &str, incoming: &str) -> Option<String> {
    let normalized = to_display_url(incoming);
    if normalized == current {
        None
    } else {
        Some(normalized.into_owned())
    }
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
    /// CEF `Browser::identifier()`. Lets the apps layer correlate this
    /// tab with browser-id-keyed sinks (favicon downloads, cursor state).
    pub browser_id: i32,
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
/// The host is created **after** `cef::initialize` succeeds. Linux and
/// macOS use OSR mode so the page and custom chrome share one compositor
/// surface. On Windows the CEF child window is parented natively.
pub struct BrowserHost {
    /// Live tab list. Only the active tab is visible; inactive tabs
    /// are `was_hidden(true)`.
    tabs: Mutex<Vec<Tab>>,
    /// Index into `tabs` of the active tab. `None` only between
    /// `close_active` of the last tab and the caller's exit decision.
    active: Mutex<Option<usize>>,
    /// Monotonic [`TabId`] minter. Reset only across process restart.
    next_id: AtomicU64,
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
    /// Shared OSR frame buffer. Written by the CEF IO thread
    /// (`OsrPaintHandler::on_paint`), read by the compositor (step 4).
    /// Always allocated — cheap even in windowed mode.
    osr_frame: SharedOsrFrame,
    /// Shared OSR viewport dimensions and scale factor. Written from
    /// the UI thread via [`Self::osr_resize`]; read by the CEF IO
    /// thread inside `OsrPaintHandler::view_rect`.
    osr_view: SharedOsrViewState,
    /// Clipboard sink. `hjkl_clipboard::Clipboard` (0.5+) is
    /// `Send + Sync` with `&self` methods but no longer `Clone`, so we
    /// wrap in `Arc` to hand cheap reference clones to worker threads
    /// via `clipboard_handle`. `Clipboard::new` may fail (no display,
    /// missing libs); `None` disables clipboard ops gracefully.
    clipboard: Option<Arc<hjkl_clipboard::Clipboard>>,
    /// Set by `BuffrLoadHandler::on_load_start` (main frame), cleared
    /// by `OsrPaintHandler::on_paint`. Apps layer reads via
    /// [`Self::is_loading`] to keep the loading anim playing across
    /// the navigation gap until the first contentful frame commits.
    loading_busy: Arc<AtomicBool>,
    /// URLs queued by `LifeSpanHandler::on_before_popup` for new-tab
    /// dispositions (`NEW_FOREGROUND_TAB`, `NEW_BACKGROUND_TAB`). The
    /// main loop drains each tick and opens them as tabs.
    popup_queue: PopupQueue,
    /// Address changes pushed by `BuffrDisplayHandler::on_address_change`
    /// on the CEF IO thread. The UI thread drains via
    /// [`Self::pump_address_changes`] each tick and writes `Tab.url`.
    address_sink: AddressSink,
    /// Stack of recently closed tabs — `(url, original_index, pinned)`.
    /// Pushed in `close_index`; popped by `reopen_closed_tab`. Capped to
    /// keep memory bounded for long-running sessions.
    closed_stack: Mutex<Vec<ClosedTab>>,
    // ── Popup-window OSR plumbing ──────────────────────────────────────────
    /// Popup browser OSR state, keyed by CEF `browser.identifier()`.
    /// Shared with every `OsrPaintHandler` so paint callbacks route to
    /// the right frame buffer. Written by `on_after_created`; removed by
    /// `popup_close` or at shutdown.
    popup_frames: PopupFrameMap,
    /// Single-slot pending alloc: allocated by `on_before_popup` for
    /// `NEW_POPUP` / `NEW_WINDOW` dispositions before the browser id is
    /// known, then consumed by `on_after_created` once CEF assigns an id.
    /// Assumption: `on_before_popup` and the matching `on_after_created`
    /// are sequenced on the same CEF UI thread without interleaving.
    pending_popup_alloc: PendingPopupAlloc,
    /// Events emitted when a popup browser is fully created. The apps
    /// layer drains these each tick and spawns a winit window per entry.
    popup_create_sink: PopupCreateSink,
    /// Events emitted when a popup browser is closed. The apps layer
    /// drains these each tick and drops the corresponding winit window.
    popup_close_sink: PopupCloseSink,
    /// Live popup browsers — kept for `popup_resize` / `popup_close`
    /// so the apps layer doesn't need to hold a `cef::Browser` handle.
    /// Keyed by `browser.identifier()`.
    popup_browsers: Arc<Mutex<HashMap<i32, cef::Browser>>>,
    /// Address changes for popup browsers that were not matched against
    /// any tab in `pump_address_changes`. The apps layer drains these
    /// each tick to update the popup window's URL bar.
    popup_address_sink: Arc<Mutex<VecDeque<(i32, String)>>>,
    /// Title changes for popup browsers, pushed by `BuffrDisplayHandler::on_title_change`
    /// when `browser.is_popup() != 0`. The apps layer drains these each
    /// tick to update the popup window's winit title.
    popup_title_sink: Arc<Mutex<VecDeque<(i32, String)>>>,
    /// Latest CEF cursor request. Written by `BuffrDisplayHandler::on_cursor_change`
    /// on the CEF IO thread, read by the apps layer each tick to forward into
    /// `winit::Window::set_cursor`.
    cursor_state: SharedCursorState,
    /// Decoded favicon bitmaps keyed by CEF browser id. Pushed by
    /// `BuffrDownloadImageCallback` after a `download_image` round-trip;
    /// drained by the apps layer each tick to populate the tab strip.
    favicon_sink: FaviconSink,
    /// Master on/off for favicon downloads. Flipped from
    /// `[general] show_favicons`. When false, `on_favicon_urlchange`
    /// short-circuits before issuing a `download_image` call.
    favicon_enabled: FaviconEnabled,
    /// Per-browser audio stream state. Written by [`BuffrAudioHandler`] on
    /// the CEF audio thread; read by the UI thread via [`Self::any_audio_active`].
    audio_sink: AudioStateSink,
    /// Edge-event queue: one entry per active→inactive or inactive→active
    /// transition per browser. Drained by the UI thread each tick via
    /// [`Self::drain_audio_events`].
    audio_queue: AudioEventQueue,
    /// Latest `__buffr_video_active` snapshot from the JS media probe.
    /// Written by [`crate::handlers::BuffrDisplayHandler::on_console_message`]
    /// when it scrapes a `__buffr_media__:` sentinel line; read by
    /// [`Self::any_video_active`] which the apps-layer idle-inhibit policy
    /// consults each tick.
    video_active: Arc<AtomicBool>,
    /// Queue of right-click events translated into [`ContextMenuRequest`]s
    /// by `BuffrContextMenuHandler::run_context_menu`. The apps layer
    /// drains these each tick via [`Self::drain_context_menu_requests`].
    context_menu_sink: ContextMenuSink,
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

/// Opaque clipboard reader handed out by `BrowserHost::clipboard_handle`.
///
/// Wraps the underlying `hjkl-clipboard` handle so embedders can read
/// the system clipboard from a worker thread without depending on the
/// crate directly. The handle is shared via `Arc`, so the wrapper is
/// `Clone + Send + Sync` — clone it freely, move it across threads.
#[derive(Clone)]
pub struct ClipboardReader(Arc<hjkl_clipboard::Clipboard>);

impl ClipboardReader {
    /// Block until the system clipboard yields a non-empty UTF-8 text
    /// payload, or return `None` if it's empty, non-text, or the read
    /// fails. Must be called off the CEF UI thread to avoid the
    /// Wayland self-deadlock when Chromium owns the selection.
    pub fn read_text(&self) -> Option<String> {
        use hjkl_clipboard::{MimeType, Selection};
        match self.0.get(Selection::Clipboard, MimeType::Text) {
            Ok(bytes) => String::from_utf8(bytes).ok().filter(|s| !s.is_empty()),
            Err(err) => {
                tracing::debug!(error = %err, "ClipboardReader::read_text: read failed");
                None
            }
        }
    }
}

impl BrowserHost {
    /// Create the host with a single initial tab loading `url`.
    ///
    /// All platforms run OSR; CEF paints into a buffer the embedder
    /// composites itself. No window handle is needed at construction.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
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
            true,
        )
    }

    /// Like [`Self::new`] but lets the embedder mark every browser as
    /// private. The flag is purely informational — the underlying CEF
    /// profile dirs are already swapped at process start by the
    /// `--private` CLI flag.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_options(
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
        show_favicons: bool,
    ) -> Result<Self, CoreError> {
        // All platforms run OSR. CEF paints into a shared bitmap; the
        // wgpu present layer composites it under buffr's chrome strips.
        info!(target: "buffr_core::host", "creating CEF browser (initial tab) in OSR mode");
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
        let popup_frames: PopupFrameMap = Arc::new(Mutex::new(HashMap::new()));
        let popup_create_sink = new_popup_create_sink();
        let popup_close_sink = new_popup_close_sink();
        let pending_popup_alloc = new_pending_popup_alloc();
        let host = Self {
            tabs: Mutex::new(Vec::new()),
            active: Mutex::new(None),
            next_id: AtomicU64::new(0),
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
            osr_frame,
            osr_view,
            clipboard: match hjkl_clipboard::Clipboard::new() {
                Ok(cb) => Some(Arc::new(cb)),
                Err(err) => {
                    tracing::warn!(error = %err, "clipboard: init failed; yank/paste disabled");
                    None
                }
            },
            popup_queue,
            address_sink,
            closed_stack: Mutex::new(Vec::new()),
            popup_frames,
            pending_popup_alloc,
            popup_create_sink,
            popup_close_sink,
            popup_browsers: Arc::new(Mutex::new(HashMap::new())),
            popup_address_sink: Arc::new(Mutex::new(VecDeque::new())),
            popup_title_sink: Arc::new(Mutex::new(VecDeque::new())),
            cursor_state: Arc::new(CursorState::new()),
            favicon_sink: new_favicon_sink(),
            favicon_enabled: new_favicon_enabled(show_favicons),
            loading_busy: Arc::new(AtomicBool::new(false)),
            audio_sink: new_audio_state_sink(),
            audio_queue: new_audio_event_queue(),
            video_active: Arc::new(AtomicBool::new(false)),
            context_menu_sink: new_context_menu_sink(),
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

    /// Clone the popup URL queue. The main loop drains it each tick
    /// and calls [`Self::open_tab`] for `target=_blank` / Ctrl+click
    /// intents that `on_before_popup` re-routed.
    pub fn popup_queue(&self) -> PopupQueue {
        self.popup_queue.clone()
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
        let mut popup_changes: Vec<(i32, String)> = Vec::new();
        for (browser_id, url) in changes {
            let mut matched = false;
            for t in tabs.iter_mut() {
                if t.browser.identifier() == browser_id {
                    if let Some(new_url) = merge_navigation_url(&t.url, &url) {
                        t.url = new_url;
                        changed = true;
                    }
                    matched = true;
                    break;
                }
            }
            if !matched {
                popup_changes.push((browser_id, url));
            }
        }
        if !popup_changes.is_empty()
            && let Ok(mut sink) = self.popup_address_sink.lock()
        {
            for entry in popup_changes {
                sink.push_back(entry);
            }
        }
        changed
    }

    /// Drain address-change events for popup browsers (those whose browser
    /// id did not match any open tab). Called by the apps layer each tick
    /// to update popup window URL bars.
    pub fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        if let Ok(mut sink) = self.popup_address_sink.lock() {
            sink.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// Drain title-change events for popup browsers. Called by the apps
    /// layer each tick to update popup window titles.
    pub fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        if let Ok(mut sink) = self.popup_title_sink.lock() {
            sink.drain(..).collect()
        } else {
            Vec::new()
        }
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

    /// Clone the shared cursor state. Apps drain it each tick (or on cursor
    /// movement) and forward the latest CEF cursor request into winit's
    /// `Window::set_cursor`.
    pub fn cursor_state(&self) -> SharedCursorState {
        self.cursor_state.clone()
    }

    /// Clone the shared favicon sink. Apps drain it each tick to refresh the
    /// per-tab favicon cache.
    pub fn favicon_sink(&self) -> FaviconSink {
        self.favicon_sink.clone()
    }

    /// Read the current favicon enable flag. Mirrors `[general] show_favicons`
    /// at startup and reflects any runtime toggle via [`Self::set_favicon_enabled`].
    pub fn favicons_enabled(&self) -> bool {
        favicon_is_enabled(&self.favicon_enabled)
    }

    /// Toggle favicon downloads at runtime. The display handler picks up the
    /// new value on the next `on_favicon_urlchange`.
    pub fn set_favicon_enabled(&self, value: bool) {
        buffr_core::favicon::set_favicon_enabled(&self.favicon_enabled, value);
    }

    /// Install a wake callback fired from `OsrPaintHandler::on_paint`
    /// every time CEF lands a new frame. Embedders use this to nudge
    /// their UI loop (e.g. `winit::EventLoopProxy::send_event`) so the
    /// surface can be repainted without a polling tick. First setter
    /// wins; subsequent calls are silently ignored.
    pub fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.osr_view.wake.set(wake);
    }

    // ---- Popup sinks -------------------------------------------------------

    /// Clone the popup-create sink so the apps layer can drain it.
    pub fn popup_create_sink(&self) -> PopupCreateSink {
        self.popup_create_sink.clone()
    }

    /// Clone the popup-close sink so the apps layer can drain it.
    pub fn popup_close_sink(&self) -> PopupCloseSink {
        self.popup_close_sink.clone()
    }

    /// Notify CEF that a popup window has been resized.
    pub fn popup_resize(&self, browser_id: i32, width: u32, height: u32) {
        // Update the view state so view_rect returns the new dims.
        if let Ok(map) = self.popup_frames.lock()
            && let Some((_, view)) = map.get(&browser_id)
        {
            view.width.store(width, Ordering::Relaxed);
            view.height.store(height, Ordering::Relaxed);
        }
        // Poke CEF so it schedules a repaint at the new size.
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
        }
        tracing::debug!(browser_id, width, height, "popup_resize");
    }

    /// Navigate the popup browser back in its own history.
    pub fn popup_history_back(&self, browser_id: i32) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
        {
            b.go_back();
        }
    }

    /// Navigate the popup browser forward in its own history.
    pub fn popup_history_forward(&self, browser_id: i32) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
        {
            b.go_forward();
        }
    }

    /// Request CEF to close a popup browser. The actual teardown is
    /// asynchronous — `on_before_close` → handler deregisters →
    /// `PopupCloseSink` is the cleanup path.
    pub fn popup_close(&self, browser_id: i32) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            host.close_browser(0);
        }
        tracing::debug!(browser_id, "popup_close requested");
    }

    /// Clone the internal Arc for `popup_browsers`. Passed into
    /// `BuffrLifeSpanHandler` so it can register browsers by id on
    /// `on_after_created`.
    fn popup_browsers_arc(&self) -> Arc<Mutex<HashMap<i32, cef::Browser>>> {
        self.popup_browsers.clone()
    }

    // ---- Popup OSR input forwarding ------------------------------------

    /// Forward a mouse-move to a popup browser.
    pub fn popup_osr_mouse_move(&self, browser_id: i32, x: i32, y: i32, modifiers: u32) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_move_event(Some(&event), 0);
        }
    }

    /// Forward a mouse-click to a popup browser.
    #[allow(clippy::too_many_arguments)]
    pub fn popup_osr_mouse_click(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        button: cef::MouseButtonType,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_click_event(Some(&event), button, mouse_up as i32, click_count);
        }
    }

    /// Forward a mouse-wheel event to a popup browser.
    pub fn popup_osr_mouse_wheel(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    ) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
        }
    }

    /// Forward a keyboard event to a popup browser.
    pub fn popup_osr_key_event(&self, browser_id: i32, event: cef::KeyEvent) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            host.send_key_event(Some(&event));
        }
    }

    /// Set focus on a popup browser.
    pub fn popup_osr_focus(&self, browser_id: i32, focused: bool) {
        if let Ok(browsers) = self.popup_browsers.lock()
            && let Some(b) = browsers.get(&browser_id)
            && let Some(host) = b.host()
        {
            host.set_focus(if focused { 1 } else { 0 });
        }
    }

    // ---- OSR input forwarding -------------------------------------------

    /// Forward a mouse-move to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode (native child window routes input).
    pub fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
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

    /// Notify CEF that the screen / monitor info has changed (e.g. the window
    /// moved to a display with a different scale factor). CEF will re-query
    /// `RenderHandler::screen_info` and `view_rect` for all active tabs.
    pub fn notify_screen_info_changed(&self) {
        let Ok(tabs) = self.tabs.lock() else {
            tracing::debug!("notify_screen_info_changed: tabs mutex poisoned");
            return;
        };
        for t in tabs.iter() {
            if let Some(host) = t.browser.host() {
                host.notify_screen_info_changed();
            }
        }
        tracing::debug!("notify_screen_info_changed: dispatched to all tabs");
    }

    /// Update the device scale factor stored in `OsrViewState` and notify
    /// CEF so it re-queries `screen_info` and `view_rect`.
    ///
    /// Single entry point for live scale changes (monitor switch, DPI change).
    /// The old scale is logged at `debug` level alongside the new value.
    pub fn set_device_scale(&self, scale: f32) {
        let old = self.osr_view.scale();
        tracing::debug!(old_scale = old, new_scale = scale, "set_device_scale");
        self.osr_view.set_scale(scale);
        self.notify_screen_info_changed();
    }

    /// Update the OSR frame rate target for ALL live browsers and
    /// any browsers created later. CEF clamps internally (current
    /// builds cap at 60 fps for windowless rendering); we accept
    /// any positive integer and let CEF decide. No-op in Windowed
    /// mode.
    pub fn set_frame_rate(&self, hz: u32) {
        let hz = hz.max(1);
        self.osr_view.frame_rate_hz.store(hz, Ordering::Relaxed);
        if let Ok(tabs) = self.tabs.lock() {
            for t in tabs.iter() {
                if let Some(host) = t.browser.host() {
                    host.set_windowless_frame_rate(hz as i32);
                }
            }
        }
        tracing::debug!(hz, "set_frame_rate: applied to live browsers");
    }

    /// Force-close every live browser. Called once at app shutdown
    /// before `cef::shutdown()` so CEF's internal teardown doesn't
    /// trip over still-active browsers (segfaults on recent builds
    /// with hardware compositing on). Caller must pump
    /// `cef::do_message_loop_work()` afterwards until OnBeforeClose
    /// fires for each browser.
    pub fn close_all_browsers(&self) {
        let tabs = match self.tabs.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for t in tabs.iter() {
            if let Some(host) = t.browser.host() {
                // Close devtools first so CEF's devtools-close callback fires
                // while the parent browser is still alive.
                if host.has_dev_tools() != 0 {
                    host.close_dev_tools();
                }
                host.close_browser(1);
            }
        }
        // Closed-tab undo stack also holds live browsers (see
        // close_tab — stashable tabs are kept alive for reopen).
        if let Ok(stack) = self.closed_stack.lock() {
            for c in stack.iter() {
                if let Some(host) = c.tab.browser.host() {
                    if host.has_dev_tools() != 0 {
                        host.close_dev_tools();
                    }
                    host.close_browser(1);
                }
            }
        }
        // Popup browsers. Popups are tracked in popup_browsers so close_browser(1)
        // suffices. Note: if OnBeforePopup ever creates a child that bypasses the
        // popup_browsers map, it would also be missed here — tracked as a separate
        // issue, not fixed in this changeset.
        if let Ok(browsers) = self.popup_browsers.lock() {
            for b in browsers.values() {
                if let Some(host) = b.host() {
                    host.close_browser(1);
                }
            }
        }
        tracing::info!("close_all_browsers: dispatched");
    }

    /// Forward a mouse-wheel event to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
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
        self.osr_view.width.store(width, Ordering::Relaxed);
        self.osr_view.height.store(height, Ordering::Relaxed);
        if let Ok(mut last) = self.last_size.lock() {
            *last = (width, height);
        }
        // Mark the cached frame as stale-until-next-paint. Without this,
        // an in-flight on_paint at the OLD dims persists in `osr_frame`;
        // when the user toggles back to those dims via a subsequent
        // `osr_resize`, the embedder's freshness gate would re-accept
        // the stale pixels because frame.dims == osr_view (by chance).
        // Forcing the gate to wait for a real post-resize paint avoids
        // showing the previous toggle's content at correct dimensions.
        if let Ok(mut frame) = self.osr_frame.lock() {
            frame.needs_fresh = true;
        }
        self.notify_was_resized(width, height);
    }

    fn notify_was_resized(&self, width: u32, height: u32) {
        let Ok(tabs) = self.tabs.lock() else {
            tracing::debug!(width, height, "notify_was_resized: tabs mutex poisoned");
            return;
        };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            tracing::debug!(
                width,
                height,
                idx,
                "notify_was_resized: calling was_resized"
            );
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
        } else {
            tracing::debug!(
                width,
                height,
                ?active_idx,
                tab_count = tabs.len(),
                "notify_was_resized: no active browser host",
            );
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
        // OSR everywhere: no parent_window, CEF calls RenderHandler
        // instead of creating a child window. The wgpu present layer
        // composites the OSR frame under buffr's chrome strips.
        window_info.windowless_rendering_enabled = 1;
        tracing::info!(
            bounds_w = window_info.bounds.width,
            bounds_h = window_info.bounds.height,
            windowless_rendering_enabled = window_info.windowless_rendering_enabled,
            runtime_style = ?window_info.runtime_style,
            "create_browser: window_info"
        );

        // Translate `buffr-src:` → `view-source:` for CEF; chrome-side
        // we keep the `buffr-src:` form (see Tab.url assignment below).
        let cef_navigation_url = to_cef_navigation_url(url);
        let cef_url = CefString::from(cef_navigation_url.as_ref());
        let mut settings = BrowserSettings::default();
        // CEF's OSR default is 30 fps — that's the lag floor for mouse
        // wheel scrolling, smooth animations, and video playback. We
        // pass through whatever rate the embedder requested via
        // `set_frame_rate` (defaults to 60; embedder typically sets
        // it to the display refresh rate). CEF clamps internally —
        // current builds cap at 60.
        let hz = self.osr_view.frame_rate_hz.load(Ordering::Relaxed);
        settings.windowless_frame_rate = hz.max(1) as i32;
        // Opaque white background. Default (0) is treated as "fully
        // transparent" for windowless browsers, which leaves pages
        // without a CSS body background (notably `view-source:`)
        // showing the wgpu clear colour through the OSR quad. Match
        // standard browser behaviour and let CEF fill un-painted
        // areas with white.
        settings.background_color = 0xFFFFFFFF;

        let render_handler = Some(make_osr_paint_handler(
            self.osr_frame.clone(),
            self.osr_view.clone(),
            self.popup_frames.clone(),
            self.loading_busy.clone(),
        ));

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
            self.popup_title_sink.clone(),
            self.popup_frames.clone(),
            self.pending_popup_alloc.clone(),
            self.popup_create_sink.clone(),
            self.popup_close_sink.clone(),
            self.popup_browsers_arc(),
            self.cursor_state.clone(),
            self.favicon_sink.clone(),
            self.favicon_enabled.clone(),
            self.loading_busy.clone(),
            self.audio_sink.clone(),
            self.audio_queue.clone(),
            self.video_active.clone(),
            self.context_menu_sink.clone(),
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
            url: to_display_url(url).into_owned(),
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
            // Force a re-paint at the (possibly updated) view_rect dims.
            // Without invalidate, CEF may dedupe a redundant was_resized
            // on a static page → no on_paint follows → embedder's
            // last_osr_dims stays at whatever the previous tab left,
            // which may not match the current browser_w/h.
            host.invalidate(cef::PaintElementType::VIEW);
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

    /// Number of pinned tabs. Equal to the index of the first
    /// unpinned tab in the strip (since pinned tabs always lead).
    /// Apps use this to clamp `o` / `O` insertion so a new unpinned
    /// tab can never land in the pinned region.
    pub fn pinned_count(&self) -> usize {
        self.tabs
            .lock()
            .map(|g| g.iter().filter(|t| t.pinned).count())
            .unwrap_or(0)
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

    /// Aggressive force-repaint: cycle was_hidden(1) → was_hidden(0)
    /// to mimic the tab-switch repaint trigger that bypasses CEF's
    /// invalidate(VIEW) deduplication.
    ///
    /// Used by the embedder's resize-paint watchdog when CEF fails to
    /// emit on_paint at the expected dims within the timeout. Same
    /// call sequence as set_active_index uses for the newly-activated
    /// browser, except it sandwiches a was_hidden(1) → was_hidden(0)
    /// transition instead of relying on the prior tab having been
    /// hidden.
    ///
    /// CEF caveats: the brief was_hidden(1) state can pause some
    /// renderer-side timers. The window is sub-millisecond in practice,
    /// so observable side effects are negligible.
    pub fn force_repaint_active(&self) {
        let Ok(tabs) = self.tabs.lock() else {
            tracing::debug!("force_repaint_active: tabs mutex poisoned");
            return;
        };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            tracing::debug!(idx, "force_repaint_active: was_hidden cycle");
            host.was_hidden(1);
            host.was_hidden(0);
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
        }
    }

    // ── OSR sleep / wake helpers ─────────────────────────────────────────────

    /// Put the active tab's CEF browser to sleep or wake it.
    ///
    /// Sleep path: calls `was_hidden(1)` on the active tab's browser host.
    /// This is the same mechanism tab-switch uses to pause inactive tabs, so
    /// it is well-tested and avoids inventing a parallel pause path.
    ///
    /// **Audio note**: in testing, `was_hidden(1)` does *not* cut audio for
    /// YouTube/Spotify — CEF continues the audio decode pipeline regardless of
    /// the paint-scheduling hint.  If a future regression is found, the fallback
    /// strategy is to call `was_hidden(0)` immediately (keep CEF painting) but
    /// skip the wgpu `surface.acquire_texture` call in the apps layer.  That
    /// fallback is not implemented here; the simpler path is tried first.
    pub fn osr_sleep(&self, sleep: bool) {
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            host.was_hidden(if sleep { 1 } else { 0 });
            tracing::debug!(
                target: "buffr_core::host",
                sleep,
                idx,
                "osr_sleep"
            );
        }
    }

    /// Wake helper: nudge CEF to deliver a fresh paint after waking from
    /// sleep.  Calls `was_resized` then `invalidate(VIEW)` on the active
    /// tab — same sequence as [`Self::set_active_index`] uses for the
    /// newly-activated browser so CEF deduplications are bypassed.
    pub fn osr_invalidate_view(&self) {
        let Ok(tabs) = self.tabs.lock() else { return };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
            && let Some(host) = t.browser.host()
        {
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
            tracing::debug!(
                target: "buffr_core::host",
                idx,
                "osr_invalidate_view"
            );
        }
    }

    // ── Audio-state accessors (for OSR sleep policy) ──────────────────────────

    /// `true` when at least one browser tracked by the audio handler has an
    /// active audio stream.  Cheap snapshot — takes the audio-sink mutex once.
    pub fn any_audio_active(&self) -> bool {
        audio_any_active(&self.audio_sink)
    }

    /// `true` when the last JS media probe reported a *video* signal active
    /// (`window.__buffr_video_active === true`).
    ///
    /// Driven by [`crate::handlers::BuffrDisplayHandler::on_console_message`]:
    /// `media_probe_poll.js` emits a `__buffr_media__:` sentinel line on every
    /// transition, the handler parses it via [`crate::media_probe::parse`],
    /// and flips this atomic. Idle-inhibit policy in `apps/buffr` consults
    /// the flag each tick.
    ///
    /// Note that the flag is process-wide — all browsers share the atomic, so
    /// the value reflects "any tab in any browser has video". For per-tab
    /// reporting we'd need to keep a per-browser-id map (out of scope for
    /// the idle-inhibit use case, which already wants any-active semantics).
    pub fn any_video_active(&self) -> bool {
        self.video_active.load(Ordering::Relaxed)
    }

    /// Drain all pending [`AudioEvent`]s that [`BuffrAudioHandler`] pushed
    /// since the last call.  Returns an empty `Vec` on mutex poison.
    pub fn drain_audio_events(&self) -> Vec<crate::audio::AudioEvent> {
        audio_drain_events(&self.audio_queue)
    }

    /// Drain all pending [`ContextMenuRequest`]s built by
    /// `BuffrContextMenuHandler::run_context_menu` since the last call.
    /// Returns an empty `Vec` on mutex poison.
    pub fn drain_context_menu_requests(&self) -> Vec<ContextMenuRequest> {
        if let Ok(mut q) = self.context_menu_sink.lock() {
            q.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    fn summarize(&self, t: &Tab) -> TabSummary {
        TabSummary {
            id: t.id,
            browser_id: t.browser.identifier(),
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
        // Confine the move to the source tab's region so a pinned
        // drag can't land in the unpinned band (and vice versa).
        let pinned_count = tabs.iter().filter(|t| t.pinned).count();
        let (region_lo, region_hi) = if tabs[from].pinned {
            (0_usize, pinned_count.saturating_sub(1))
        } else {
            (pinned_count, len - 1)
        };
        let to = to.clamp(region_lo, region_hi);
        if to == from {
            return;
        }
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
    /// Repositions the tab so pinned tabs always occupy the leading
    /// slots in the strip.
    pub fn toggle_pin_active(&self) {
        let id = match self.active.lock().ok().and_then(|g| *g) {
            Some(idx) => match self.tabs.lock() {
                Ok(mut tabs) => match tabs.get_mut(idx) {
                    Some(t) => {
                        t.pinned = !t.pinned;
                        t.id
                    }
                    None => return,
                },
                Err(_) => return,
            },
            None => return,
        };
        let _ = id;
        self.enforce_pinned_ordering();
    }

    /// Set the pinned bit on the tab with `id`. Used by session
    /// restore to apply the saved pin state without depending on
    /// which tab is currently active.
    pub fn set_pinned(&self, id: TabId, pinned: bool) {
        if let Ok(mut tabs) = self.tabs.lock()
            && let Some(t) = tabs.iter_mut().find(|t| t.id == id)
        {
            t.pinned = pinned;
        }
        self.enforce_pinned_ordering();
    }

    /// Stable partition: pinned tabs first, unpinned next, relative
    /// order within each group preserved. The active index is
    /// re-resolved against the same `TabId` so the user's focus
    /// follows its tab through the rearrangement.
    fn enforce_pinned_ordering(&self) {
        let active_id = {
            let Ok(active) = self.active.lock() else {
                return;
            };
            let Ok(tabs) = self.tabs.lock() else {
                return;
            };
            (*active).and_then(|i| tabs.get(i).map(|t| t.id))
        };
        let Ok(mut tabs) = self.tabs.lock() else {
            return;
        };
        let drained: Vec<Tab> = tabs.drain(..).collect();
        let (pinned, unpinned): (Vec<Tab>, Vec<Tab>) = drained.into_iter().partition(|t| t.pinned);
        tabs.extend(pinned);
        tabs.extend(unpinned);
        let new_idx = active_id.and_then(|id| tabs.iter().position(|t| t.id == id));
        drop(tabs);
        if let Ok(mut a) = self.active.lock() {
            *a = new_idx;
        }
    }

    /// Update the URL field on the tab whose `Browser::identifier`
    /// matches `cef_id`. Used by the load handler to keep the chrome
    /// in sync. Returns the [`TabId`] of the affected tab, if any.
    pub fn record_url(&self, cef_id: i32, url: &str) -> Option<TabId> {
        let mut tabs = self.tabs.lock().ok()?;
        for t in tabs.iter_mut() {
            if t.browser.identifier() == cef_id {
                if let Some(new_url) = merge_navigation_url(&t.url, url) {
                    t.url = new_url;
                }
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
            let cef_navigation_url = to_cef_navigation_url(trimmed);
            let cef_url = CefString::from(cef_navigation_url.as_ref());
            frame.load_url(Some(&cef_url));
            t.url = to_display_url(trimmed).into_owned();
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
                // Mark user gesture before focusing — edit.js's focusin
                // handler blurs any focus that arrives without a gesture
                // flag, so without this the script would self-cancel.
                self.run_js("window.__buffrUserGesture = true;");
                self.run_js(buffr_core::scripts::FOCUS_FIRST_INPUT);
            }

            A::ExitInsertMode => {
                // Blur whatever the page has focused. The DOM blur event will
                // propagate to edit.js, which posts a `blur` console event;
                // the main loop drains it and resets EditFocus. As a defensive
                // measure, the caller (apps/buffr/src/main.rs's dispatch_action
                // path) should ALSO clear local state synchronously.
                self.run_js(buffr_core::scripts::EXIT_INSERT);
            }

            A::ClearCompletedDownloads => match self.downloads.clear_completed() {
                Ok(n) => tracing::info!(removed = n, "downloads: cleared completed"),
                Err(err) => tracing::warn!(error = %err, "downloads: clear_completed failed"),
            },

            A::YankUrl => {
                self.with_active(|t| {
                    if let Some(frame) = t.browser.main_frame() {
                        let url = CefStringUtf16::from(&frame.url()).to_string();
                        if self.clipboard_set_text(&url) {
                            tracing::debug!(url, "yanked URL to clipboard");
                        } else {
                            tracing::warn!(url, "yank failed: clipboard write failed");
                        }
                    } else {
                        tracing::warn!("YankUrl: main frame unavailable");
                    }
                });
            }

            A::YankSelection => {
                // Ask the page for its current selection. edit.js emits a
                // `selection` console-log sentinel which the apps layer
                // drains and writes through `hjkl-clipboard` so the
                // payload lands on the system clipboard, not Chromium's
                // internal one.
                self.run_main_frame_js(
                    "if (window.__buffrEmitSelection) window.__buffrEmitSelection()",
                    "buffr://yank-selection",
                );
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
        let Some(event) = buffr_core::hint::take_hint_event(&self.hint_sink) else {
            return false;
        };
        match event {
            buffr_core::hint::HintConsoleEvent::Ready { hints, alphabet: _ } => {
                let alphabet = self.hint_alphabet.clone();
                self.with_active(|t| {
                    if let Some(existing) = t.session.hint_session.as_mut() {
                        let background = existing.background;
                        *existing = HintSession::new(alphabet, hints, background);
                    }
                });
                true
            }
            buffr_core::hint::HintConsoleEvent::Error { message } => {
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
    pub fn run_main_frame_js(&self, code: &str, url: &str) {
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
        // Mark user gesture so edit.js's focusin gate doesn't blur the
        // re-focus call. This is invoked from `i` / FocusFirstInput
        // when last_focused_field is set — both are deliberate.
        self.run_main_frame_js(
            &format!(
                "window.__buffrUserGesture = true; \
                 if (window.__buffrEditFocus) window.__buffrEditFocus({escaped_id})"
            ),
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
    /// Clone the clipboard handle so the embedder can do reads on a
    /// worker thread (avoiding the Wayland self-deadlock when Chromium
    /// owns the clipboard and the wl_data_source.send callback runs on
    /// CEF's UI thread, i.e. the main thread).
    ///
    /// Returns an opaque `ClipboardReader` so callers don't pull in
    /// `hjkl-clipboard` directly. The underlying handle is
    /// `Clone + Send + Sync`, so this is a cheap reference clone.
    pub fn clipboard_handle(&self) -> Option<ClipboardReader> {
        self.clipboard.clone().map(ClipboardReader)
    }

    /// Is a main-frame navigation currently in flight?
    ///
    /// Set by `BuffrLoadHandler::on_load_start` and cleared by the
    /// next successful `OsrPaintHandler::on_paint`. The apps layer
    /// uses this to keep the loading anim playing across the
    /// navigation gap until the first contentful frame commits.
    pub fn is_loading(&self) -> bool {
        self.loading_busy.load(Ordering::Relaxed)
    }

    pub fn clipboard_text(&self) -> Option<String> {
        use hjkl_clipboard::{MimeType, Selection};
        let cb = self.clipboard.as_ref()?;
        match cb.get(Selection::Clipboard, MimeType::Text) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) if !s.is_empty() => Some(s),
                Ok(_) => None,
                Err(err) => {
                    tracing::debug!(error = %err, "clipboard_text: non-utf8 payload");
                    None
                }
            },
            Err(err) => {
                tracing::debug!(error = %err, "clipboard_text: read failed");
                None
            }
        }
    }

    /// Write `text` to the system clipboard via `hjkl-clipboard`.
    /// Returns `true` on success. Used by both YankUrl and YankSelection
    /// so both yanks land on the same `+` register rather than
    /// Chromium's internal one.
    pub fn clipboard_set_text(&self, text: &str) -> bool {
        use hjkl_clipboard::{MimeType, Selection};
        let Some(cb) = self.clipboard.as_ref() else {
            return false;
        };
        match cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(error = %err, "clipboard_set_text: write failed");
                false
            }
        }
    }

    /// Spawn an off-thread worker that fetches `url`, decodes the
    /// image, re-encodes it as PNG, and writes it to the system
    /// clipboard. Used by the right-click "Copy Image" item. Returns
    /// immediately; outcome surfaces via `tracing` (see [`image_copy`]).
    pub fn copy_image_url_to_clipboard(&self, url: &str) {
        buffr_core::image_copy::copy_image_to_clipboard(url.to_string());
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

    // ── Per-frame edit ops (context-menu slice 4) ─────────────────────────────
    //
    // CEF's `cef_frame_t` edit methods operate on the focused frame.
    // We use `browser.focused_frame()` with a fallback to `main_frame()`.
    // Each helper is named after the CEF method it wraps.

    fn with_focused_frame<R>(&self, f: impl FnOnce(cef::Frame) -> R) -> Option<R> {
        self.with_active(|t| {
            let frame = t
                .browser
                .focused_frame()
                .or_else(|| t.browser.main_frame())?;
            Some(f(frame))
        })?
    }

    /// Send Undo to the active tab's focused frame.
    pub fn frame_undo(&self) {
        if self.with_focused_frame(|f| f.undo()).is_none() {
            warn!("frame_undo: no focused frame");
        }
    }

    /// Send Redo to the active tab's focused frame.
    pub fn frame_redo(&self) {
        if self.with_focused_frame(|f| f.redo()).is_none() {
            warn!("frame_redo: no focused frame");
        }
    }

    /// Send Cut to the active tab's focused frame.
    pub fn frame_cut(&self) {
        if self.with_focused_frame(|f| f.cut()).is_none() {
            warn!("frame_cut: no focused frame");
        }
    }

    /// Send Copy to the active tab's focused frame.
    pub fn frame_copy(&self) {
        if self.with_focused_frame(|f| f.copy()).is_none() {
            warn!("frame_copy: no focused frame");
        }
    }

    /// Send Paste to the active tab's focused frame.
    pub fn frame_paste(&self) {
        if self.with_focused_frame(|f| f.paste()).is_none() {
            warn!("frame_paste: no focused frame");
        }
    }

    /// Send Paste-as-plain-text to the active tab's focused frame.
    pub fn frame_paste_plain(&self) {
        if self
            .with_focused_frame(|f| f.paste_and_match_style())
            .is_none()
        {
            warn!("frame_paste_plain: no focused frame");
        }
    }

    /// Send Delete to the active tab's focused frame.
    pub fn frame_del(&self) {
        if self.with_focused_frame(|f| f.del()).is_none() {
            warn!("frame_del: no focused frame");
        }
    }

    /// Send SelectAll to the active tab's focused frame.
    pub fn frame_select_all(&self) {
        if self.with_focused_frame(|f| f.select_all()).is_none() {
            warn!("frame_select_all: no focused frame");
        }
    }

    /// Reload the active tab ignoring cache.
    pub fn reload_ignore_cache_active(&self) {
        self.with_active(|t| t.browser.reload_ignore_cache());
    }

    /// Print the active tab (opens system print dialog).
    pub fn print_active(&self) {
        self.with_active(|t| {
            if let Some(host) = t.browser.host() {
                host.print();
            } else {
                warn!("print_active: browser.host() returned None");
            }
        });
    }

    /// Trigger a file-as-download for `url` via CEF's download manager.
    pub fn start_download(&self, url: &str) {
        self.with_active(|t| {
            if let Some(host) = t.browser.host() {
                let cef_url = CefString::from(url);
                host.start_download(Some(&cef_url));
            } else {
                warn!("start_download: browser.host() returned None");
            }
        });
    }

    /// Open DevTools for the active tab, with an optional inspect-element
    /// hit-point in browser-local pixel coords. Pass `None` to open without
    /// a specific element.
    pub fn show_dev_tools_at(&self, x: Option<i32>, y: Option<i32>) {
        self.with_active(|t| {
            let Some(host) = t.browser.host() else {
                warn!("show_dev_tools_at: browser.host() returned None");
                return;
            };
            let window_info = WindowInfo::default();
            let settings = BrowserSettings::default();
            let point = x.zip(y).map(|(px, py)| cef::Point { x: px, y: py });
            host.show_dev_tools(Some(&window_info), None, Some(&settings), point.as_ref());
        });
    }

    /// Fire the JS media-activity poll against the active tab's main frame.
    ///
    /// The poll script (see [`buffr_core::scripts::MEDIA_PROBE_POLL_JS`]) reads all
    /// five signal sources and writes a boolean result to
    /// `window.__buffr_media_active`.  A follow-up JS snippet can read it
    /// back on the next tick via [`Self::read_media_probe_result`].
    ///
    /// The patched-constructor signals (signals 3–5) only work after the init
    /// script ([`buffr_core::scripts::MEDIA_PROBE_INIT_JS`]) has been injected at
    /// page-load time.  If it hasn't run yet the poll degrades gracefully to
    /// the v1 signals (mediaSession + fullscreen video).
    ///
    /// ## Why fire-and-forget?
    ///
    /// `cef::Frame::execute_java_script` does not expose a return-value path —
    /// the cef-rs binding has no `StringVisitor` overload for `execute_java_script`
    /// (only `get_text`/`get_source` carry a visitor callback).  A full
    /// return-value bridge would require a custom scheme + IPC round-trip,
    /// which is out of scope for phase-1.  The two-call window-property
    /// approach is cheap and correct: probe writes, reader reads one tick later.
    pub fn run_media_probe(&self) {
        self.run_js(buffr_core::scripts::MEDIA_PROBE_POLL_JS);
    }

    /// Read the result written by the last [`Self::run_media_probe`] call.
    ///
    /// Executes a tiny inline JS snippet that reads `window.__buffr_media_active`
    /// (a boolean written by the probe) and pushes the result via the audio
    /// event queue as a synthetic entry so the UI thread can consume it with
    /// the same `drain_audio_events` path.
    ///
    /// Currently not implemented: cef-rs `execute_java_script` has no
    /// synchronous return-value mechanism and the async visitor path would
    /// require a custom scheme.  Phase-1 relies solely on the CEF
    /// `AudioHandler` for media detection; this method is a placeholder.
    /// See `MEDIA_PROBE_JS` doc comment for the rationale.
    pub fn read_media_probe_result(&self) {
        // Phase-1: no-op. Detection relies on BuffrAudioHandler alone.
        // Phase-2 can implement a custom-scheme IPC to read
        // window.__buffr_media_active from the JS side.
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

    // ── Context-menu JS injection helpers (slice 4b) ──────────────────────────
    //
    // Each helper builds a self-executing JS snippet and fires it via
    // `run_main_frame_js`. Coordinates come from the `ContextMenuRequest`
    // (CEF browser-local pixels == CSS pixels). serde_json encodes integers
    // into JS literals — defense in depth even though coords are ours.

    /// Toggle `.paused` on the `<video>`/`<audio>` element under `(x, y)`.
    pub fn media_play_pause(&self, x: i32, y: i32) {
        let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
        let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               if(el.paused)el.play();else el.pause();\
             }})({x},{y});"
        );
        self.run_main_frame_js(&js, "buffr://context-menu");
    }

    /// Toggle `.muted` on the media element under `(x, y)`.
    pub fn media_toggle_mute(&self, x: i32, y: i32) {
        let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
        let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.muted=!el.muted;\
             }})({x},{y});"
        );
        self.run_main_frame_js(&js, "buffr://context-menu");
    }

    /// Toggle `.loop` on the media element under `(x, y)`.
    pub fn media_toggle_loop(&self, x: i32, y: i32) {
        let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
        let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.loop=!el.loop;\
             }})({x},{y});"
        );
        self.run_main_frame_js(&js, "buffr://context-menu");
    }

    /// Toggle `.controls` on the media element under `(x, y)`.
    pub fn media_toggle_controls(&self, x: i32, y: i32) {
        let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
        let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.controls=!el.controls;\
             }})({x},{y});"
        );
        self.run_main_frame_js(&js, "buffr://context-menu");
    }

    /// Request Picture-in-Picture for the media element under `(x, y)`,
    /// or exit PiP if it's already the PiP element. Wrapped in try/catch
    /// because some hosts disable PiP.
    pub fn media_picture_in_picture(&self, x: i32, y: i32) {
        let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
        let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
        let js = format!(
            "(function(x,y){{\
               try{{\
                 var el=document.elementFromPoint(x,y);\
                 while(el&&!(el instanceof HTMLVideoElement))el=el.parentElement;\
                 if(!el)el=document.querySelector('video');\
                 if(!el)return;\
                 if(document.pictureInPictureElement===el){{\
                   document.exitPictureInPicture();\
                 }}else{{\
                   el.requestPictureInPicture();\
                 }}\
               }}catch(e){{}}\
             }})({x},{y});"
        );
        self.run_main_frame_js(&js, "buffr://context-menu");
    }

    /// Rotate the `<img>` element under `(x, y)` by `delta_deg` degrees
    /// (positive = clockwise, negative = counter-clockwise). Accumulates
    /// into `el.dataset.buffrRotate` so successive rotations compose.
    pub fn image_rotate(&self, x: i32, y: i32, delta_deg: i32) {
        let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
        let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
        let delta = serde_json::to_string(&delta_deg).unwrap_or_else(|_| "0".to_string());
        let js = format!(
            "(function(x,y,delta){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLImageElement))el=el.parentElement;\
               if(!el)return;\
               var cur=parseInt(el.dataset.buffrRotate||'0',10);\
               cur=(cur+delta)%360;\
               if(cur<0)cur+=360;\
               el.dataset.buffrRotate=String(cur);\
               el.style.transform='rotate('+cur+'deg)';\
             }})({x},{y},{delta});"
        );
        self.run_main_frame_js(&js, "buffr://context-menu");
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
    fn cef_navigation_is_identity() {
        // `buffr-src:` URLs now reach CEF unmodified — the custom scheme
        // handler intercepts them. `to_cef_navigation_url` is a pass-through.
        assert_eq!(
            to_cef_navigation_url("buffr-src:https://example.com").as_ref(),
            "buffr-src:https://example.com"
        );
        assert_eq!(
            to_cef_navigation_url("https://example.com").as_ref(),
            "https://example.com"
        );
        assert_eq!(to_cef_navigation_url("").as_ref(), "");
    }

    #[test]
    fn display_url_translates_view_source_prefix() {
        // Safety-net normalization: old session files or context-menu paths
        // may produce `view-source:` URLs; rewrite them to `buffr-src:`.
        assert_eq!(
            to_display_url("view-source:https://example.com").as_ref(),
            "buffr-src:https://example.com"
        );
        assert_eq!(
            to_display_url("https://example.com").as_ref(),
            "https://example.com"
        );
        // `buffr-src:` URLs are already in user-facing form — pass through.
        assert_eq!(
            to_display_url("buffr-src:https://example.com").as_ref(),
            "buffr-src:https://example.com"
        );
    }

    #[test]
    fn merge_normalizes_view_source_to_buffr_src() {
        // Defensive: raw `view-source:` leaking in from old session or
        // context-menu → normalize to `buffr-src:`.
        assert_eq!(
            merge_navigation_url("https://old.com", "view-source:https://new.com"),
            Some("buffr-src:https://new.com".to_string())
        );
    }

    #[test]
    fn merge_buffr_src_address_change_no_op() {
        // CEF now emits the `buffr-src:` URL unchanged in address-change
        // events (the custom scheme handler owns the URL end-to-end).
        assert_eq!(
            merge_navigation_url("buffr-src:https://x.com", "buffr-src:https://x.com"),
            None
        );
    }

    #[test]
    fn merge_replaces_on_real_navigation_away() {
        assert_eq!(
            merge_navigation_url("buffr-src:https://x.com", "https://other.com"),
            Some("https://other.com".to_string())
        );
    }

    #[test]
    fn merge_no_op_when_unchanged() {
        assert_eq!(merge_navigation_url("https://x.com", "https://x.com"), None);
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
            browser_id: 0,
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

    // ── JS-snippet builder tests (no CEF runtime required) ────────────────
    //
    // These tests verify that the helpers produce JS strings containing the
    // correct structural elements. They do NOT invoke CEF.

    /// Build the play-pause snippet for the given coords and return it.
    fn build_media_play_pause_js(x: i32, y: i32) -> String {
        let x = serde_json::to_string(&x).unwrap();
        let y = serde_json::to_string(&y).unwrap();
        format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               if(el.paused)el.play();else el.pause();\
             }})({x},{y});"
        )
    }

    fn build_media_mute_js(x: i32, y: i32) -> String {
        let x = serde_json::to_string(&x).unwrap();
        let y = serde_json::to_string(&y).unwrap();
        format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.muted=!el.muted;\
             }})({x},{y});"
        )
    }

    fn build_media_loop_js(x: i32, y: i32) -> String {
        let x = serde_json::to_string(&x).unwrap();
        let y = serde_json::to_string(&y).unwrap();
        format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.loop=!el.loop;\
             }})({x},{y});"
        )
    }

    fn build_media_controls_js(x: i32, y: i32) -> String {
        let x = serde_json::to_string(&x).unwrap();
        let y = serde_json::to_string(&y).unwrap();
        format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.controls=!el.controls;\
             }})({x},{y});"
        )
    }

    fn build_pip_js(x: i32, y: i32) -> String {
        let x = serde_json::to_string(&x).unwrap();
        let y = serde_json::to_string(&y).unwrap();
        format!(
            "(function(x,y){{\
               try{{\
                 var el=document.elementFromPoint(x,y);\
                 while(el&&!(el instanceof HTMLVideoElement))el=el.parentElement;\
                 if(!el)el=document.querySelector('video');\
                 if(!el)return;\
                 if(document.pictureInPictureElement===el){{\
                   document.exitPictureInPicture();\
                 }}else{{\
                   el.requestPictureInPicture();\
                 }}\
               }}catch(e){{}}\
             }})({x},{y});"
        )
    }

    fn build_image_rotate_js(x: i32, y: i32, delta_deg: i32) -> String {
        let x = serde_json::to_string(&x).unwrap();
        let y = serde_json::to_string(&y).unwrap();
        let delta = serde_json::to_string(&delta_deg).unwrap();
        format!(
            "(function(x,y,delta){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLImageElement))el=el.parentElement;\
               if(!el)return;\
               var cur=parseInt(el.dataset.buffrRotate||'0',10);\
               cur=(cur+delta)%360;\
               if(cur<0)cur+=360;\
               el.dataset.buffrRotate=String(cur);\
               el.style.transform='rotate('+cur+'deg)';\
             }})({x},{y},{delta});"
        )
    }

    #[test]
    fn media_play_pause_js_contains_coords_and_toggle() {
        let js = build_media_play_pause_js(100, 200);
        assert!(js.contains("100"), "x coord missing");
        assert!(js.contains("200"), "y coord missing");
        assert!(js.contains("el.paused"), "paused check missing");
        assert!(js.contains("el.play()"), "play() call missing");
        assert!(js.contains("el.pause()"), "pause() call missing");
        assert!(js.contains("HTMLMediaElement"), "media type guard missing");
        assert!(
            js.contains("querySelector('video, audio')"),
            "overlay-sibling fallback missing"
        );
    }

    #[test]
    fn media_mute_js_contains_coords_and_toggle() {
        let js = build_media_mute_js(10, 20);
        assert!(js.contains("10"), "x coord missing");
        assert!(js.contains("20"), "y coord missing");
        assert!(js.contains("el.muted=!el.muted"), "mute toggle missing");
        assert!(js.contains("HTMLMediaElement"), "media type guard missing");
        assert!(
            js.contains("querySelector('video, audio')"),
            "overlay-sibling fallback missing"
        );
    }

    #[test]
    fn media_loop_js_contains_toggle() {
        let js = build_media_loop_js(0, 0);
        assert!(js.contains("el.loop=!el.loop"), "loop toggle missing");
        assert!(js.contains("HTMLMediaElement"), "media type guard missing");
        assert!(
            js.contains("querySelector('video, audio')"),
            "overlay-sibling fallback missing"
        );
    }

    #[test]
    fn media_controls_js_contains_toggle() {
        let js = build_media_controls_js(0, 0);
        assert!(
            js.contains("el.controls=!el.controls"),
            "controls toggle missing"
        );
        assert!(js.contains("HTMLMediaElement"), "media type guard missing");
        assert!(
            js.contains("querySelector('video, audio')"),
            "overlay-sibling fallback missing"
        );
    }

    #[test]
    fn pip_js_contains_pip_api_and_try_catch() {
        let js = build_pip_js(50, 60);
        assert!(js.contains("pictureInPictureElement"), "PiP check missing");
        assert!(js.contains("requestPictureInPicture"), "requestPiP missing");
        assert!(js.contains("exitPictureInPicture"), "exitPiP missing");
        assert!(js.contains("try{"), "try/catch missing");
        assert!(js.contains("HTMLVideoElement"), "video type guard missing");
        assert!(
            js.contains("querySelector('video')"),
            "overlay-sibling fallback missing"
        );
    }

    #[test]
    fn image_rotate_js_clockwise_90() {
        let js = build_image_rotate_js(30, 40, 90);
        assert!(js.contains("90"), "delta missing");
        assert!(js.contains("buffrRotate"), "dataset key missing");
        assert!(js.contains("rotate("), "transform missing");
        assert!(js.contains("HTMLImageElement"), "image type guard missing");
    }

    #[test]
    fn image_rotate_js_counterclockwise_handles_negative() {
        let js = build_image_rotate_js(0, 0, -90);
        assert!(js.contains("-90"), "negative delta missing");
        assert!(
            js.contains("if(cur<0)cur+=360"),
            "negative wrap-around missing"
        );
    }
}
