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
//! `set_focus(true)` on the next. See `docs/site/multi-tab.md`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use buffr_engine::internal_server::InternalServer;

use buffr_config::DownloadsConfig;
use buffr_downloads::Downloads;
use buffr_history::History;
use buffr_permissions::Permissions;
use buffr_zoom::ZoomStore;
use std::path::Path;

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
    OsrFrame, OsrViewState, PendingPopupAllocQueue, PopupFrameMap, SharedOsrFrame,
    SharedOsrViewState, make_osr_paint_handler, new_pending_popup_alloc_queue,
};
use crate::{
    PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
    new_popup_queue,
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
    DEFAULT_HINT_SELECTORS, HintAlphabet, HintEventSink, HintSession, build_inject_script,
};
use buffr_core::telemetry::{KEY_TABS_OPENED, UsageCounters};
use buffr_engine::{HintAction, HintStatus};

// `HostMode` removed: every platform now runs OSR. CEF paints into a
// shared bitmap, the wgpu present layer composites it under buffr's
// chrome strips. Windows previously embedded a CEF child HWND;
// migrated to OSR so platform behavior is uniform (cursor sync, IME
// integration, scale-factor changes, MSI footprint).

/// Per-tab UI state preserved across tab switches. Find query and hint
/// session restore on focus.
#[derive(Debug, Default, Clone)]
pub struct TabSession {
    pub find_query: Option<String>,
    pub hint_session: Option<HintSession>,
}

// Bring neutral TabId + TabSummary into scope. These replace the
// previously-local definitions; all host.rs code uses them via this import.
use buffr_engine::{TabId, TabSummary};

/// User-facing prefix for "view source" navigations. `buffr-src:` URLs
/// are now served by the custom CEF scheme handler in
/// `view_source_scheme.rs` — no rewrite to `view-source:` at the
/// navigation boundary. This const is kept public because
/// `apps/buffr-app/src/main.rs` constructs `buffr-src:<url>` literals
/// when dispatching "view page source" from the context menu.
pub const BUFFR_SRC_PREFIX: &str = "buffr-src:";

/// Identity translation kept for the unit-test in this file. Production
/// code uses `BrowserHost::cef_navigation_url` which rewrite `buffr://`
/// URLs to the InternalServer HTTP loopback when a server is attached.
#[cfg(test)]
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

/// Correct the stored active index after the tab at `removed_idx` is
/// removed: the active tab shifts down by one when the closed tab sat
/// before it, otherwise the index is unchanged. This is what keeps
/// `set_active_index`'s hide-previous guard seeing the real active tab
/// (backlog §10 item 1 — closing a tab left of the active one used to
/// leave the old active browser running as if foregrounded).
fn active_after_close(active: Option<usize>, removed_idx: usize) -> Option<usize> {
    match active {
        Some(prev) if removed_idx < prev => Some(prev - 1),
        other => other,
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
    pending_popup_alloc: PendingPopupAllocQueue,
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
    /// Per-browser nonces authenticating the renderer → browser
    /// console-log IPC (hint / edit / media probe).
    ///
    /// `BuffrLoadHandler::on_load_end` rotates the page nonce on every
    /// main-frame load and splices it into `edit.js`;
    /// [`Self::enter_hint_mode`] rotates the hint nonce per session and
    /// splices it into `hint.js`; [`Self::run_media_probe`] splices the
    /// current page nonce into the poll script. The display handler drops
    /// any sentinel line that does not carry the matching value, which is
    /// what stops an arbitrary frame from driving buffr's internals. See
    /// [`buffr_core::console_nonce`] for the threat model.
    console_nonces: buffr_core::console_nonce::ConsoleNonces,
    /// Queue of right-click events translated into [`ContextMenuRequest`]s
    /// by `BuffrContextMenuHandler::run_context_menu`. The apps layer
    /// drains these each tick via [`Self::drain_context_menu_requests`].
    context_menu_sink: ContextMenuSink,
    /// Per-engine CEF request context. `Some` when this engine was opened
    /// with a `data_dir` — CEF isolates cookies, cache, local-storage,
    /// and IndexedDB per context. `None` uses CEF's process-global default
    /// (backward-compatible for single-engine configs with no `data_dir`).
    ///
    /// Wrapped in `Mutex` because `browser_host_create_browser_sync` takes
    /// `Option<&mut RequestContext>` and `create_browser` is called via
    /// `&self` (the `BrowserEngine` trait does not grant `&mut self`).
    request_context: Mutex<Option<cef::RequestContext>>,
    /// Neutral permissions queue (Phase 8a, #88). Mirrors the CEF-internal
    /// `permissions_queue` entries in backend-neutral form so the apps layer
    /// can use `BrowserEngine::permissions_queue()` without touching CEF types.
    neutral_permissions_queue: buffr_engine::PermissionsQueue,
    /// Registry mapping `resolve_id → CefPendingPermission` (Phase 8a, #88).
    /// Written on the CEF IO thread when a permission request arrives;
    /// read + drained by `BrowserEngine::resolve_permission` on the UI thread.
    cef_callback_registry: crate::permissions::CefCallbackRegistry,
    /// Shared loopback HTTP server for `buffr://*` internal pages. When
    /// `Some`, `buffr://new` and friends are rewritten to
    /// `http://127.0.0.1:<port>/<token>/<path>` before CEF sees them —
    /// eliminating `ERR_UNKNOWN_URL_SCHEME` without a custom scheme handler.
    /// Set via [`Self::set_internal_server`] after construction.
    internal_server: Mutex<Option<Arc<InternalServer>>>,
    /// Display URL overrides keyed by [`TabId`]. When a tab was opened with
    /// a `buffr://` URL the actual navigation goes to the InternalServer HTTP
    /// URL; we remember the original `buffr://` form here so the omnibar
    /// shows `buffr://new` instead of `http://127.0.0.1:…/…`.
    display_urls: Mutex<HashMap<TabId, String>>,
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
    /// The tab's `display_urls` override at close time, if it had one.
    ///
    /// M19: `close_index` used to call `forget_display_url` unconditionally
    /// even though the stashable branch keeps the `Tab` alive on this stack.
    /// After `reopen_closed_tab` the override was gone, so the omnibar — and
    /// the session file — showed the raw loopback `InternalServer` URL
    /// *including its auth token* for what the user knows as `buffr://new`.
    /// Carrying it here lets reopen restore the friendly form.
    display_url: Option<String>,
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

/// `ClipboardReader` satisfies the engine-agnostic [`buffr_engine::ClipboardRead`]
/// trait so the apps layer can accept it as `Arc<dyn ClipboardRead>` without
/// depending on `hjkl_clipboard` directly.
impl buffr_engine::ClipboardRead for ClipboardReader {
    fn read_text(&self) -> Option<String> {
        self.read_text()
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
            notice_queue,
            find_sink,
            hint_sink,
            edit_sink,
            hint_alphabet,
            initial_size,
            false,
            None,
            true,
            None,
            None,
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
        notice_queue: DownloadNoticeQueue,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        edit_sink: EditEventSink,
        hint_alphabet: HintAlphabet,
        initial_size: (u32, u32),
        private: bool,
        counters: Option<Arc<UsageCounters>>,
        show_favicons: bool,
        data_dir: Option<&Path>,
        internal_server: Option<Arc<InternalServer>>,
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

        // Per-engine `CefRequestContext` is disabled — CEF Alloy runtime
        // collapses any child `cache_path` to the global `Default/` profile
        // anyway (see kryptic-sh/buffr#158). Creating the context also
        // somehow blocks cookie persistence even after the global context
        // works fine standalone. Drop until Alloy is replaced with Chrome
        // runtime OR per-engine isolation is genuinely needed.
        let _ = data_dir; // intentionally unused
        let request_context: Option<cef::RequestContext> = None;

        let popup_queue = new_popup_queue();
        let address_sink: AddressSink = Arc::new(Mutex::new(VecDeque::new()));
        let popup_frames: PopupFrameMap = Arc::new(Mutex::new(HashMap::new()));
        let popup_create_sink = new_popup_create_sink();
        let popup_close_sink = new_popup_close_sink();
        let pending_popup_alloc = new_pending_popup_alloc_queue();
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
            console_nonces: buffr_core::console_nonce::ConsoleNonces::new(),
            context_menu_sink: new_context_menu_sink(),
            request_context: Mutex::new(request_context),
            neutral_permissions_queue: buffr_engine::permissions::new_queue(),
            cef_callback_registry: Arc::new(Mutex::new(std::collections::HashMap::new())),
            internal_server: Mutex::new(internal_server),
            display_urls: Mutex::new(HashMap::new()),
        };
        host.open_tab(url)?;
        Ok(host)
    }

    /// Attach a shared [`InternalServer`] so future `buffr://*` navigations
    /// resolve to authenticated localhost HTTP URLs. Idempotent; later calls
    /// replace the previous server.
    pub fn set_internal_server(&self, server: Arc<InternalServer>) {
        if let Ok(mut guard) = self.internal_server.lock() {
            *guard = Some(server);
        }
    }

    /// Translate a user-facing URL into the form CEF should actually navigate
    /// to. `buffr-src:` URLs pass through unchanged (handled by the custom
    /// scheme handler). `buffr://` URLs are rewritten to the InternalServer
    /// HTTP loopback when one is attached; otherwise passed through so CEF
    /// fails visibly rather than masking the error.
    fn cef_navigation_url<'a>(&self, url: &'a str) -> std::borrow::Cow<'a, str> {
        if let Some(rest) = url.strip_prefix("buffr://")
            && let Ok(guard) = self.internal_server.lock()
            && let Some(server) = guard.as_ref()
        {
            return std::borrow::Cow::Owned(server.url_for(&format!("/{rest}")));
        }
        std::borrow::Cow::Borrowed(url)
    }

    /// Record that `tab_id` was navigated with the user-facing URL `original`.
    /// Lets the omnibar show `buffr://new` instead of the http://127.0.0.1:…
    /// URL that CEF actually loaded.
    fn record_display_url(&self, tab_id: TabId, original: &str) {
        if let Ok(mut guard) = self.display_urls.lock() {
            guard.insert(tab_id, original.to_owned());
        }
    }

    fn forget_display_url(&self, tab_id: TabId) {
        if let Ok(mut guard) = self.display_urls.lock() {
            guard.remove(&tab_id);
        }
    }

    fn display_url_for(&self, tab_id: TabId) -> Option<String> {
        self.display_urls.lock().ok()?.get(&tab_id).cloned()
    }

    /// Borrow the shared permissions store. The UI thread uses this to
    /// persist user-chosen "always" decisions when resolving a queued
    /// prompt.
    pub fn permissions(&self) -> &Arc<Permissions> {
        &self.permissions
    }

    /// Clone the popup URL queue. The main loop drains it each tick
    /// and calls [`Self::open_tab`] for `target=_blank` / Ctrl+click
    /// intents that `on_before_popup` re-routed.
    pub fn popup_queue(&self) -> PopupQueue {
        self.popup_queue.clone()
    }

    /// Returns the user-facing URL of the active tab. For `buffr://` tabs the
    /// original `buffr://new` form is returned (from `display_urls`) rather
    /// than the `http://127.0.0.1:…` URL that CEF actually navigated to.
    /// Updated by `pump_address_changes` whenever CEF fires `on_address_change`.
    /// Empty string if no active tab.
    pub fn active_tab_live_url(&self) -> String {
        let Ok(tabs) = self.tabs.lock() else {
            return String::new();
        };
        let active_idx = self.active.lock().ok().and_then(|g| *g);
        if let Some(idx) = active_idx
            && let Some(t) = tabs.get(idx)
        {
            // Prefer the display-URL override (set for `buffr://` navigations)
            // so the omnibar shows `buffr://new` rather than http://127.0.0.1:…
            self.display_url_for(t.id).unwrap_or_else(|| t.url.clone())
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
        self.with_active_host(|host| host.zoom_level())
            .unwrap_or(0.0)
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
        self.with_popup_host(browser_id, |host| {
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
        });
        tracing::debug!(browser_id, width, height, "popup_resize");
    }

    /// Navigate the popup browser back in its own history.
    pub fn popup_history_back(&self, browser_id: i32) {
        self.with_popup_browser(browser_id, |b| b.go_back());
    }

    /// Navigate the popup browser forward in its own history.
    pub fn popup_history_forward(&self, browser_id: i32) {
        self.with_popup_browser(browser_id, |b| b.go_forward());
    }

    /// Request CEF to close a popup browser. The actual teardown is
    /// asynchronous — `on_before_close` → handler deregisters →
    /// `PopupCloseSink` is the cleanup path.
    pub fn popup_close(&self, browser_id: i32) {
        self.with_popup_host(browser_id, |host| host.close_browser(0));
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
        self.with_popup_host(browser_id, |host| {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_move_event(Some(&event), 0);
        });
    }

    /// Forward a mouse-click to a popup browser.
    #[allow(clippy::too_many_arguments)]
    pub fn popup_osr_mouse_click(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        button: buffr_engine::MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        self.with_popup_host(browser_id, |host| {
            let cef_button = crate::convert::neutral_to_cef_button(button);
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_click_event(Some(&event), cef_button, mouse_up as i32, click_count);
        });
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
        self.with_popup_host(browser_id, |host| {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
        });
    }

    /// Forward a keyboard event to a popup browser.
    pub fn popup_osr_key_event(&self, browser_id: i32, event: buffr_engine::NeutralKeyEvent) {
        self.with_popup_host(browser_id, |host| {
            let cef_ev = crate::convert::neutral_to_cef_key(event);
            host.send_key_event(Some(&cef_ev));
        });
    }

    /// Set focus on a popup browser.
    pub fn popup_osr_focus(&self, browser_id: i32, focused: bool) {
        self.with_popup_host(browser_id, |host| {
            host.set_focus(if focused { 1 } else { 0 });
        });
    }

    // ---- OSR input forwarding -------------------------------------------

    /// Forward a mouse-move to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode (native child window routes input).
    pub fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        self.with_active_host(|host| {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_move_event(Some(&event), 0);
        });
    }

    /// Forward a mouse-click to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: buffr_engine::MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        let cef_button = crate::convert::neutral_to_cef_button(button);
        tracing::debug!(
            target: "buffr_core::host",
            x, y, ?cef_button, mouse_up, click_count, modifiers,
            "osr_mouse_click"
        );
        let sent = self.with_active_host(|host| {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_click_event(Some(&event), cef_button, mouse_up as i32, click_count);
        });
        if sent.is_none() {
            warn!(target: "buffr_core::host", "osr_mouse_click: no active browser host — click dropped");
        }
    }

    /// Notify CEF the mouse left the window.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_mouse_leave(&self, modifiers: u32) {
        self.with_active_host(|host| {
            let event = cef::MouseEvent {
                x: 0,
                y: 0,
                modifiers,
            };
            host.send_mouse_move_event(Some(&event), 1);
        });
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
        // C8: popups keep their own OsrViewState, which resolve_scale
        // reads when CEF asks for a popup's screen_info. Keep them in
        // step with the main view so a live scale change (monitor
        // switch, DPI change) doesn't leave popups rendering at the old
        // factor.
        if let Ok(map) = self.popup_frames.lock() {
            for (_frame, view) in map.values() {
                view.set_scale(scale);
            }
        }
        if let Ok(browsers) = self.popup_browsers.lock() {
            for browser in browsers.values() {
                if let Some(host) = browser.host() {
                    host.notify_screen_info_changed();
                }
            }
        }
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
        // Retire any permission requests still awaiting a UI decision while
        // CEF's threads are still running. Without this the registry `Arc`
        // is only released when the last CEF-held client ref drops, which
        // happens somewhere inside `cef::shutdown()` — far too late to be
        // invoking C++ callbacks (L16 wiring for
        // `drain_registry_with_defer`).
        crate::permissions::drain_registry_with_defer(
            &self.cef_callback_registry,
            &self.permissions,
        );
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
        self.with_active_host(|host| {
            let event = cef::MouseEvent { x, y, modifiers };
            host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
        });
    }

    /// Forward a keyboard event to the active tab's browser host.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_key_event(&self, event: buffr_engine::NeutralKeyEvent) {
        let cef_ev = crate::convert::neutral_to_cef_key(event);
        self.with_active_host(|host| host.send_key_event(Some(&cef_ev)));
    }

    /// Notify CEF of focus changes.
    ///
    /// No-op when the host is in `Windowed` mode.
    pub fn osr_focus(&self, focused: bool) {
        self.with_active_host(|host| host.set_focus(if focused { 1 } else { 0 }));
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
        let notified = self.with_active_host_idx(|host, idx| {
            tracing::debug!(
                width,
                height,
                idx,
                "notify_was_resized: calling was_resized"
            );
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
        });
        if notified.is_none() {
            tracing::debug!(width, height, "notify_was_resized: no active browser host",);
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
        // Restore the display-URL override before the tab becomes visible,
        // so the first `summarize` / `active_tab_live_url` after reopen
        // already reports `buffr://new` rather than the tokenised loopback
        // URL `pump_address_changes` wrote into `Tab::url` (M19).
        if let Some(display_url) = entry.display_url.as_deref() {
            self.record_display_url(id, display_url);
        }
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
        // C7: a restored tab can sit outside the pinned block (its saved index
        // predates later pin changes), breaking the pinned-first invariant the
        // tab-strip hit-testing relies on. Re-partition like toggle_pin_active /
        // set_pinned; active follows the restored tab by id.
        self.enforce_pinned_ordering();
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

        // Translate `buffr://` → InternalServer HTTP URL for CEF; the
        // user-facing form is preserved in `display_urls` (via
        // `record_display_url`) so the omnibar shows `buffr://new`.
        // `buffr-src:` passes through unchanged — handled by the custom
        // scheme handler.
        let cef_navigation_url = self.cef_navigation_url(url);
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
            self.neutral_permissions_queue.clone(),
            self.cef_callback_registry.clone(),
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
            self.console_nonces.clone(),
            self.osr_view.clone(),
        );
        let mut rc_guard = self
            .request_context
            .lock()
            .map_err(|_| CoreError::CreateBrowserFailed)?;
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&cef_url),
            Some(&settings),
            None,
            rc_guard.as_mut(),
        )
        .ok_or(CoreError::CreateBrowserFailed)?;
        drop(rc_guard);

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

        let new_idx = {
            let mut tabs = self
                .tabs
                .lock()
                .map_err(|_| CoreError::CreateBrowserFailed)?;
            tabs.push(tab);
            let new_idx = tabs.len() - 1;
            if background {
                // M47: hide the new browser inside the SAME guard that
                // pushed it. The old code dropped the guard, re-locked, and
                // did a bare `tabs[new_idx]` — if another holder of the
                // `Arc<dyn BrowserEngine>` closed a tab in that window the
                // index panics, and with `panic = "unwind"` and no
                // `catch_unwind` in cef-rs the process aborts at the
                // `extern "C"` boundary.
                if let Some(host) = tabs[new_idx].browser.host() {
                    host.was_hidden(1);
                }
            }
            new_idx
        };
        if !background {
            self.set_active_index(new_idx);
        }
        info!(target: "buffr_core::host", %id, background, "tab opened");
        tracing::debug!(target: "buffr_core::host", %url, "tab opened — url");
        // Remember the user-facing URL for `buffr://` tabs so the omnibar
        // shows `buffr://new` rather than the http://127.0.0.1:… form.
        if url.starts_with("buffr://") || url.starts_with("buffr-src:") {
            self.record_display_url(id, url);
        }
        // Phase 6 telemetry: count every tab open (foreground +
        // background) — they are equally "user opened a tab" events.
        if let Some(c) = self.counters.as_ref() {
            c.increment(KEY_TABS_OPENED);
        }
        Ok(id)
    }

    fn set_active_index(&self, new_idx: usize) {
        // Lock order: `tabs` BEFORE `active` (M15). This used to be
        // reversed, which deadlocked against every other path the moment a
        // second thread touched the engine.
        let tabs = match self.tabs.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let mut active = match self.active.lock() {
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
        self.with_active_host_idx(|host, idx| {
            tracing::debug!(idx, "force_repaint_active: was_hidden cycle");
            host.was_hidden(1);
            host.was_hidden(0);
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
        });
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
        self.with_active_host_idx(|host, idx| {
            host.was_hidden(if sleep { 1 } else { 0 });
            tracing::debug!(
                target: "buffr_core::host",
                sleep,
                idx,
                "osr_sleep"
            );
        });
    }

    /// Wake helper: nudge CEF to deliver a fresh paint after waking from
    /// sleep.  Calls `was_resized` then `invalidate(VIEW)` on the active
    /// tab — same sequence as [`Self::set_active_index`] uses for the
    /// newly-activated browser so CEF deduplications are bypassed.
    pub fn osr_invalidate_view(&self) {
        self.with_active_host_idx(|host, idx| {
            host.was_resized();
            host.invalidate(cef::PaintElementType::VIEW);
            tracing::debug!(
                target: "buffr_core::host",
                idx,
                "osr_invalidate_view"
            );
        });
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
            url: self.display_url_for(t.id).unwrap_or_else(|| t.url.clone()),
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

        // Snapshot the display-URL override before deciding what to do
        // with the tab. It is only *forgotten* on the paths that genuinely
        // retire the tab — a stashed tab keeps it so reopen can restore it
        // (M19).
        let display_url = self.display_url_for(removed.id);

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
                    display_url,
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
            // Tear down any stack-evicted browsers outside the lock. These
            // are gone for good, so their overrides go too (M19).
            for t in evicted {
                self.forget_display_url(t.id);
                // Browser is gone for good — drop its console nonces so
                // the table doesn't grow with tab churn.
                self.console_nonces.forget(t.browser.identifier());
                if let Some(host) = t.browser.host() {
                    host.close_browser(1);
                }
            }
        } else {
            // Not stashable — close immediately and drop the override.
            self.forget_display_url(removed.id);
            self.console_nonces.forget(removed.browser.identifier());
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
        // The tab list shifted down by one when the closed tab sat before
        // the active one; fix up the stored index so set_active_index's
        // hide-previous guard still sees the tab that really is active.
        if let Ok(mut a) = self.active.lock() {
            *a = active_after_close(*a, idx);
        }
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
        {
            // Lock order: `tabs` BEFORE `active` (M15).
            let Ok(mut tabs) = self.tabs.lock() else {
                return;
            };
            let Some(idx) = self.active.lock().ok().and_then(|g| *g) else {
                return;
            };
            let Some(t) = tabs.get_mut(idx) else {
                return;
            };
            t.pinned = !t.pinned;
        }
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
        // Lock order: `tabs` BEFORE `active` (M15).
        let active_id = {
            let Ok(tabs) = self.tabs.lock() else {
                return;
            };
            let Ok(active) = self.active.lock() else {
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

    // L17: `record_url` had no callers — `pump_address_changes` is the live
    // path that folds CEF's `on_address_change` events into `Tab::url`
    // (and it is what actually calls `merge_navigation_url`). Deleted.

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
        let cef_navigation_url = self.cef_navigation_url(trimmed);
        self.with_active(|t| {
            let Some(frame) = t.browser.main_frame() else {
                warn!("navigate: main frame unavailable");
                return Err(CoreError::CreateBrowserFailed);
            };
            let cef_url = CefString::from(cef_navigation_url.as_ref());
            frame.load_url(Some(&cef_url));
            t.url = to_display_url(trimmed).into_owned();
            tracing::debug!(target: "buffr_core::host", url = %trimmed, "navigate");
            // Track or clear the display-url override so the omnibar / session
            // reports the right URL: buffr://* stays as the friendly form;
            // anything else clears any prior buffr:// override.
            if trimmed.starts_with("buffr://") || trimmed.starts_with("buffr-src:") {
                self.record_display_url(t.id, trimmed);
            } else {
                self.forget_display_url(t.id);
            }
            Ok(())
        })
        .ok_or(CoreError::CreateBrowserFailed)?
    }

    // ── Tab-strip lock discipline ────────────────────────────────────────
    //
    // LOCK ORDER INVARIANT (M15): whenever both `self.tabs` and
    // `self.active` are held at the same time, `tabs` MUST be acquired
    // FIRST. `BrowserHost` is handed to the apps layer as
    // `Arc<dyn BrowserEngine>` (`Send + Sync`), so the moment a second
    // thread calls any engine method, a path that takes `active → tabs`
    // deadlocks against a path that takes `tabs → active`, and the UI is
    // wedged permanently with no recovery.
    //
    // `set_active_index`, `enforce_pinned_ordering` and `toggle_pin_active`
    // used to take them the other way round. Everything now funnels through
    // the helpers below (or states the order explicitly), so the ordering
    // lives in one place instead of being re-derived at 19 call sites (L1).

    /// Borrow the active tab mutably under the manager mutex.
    /// Returns `None` only when there is no active tab.
    ///
    /// Takes `tabs` then `active` — see the lock-order invariant above.
    fn with_active<R>(&self, f: impl FnOnce(&mut Tab) -> R) -> Option<R> {
        let mut tabs = self.tabs.lock().ok()?;
        let idx = (*self.active.lock().ok()?)?;
        let t = tabs.get_mut(idx)?;
        Some(f(t))
    }

    /// Run `f` against the active tab's `cef::BrowserHost`.
    ///
    /// Returns `None` — and does not call `f` — when either mutex is
    /// poisoned, there is no active tab, the index is stale, or CEF has
    /// already torn the browser host down.
    ///
    /// This is the single place the `lock tabs → read active → get host`
    /// prologue lives (L1). Both mutexes are held for the duration of `f`,
    /// which is exactly what the open-coded copies did; keep `f` to CEF
    /// calls and cheap logging, and never re-enter `BrowserHost` from it.
    fn with_active_host<R>(&self, f: impl FnOnce(cef::BrowserHost) -> R) -> Option<R> {
        let tabs = self.tabs.lock().ok()?;
        let idx = (*self.active.lock().ok()?)?;
        let host = tabs.get(idx)?.browser.host()?;
        Some(f(host))
    }

    /// Like [`Self::with_active_host`] but also hands `f` the active index —
    /// only used by call sites that log it.
    fn with_active_host_idx<R>(&self, f: impl FnOnce(cef::BrowserHost, usize) -> R) -> Option<R> {
        let tabs = self.tabs.lock().ok()?;
        let idx = (*self.active.lock().ok()?)?;
        let host = tabs.get(idx)?.browser.host()?;
        Some(f(host, idx))
    }

    /// Run `f` against a popup browser's `cef::BrowserHost`.
    ///
    /// The popup equivalent of [`Self::with_active_host`] — popups live in
    /// `popup_browsers`, keyed by CEF `browser.identifier()`, and are not
    /// part of the tab strip, so this touches neither `tabs` nor `active`.
    fn with_popup_host<R>(
        &self,
        browser_id: i32,
        f: impl FnOnce(cef::BrowserHost) -> R,
    ) -> Option<R> {
        let browsers = self.popup_browsers.lock().ok()?;
        let host = browsers.get(&browser_id)?.host()?;
        Some(f(host))
    }

    /// Run `f` against a popup `cef::Browser` itself (not its host) — for
    /// the navigation calls that live on `Browser` rather than
    /// `BrowserHost`.
    fn with_popup_browser<R>(
        &self,
        browser_id: i32,
        f: impl FnOnce(&cef::Browser) -> R,
    ) -> Option<R> {
        let browsers = self.popup_browsers.lock().ok()?;
        let b = browsers.get(&browser_id)?;
        Some(f(b))
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

            A::Engine(_id) => {
                // Engine-swap is handled entirely at the apps layer (dispatch_action
                // in main.rs). The host dispatch path is a no-op so a stray
                // fallthrough doesn't confuse CEF.
                tracing::debug!("Engine action reached host dispatch — apps layer should handle");
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

        let alphabet = self.hint_alphabet.clone();
        let mut bail = false;
        self.with_active(|t| {
            t.session.hint_session = Some(HintSession::new(alphabet, Vec::new(), background));
            let Some(frame) = t.browser.main_frame() else {
                warn!("enter_hint_mode: main frame unavailable");
                bail = true;
                return;
            };
            // Fresh nonce per hint session (H5): a forged `kind:"ready"`
            // event picks the element the user's next hint keystroke
            // activates, so a nonce that leaked during an earlier session
            // must not still work here. Built inside `with_active` so the
            // nonce is keyed to the browser we actually inject into, and
            // only ever handed to the *main* frame.
            let nonce = self.console_nonces.rotate_hint(t.browser.identifier());
            let script =
                build_inject_script(&alphabet_str, &labels, DEFAULT_HINT_SELECTORS, &nonce);
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
                            "hint background commit: NOT routed through \
                             `open_tab_background` — committing as a same-tab click",
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

    /// Call an `edit.js` entry point on the active tab's main frame.
    ///
    /// L3: `run_edit_apply` / `run_edit_attach` / `run_edit_focus` /
    /// `run_edit_detach` were four copies of the same
    /// `serde_json::to_string(..).unwrap_or_else(..)` + `format!` against
    /// `"buffr://edit"`. `args` are raw JS expressions — pass them through
    /// [`js_arg`] so page-supplied field ids can't break out of the call.
    fn call_edit_fn(&self, name: &str, args: &[String], prelude: &str) {
        let joined = args.join(", ");
        self.run_main_frame_js(
            &format!("{prelude}if (window.{name}) window.{name}({joined})"),
            "buffr://edit",
        );
    }

    /// Push a new value into the focused field via `__buffrEditApply`.
    pub fn run_edit_apply(&self, field_id: &str, value: &str) {
        self.call_edit_fn("__buffrEditApply", &[js_arg(field_id), js_arg(value)], "");
    }

    /// Add the edit-active CSS class to the field via `__buffrEditAttach`.
    pub fn run_edit_attach(&self, field_id: &str) {
        self.call_edit_fn("__buffrEditAttach", &[js_arg(field_id)], "");
    }

    /// Re-focus a field by its buffr-assigned ID via `__buffrEditFocus`.
    pub fn run_edit_focus(&self, field_id: &str) {
        // Mark user gesture so edit.js's focusin gate doesn't blur the
        // re-focus call. This is invoked from `i` / FocusFirstInput
        // when last_focused_field is set — both are deliberate.
        self.call_edit_fn(
            "__buffrEditFocus",
            &[js_arg(field_id)],
            "window.__buffrUserGesture = true; ",
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
        self.call_edit_fn("__buffrCycleInput", &[arg.to_string()], "");
    }

    /// Remove the edit-active CSS class from the field via `__buffrEditDetach`.
    pub fn run_edit_detach(&self, field_id: &str) {
        self.call_edit_fn("__buffrEditDetach", &[js_arg(field_id)], "");
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

    // ── IME composition (#86) ─────────────────────────────────────────────────
    //
    // Thin wrappers around CEF's `BrowserHost` IME methods. The cef-rs bindings
    // expose:
    //   - `BrowserHost::ime_set_composition(text, underlines, replacement_range,
    //       selection_range)` — sets the preedit string.
    //   - `BrowserHost::ime_commit_text(text, replacement_range, relative_cursor_pos)`
    //     — commits text directly.
    //   - `BrowserHost::ime_cancel_composition()` — cancels composition.
    //
    // `underlines` and replacement ranges are left empty / None here; the OS IME
    // framework has already decided the underline styling and we have no
    // per-character metadata from winit.

    /// Update the active browser's IME preedit string.
    ///
    /// `cursor` is `(start, end)` as **byte** offsets within `text`; `None`
    /// collapses the selection to the end of `text`. Both are converted to
    /// UTF-16 code units before they reach CEF — see
    /// [`utf16_selection_range`].
    pub fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        let (sel_from, sel_to) = utf16_selection_range(text, cursor);
        self.with_active_host(|host| {
            let cef_text = cef::CefString::from(text);
            let selection_range = cef::Range {
                from: sel_from,
                to: sel_to,
            };
            host.ime_set_composition(Some(&cef_text), Some(&[]), None, Some(&selection_range));
        });
    }

    /// Commit `text` directly to the active browser's focused editable.
    pub fn ime_commit(&self, text: &str) {
        self.with_active(|t| {
            let Some(host) = t.browser.host() else {
                warn!("ime_commit: browser.host() returned None");
                return;
            };
            let cef_text = cef::CefString::from(text);
            host.ime_commit_text(Some(&cef_text), None, 0);
        });
    }

    /// Cancel any in-progress IME composition on the active browser.
    pub fn ime_cancel(&self) {
        self.with_active(|t| {
            let Some(host) = t.browser.host() else {
                warn!("ime_cancel: browser.host() returned None");
                return;
            };
            host.ime_cancel_composition();
        });
    }

    /// Fire the JS media-activity poll against the active tab's main frame.
    ///
    /// The poll script (see [`buffr_core::scripts::MEDIA_PROBE_POLL_JS`]) reads all
    /// five signal sources and writes a boolean result to
    /// `window.__buffr_media_active`.  A follow-up JS snippet can read it
    /// back on the next tick by a follow-up snippet.
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
        // The poll script's console sentinel carries the active tab's page
        // nonce (H5) — without it the display handler drops the line, and
        // conversely a page that emits `__buffr_media__:{"video":true}` on
        // its own can no longer pin the idle inhibitor on.
        self.with_active(|t| {
            let Some(frame) = t.browser.main_frame() else {
                warn!("run_media_probe: main frame unavailable");
                return;
            };
            let nonce = self.console_nonces.page(t.browser.identifier());
            let code = CefString::from(buffr_core::media_probe::build_poll_script(&nonce).as_str());
            let script_url = CefString::from("buffr://media-probe-poll");
            frame.execute_java_script(Some(&code), Some(&script_url), 0);
        });
    }

    // L17: `read_media_probe_result` was a documented no-op with no
    // callers. Media detection runs entirely through `BuffrAudioHandler`
    // plus the `__buffr_media__:` console sentinel scraped in
    // `BuffrDisplayHandler::on_console_message`. Deleted.

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
        self.run_main_frame_js(
            &media_op(x, y, MEDIA_BODY_PLAY_PAUSE),
            "buffr://context-menu",
        );
    }

    /// Toggle `.muted` on the media element under `(x, y)`.
    pub fn media_toggle_mute(&self, x: i32, y: i32) {
        self.run_main_frame_js(&media_op(x, y, MEDIA_BODY_MUTE), "buffr://context-menu");
    }

    /// Toggle `.loop` on the media element under `(x, y)`.
    pub fn media_toggle_loop(&self, x: i32, y: i32) {
        self.run_main_frame_js(&media_op(x, y, MEDIA_BODY_LOOP), "buffr://context-menu");
    }

    /// Toggle `.controls` on the media element under `(x, y)`.
    pub fn media_toggle_controls(&self, x: i32, y: i32) {
        self.run_main_frame_js(&media_op(x, y, MEDIA_BODY_CONTROLS), "buffr://context-menu");
    }

    /// Request Picture-in-Picture for the media element under `(x, y)`,
    /// or exit PiP if it's already the PiP element. Wrapped in try/catch
    /// because some hosts disable PiP.
    ///
    /// The only one of the five that does not go through [`media_op`]: it
    /// walks `HTMLVideoElement` rather than `HTMLMediaElement` and wraps the
    /// whole body in try/catch, so it has its own builder.
    pub fn media_picture_in_picture(&self, x: i32, y: i32) {
        self.run_main_frame_js(&media_pip_js(x, y), "buffr://context-menu");
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

// L17: `fn _hint_used(_: Hint) {}` was a literal `#[allow(dead_code)]`
// placeholder with no callers. Deleted.

/// Encode `value` as a JS literal safe to splice into a call expression.
///
/// Falls back to an empty string literal if serialization somehow fails, so
/// a malformed field id degrades to a no-op call rather than injecting raw
/// text into the page.
fn js_arg(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Convert a caller-supplied `(start, end)` **byte** range within `text`
/// into the UTF-16 code-unit range CEF's `selection_range` expects (M20).
///
/// `None` collapses the selection to the end of `text`. Byte offsets that
/// are out of range or not on a `char` boundary are clamped to the nearest
/// valid boundary at or below the offset — never a panic, and never an
/// out-of-range selection handed to CEF.
///
/// A 3-character Japanese preedit `"こんに"` is 9 bytes but 3 UTF-16 units;
/// passing the byte length made CEF see an out-of-range selection on
/// exactly the input class IME exists to support.
fn utf16_selection_range(text: &str, cursor: Option<(usize, usize)>) -> (u32, u32) {
    let total = text.encode_utf16().count() as u32;
    let Some((start, end)) = cursor else {
        return (total, total);
    };
    let from = byte_offset_to_utf16(text, start);
    let to = byte_offset_to_utf16(text, end);
    (from.min(to), from.max(to))
}

/// UTF-16 code-unit index of byte offset `byte` within `text`.
///
/// Clamps past-the-end to the string's UTF-16 length and rounds a
/// non-boundary offset down to the previous `char` boundary.
fn byte_offset_to_utf16(text: &str, byte: usize) -> u32 {
    if byte >= text.len() {
        return text.encode_utf16().count() as u32;
    }
    let mut b = byte;
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    text[..b].encode_utf16().count() as u32
}

// ── Context-menu media JS (L2) ───────────────────────────────────────────────
//
// `media_play_pause` / `media_toggle_mute` / `media_toggle_loop` /
// `media_toggle_controls` were four copies of the same twelve-line
// `elementFromPoint` walk with a single statement swapped. Only the body
// differs, so only the body is stored.
//
// (`crates/buffr-engine/src/media_js.rs` holds a second set of copies; that
// file is outside this change.)

/// Body for [`media_op`]: toggle play/pause.
const MEDIA_BODY_PLAY_PAUSE: &str = "if(el.paused)el.play();else el.pause();";
/// Body for [`media_op`]: toggle mute.
const MEDIA_BODY_MUTE: &str = "el.muted=!el.muted;";
/// Body for [`media_op`]: toggle loop.
const MEDIA_BODY_LOOP: &str = "el.loop=!el.loop;";
/// Body for [`media_op`]: toggle native controls.
const MEDIA_BODY_CONTROLS: &str = "el.controls=!el.controls;";

/// Build the `elementFromPoint` → nearest `HTMLMediaElement` walk and run
/// `body` against the element it finds (bound as `el`).
///
/// Falls back to the first `<video>`/`<audio>` in the document when the hit
/// point is an overlay sibling rather than the media element itself. `x`/`y`
/// are JSON-encoded — they are our own CEF-local coordinates, but encoding
/// them keeps the splice honest.
fn media_op(x: i32, y: i32, body: &str) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
    format!(
        "(function(x,y){{\
           var el=document.elementFromPoint(x,y);\
           while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
           if(!el)el=document.querySelector('video, audio');\
           if(!el)return;\
           {body}\
         }})({x},{y});"
    )
}

/// Picture-in-Picture toggle for the `<video>` under `(x, y)`.
///
/// Not expressible via [`media_op`]: it walks `HTMLVideoElement` (PiP is
/// video-only) and wraps everything in try/catch because some hosts disable
/// the PiP API outright.
fn media_pip_js(x: i32, y: i32) -> String {
    let x = serde_json::to_string(&x).unwrap_or_else(|_| "0".to_string());
    let y = serde_json::to_string(&y).unwrap_or_else(|_| "0".to_string());
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

/// Pixels per scroll-unit. `ScrollDown(3)` therefore moves 120px,
/// matching a typical "tap j three times" feel without making each
/// `j` feel laggy. Half/full-page scrolls go through their own
/// `window.innerHeight`-relative path so they're DPI-independent.
const STEP_PX: i64 = 40;

// ── BrowserEngine impl ────────────────────────────────────────────────────────
//
// Delegates every trait method to the matching inherent method. The inherent
// block continues to carry the full implementation; this impl is a thin shim
// that exposes it behind the neutral trait surface.
//
// Methods that still use CEF types internally (e.g. `CoreError` return values)
// convert at the boundary via `EngineError::Other(err.to_string())`.

impl buffr_engine::BrowserEngine for BrowserHost {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        self.close_all_browsers()
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, buffr_engine::EngineError> {
        self.open_tab(url)
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, buffr_engine::EngineError> {
        self.open_tab_background(url)
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn open_tab_at(
        &self,
        url: &str,
        insert_idx: usize,
    ) -> Result<TabId, buffr_engine::EngineError> {
        self.open_tab_at(url, insert_idx)
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn close_tab(&self, id: TabId) -> Result<bool, buffr_engine::EngineError> {
        self.close_tab(id)
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn close_active(&self) -> Result<bool, buffr_engine::EngineError> {
        self.close_active()
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn select_tab(&self, id: TabId) {
        self.select_tab(id)
    }

    fn next_tab(&self) {
        self.next_tab()
    }

    fn prev_tab(&self) {
        self.prev_tab()
    }

    fn move_tab(&self, from: usize, to: usize) {
        self.move_tab(from, to)
    }

    fn duplicate_active(&self) -> Result<TabId, buffr_engine::EngineError> {
        self.duplicate_active()
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn toggle_pin_active(&self) {
        self.toggle_pin_active()
    }

    fn set_pinned(&self, id: TabId, pinned: bool) {
        self.set_pinned(id, pinned)
    }

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, buffr_engine::EngineError> {
        self.reopen_closed_tab()
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn closed_stack_len(&self) -> usize {
        self.closed_stack_len()
    }

    fn active_tab(&self) -> Option<TabSummary> {
        self.active_tab()
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        self.tabs_summary()
    }

    fn tab_count(&self) -> usize {
        self.tab_count()
    }

    fn pinned_count(&self) -> usize {
        self.pinned_count()
    }

    fn active_index(&self) -> Option<usize> {
        self.active_index()
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), buffr_engine::EngineError> {
        self.navigate(url)
            .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn active_tab_live_url(&self) -> String {
        self.active_tab_live_url()
    }

    fn pump_address_changes(&self) -> bool {
        self.pump_address_changes()
    }

    // ── Viewport ─────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        self.resize(width, height)
    }

    fn set_device_scale(&self, scale: f32) {
        self.set_device_scale(scale)
    }

    fn set_frame_rate(&self, hz: u32) {
        self.set_frame_rate(hz)
    }

    fn notify_screen_info_changed(&self) {
        self.notify_screen_info_changed()
    }

    fn osr_resize(&self, width: u32, height: u32) {
        self.osr_resize(width, height)
    }

    // ── Input — neutral types ────────────────────────────────────────────────

    fn osr_key_event(&self, event: buffr_engine::NeutralKeyEvent) {
        self.osr_key_event(event)
    }

    fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        self.osr_mouse_move(x, y, modifiers)
    }

    fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: buffr_engine::MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        self.osr_mouse_click(x, y, button, mouse_up, click_count, modifiers)
    }

    fn osr_mouse_leave(&self, modifiers: u32) {
        self.osr_mouse_leave(modifiers)
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        self.osr_mouse_wheel(x, y, delta_x, delta_y, modifiers)
    }

    fn osr_focus(&self, focused: bool) {
        self.osr_focus(focused)
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> buffr_engine::SharedOsrFrame {
        self.osr_frame()
    }

    fn osr_view(&self) -> buffr_engine::SharedOsrViewState {
        self.osr_view()
    }

    fn force_repaint_active(&self) {
        self.force_repaint_active()
    }

    fn osr_sleep(&self, sleep: bool) {
        self.osr_sleep(sleep)
    }

    fn osr_invalidate_view(&self) {
        self.osr_invalidate_view()
    }

    fn set_osr_wake(&self, wake: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.set_osr_wake(wake)
    }

    // ── DevTools ─────────────────────────────────────────────────────────────

    fn open_devtools(&self, _tab: buffr_engine::TabId) -> Result<(), buffr_engine::EngineError> {
        self.show_dev_tools_at(None, None);
        Ok(())
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, query: &str, forward: bool) {
        self.start_find(query, forward)
    }

    fn stop_find(&self) {
        self.stop_find()
    }

    fn active_zoom_level(&self) -> f64 {
        self.active_zoom_level()
    }

    fn zoom_in(&self) {
        self.adjust_zoom(0.25);
    }

    fn zoom_out(&self) {
        self.adjust_zoom(-0.25);
    }

    fn zoom_reset(&self) {
        self.reset_zoom();
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        self.any_audio_active()
    }

    fn any_video_active(&self) -> bool {
        self.any_video_active()
    }

    // ── Popup sinks (Phase 6a, #95) ──────────────────────────────────────────

    fn popup_queue(&self) -> buffr_engine::popup::PopupQueue {
        self.popup_queue()
    }

    fn popup_create_sink(&self) -> buffr_engine::popup::PopupCreateSink {
        self.popup_create_sink()
    }

    fn popup_close_sink(&self) -> buffr_engine::popup::PopupCloseSink {
        self.popup_close_sink()
    }

    fn popup_resize(&self, browser_id: i32, width: u32, height: u32) {
        self.popup_resize(browser_id, width, height)
    }

    fn popup_close(&self, browser_id: i32) {
        self.popup_close(browser_id)
    }

    fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        self.popup_drain_address_changes()
    }

    fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        self.popup_drain_title_changes()
    }

    fn popup_history_back(&self, browser_id: i32) {
        self.popup_history_back(browser_id)
    }

    fn popup_history_forward(&self, browser_id: i32) {
        self.popup_history_forward(browser_id)
    }

    fn popup_osr_focus(&self, browser_id: i32, focused: bool) {
        self.popup_osr_focus(browser_id, focused)
    }

    fn popup_osr_key_event(&self, browser_id: i32, event: buffr_engine::NeutralKeyEvent) {
        self.popup_osr_key_event(browser_id, event)
    }

    fn popup_osr_mouse_click(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        button: buffr_engine::MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        self.popup_osr_mouse_click(browser_id, x, y, button, mouse_up, click_count, modifiers)
    }

    fn popup_osr_mouse_move(&self, browser_id: i32, x: i32, y: i32, modifiers: u32) {
        self.popup_osr_mouse_move(browser_id, x, y, modifiers)
    }

    fn popup_osr_mouse_wheel(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    ) {
        self.popup_osr_mouse_wheel(browser_id, x, y, delta_x, delta_y, modifiers)
    }

    // ── Hint mode (Phase 6b, #95) ─────────────────────────────────────────────

    fn is_hint_mode(&self) -> bool {
        self.is_hint_mode()
    }

    fn hint_status(&self) -> Option<buffr_engine::HintStatus> {
        self.hint_status()
    }

    fn pump_hint_events(&self) -> bool {
        self.pump_hint_events()
    }

    fn feed_hint_key(&self, c: char) -> Option<buffr_engine::HintAction> {
        self.feed_hint_key(c)
    }

    fn backspace_hint(&self) -> Option<buffr_engine::HintAction> {
        self.backspace_hint()
    }

    fn cancel_hint(&self) {
        self.cancel_hint()
    }

    // ── Frame editing (Phase 6c, #95) ────────────────────────────────────────

    fn frame_undo(&self) {
        self.frame_undo()
    }

    fn frame_redo(&self) {
        self.frame_redo()
    }

    fn frame_cut(&self) {
        self.frame_cut()
    }

    fn frame_copy(&self) {
        self.frame_copy()
    }

    fn frame_paste(&self) {
        self.frame_paste()
    }

    fn frame_paste_plain(&self) {
        self.frame_paste_plain()
    }

    fn frame_select_all(&self) {
        self.frame_select_all()
    }

    // ── Media (Phase 6c, #95) ────────────────────────────────────────────────

    fn media_play_pause(&self, x: i32, y: i32) {
        self.media_play_pause(x, y)
    }

    fn media_picture_in_picture(&self, x: i32, y: i32) {
        self.media_picture_in_picture(x, y)
    }

    fn media_toggle_controls(&self, x: i32, y: i32) {
        self.media_toggle_controls(x, y)
    }

    fn media_toggle_loop(&self, x: i32, y: i32) {
        self.media_toggle_loop(x, y)
    }

    fn media_toggle_mute(&self, x: i32, y: i32) {
        self.media_toggle_mute(x, y)
    }

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────

    fn run_js(&self, code: &str) -> Result<(), buffr_engine::EngineError> {
        self.run_js(code);
        Ok(())
    }

    fn run_main_frame_js(&self, code: &str, url: &str) -> Result<(), buffr_engine::EngineError> {
        self.run_main_frame_js(code, url);
        Ok(())
    }

    // ── Edit IPC helpers (Phase 6c, #95) ─────────────────────────────────────

    fn run_edit_attach(&self, field_id: &str) {
        self.run_edit_attach(field_id)
    }

    fn run_edit_cycle(&self, forward: bool) {
        self.run_edit_cycle(forward)
    }

    fn run_edit_detach(&self, field_id: &str) {
        self.run_edit_detach(field_id)
    }

    fn run_edit_focus(&self, field_id: &str) {
        self.run_edit_focus(field_id)
    }

    fn run_media_probe(&self) {
        self.run_media_probe()
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    fn start_download(&self, url: &str) {
        self.start_download(url)
    }

    // ── DevTools at point (Phase 6c, #95) ────────────────────────────────────

    fn show_dev_tools_at(&self, x: i32, y: i32) {
        self.show_dev_tools_at(Some(x), Some(y))
    }

    // ── Clipboard (Phase 6d, #95) ─────────────────────────────────────────────

    fn clipboard_handle(&self) -> Option<buffr_engine::ClipboardReader> {
        // `BrowserHost::clipboard_handle` returns `Option<crate::ClipboardReader>`.
        // `crate::ClipboardReader` wraps `Arc<hjkl_clipboard::Clipboard>` and
        // implements `buffr_engine::ClipboardRead`. Wrap it in `Arc` to match
        // the `ClipboardReader = Arc<dyn ClipboardRead>` alias.
        self.clipboard_handle()
            .map(|r| -> buffr_engine::ClipboardReader { std::sync::Arc::new(r) })
    }

    fn clipboard_text(&self) -> Option<String> {
        self.clipboard_text()
    }

    fn clipboard_set_text(&self, text: &str) -> bool {
        self.clipboard_set_text(text)
    }

    fn copy_image_url_to_clipboard(&self, url: &str) {
        self.copy_image_url_to_clipboard(url)
    }

    // ── Audio events (Phase 6d, #95) ─────────────────────────────────────────

    fn drain_audio_events(&self) -> Vec<buffr_engine::AudioEvent> {
        // `BrowserHost::drain_audio_events` returns `Vec<crate::audio::AudioEvent>`.
        // `crate::audio::AudioEvent` and `buffr_engine::AudioEvent` are structurally
        // identical (same field names and types); convert field-by-field.
        self.drain_audio_events()
            .into_iter()
            .map(|e| buffr_engine::AudioEvent {
                browser_id: e.browser_id,
                active: e.active,
            })
            .collect()
    }

    // ── Cursor (Phase 6d, #95) ────────────────────────────────────────────────

    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        self.cursor_state().take()
    }

    // ── Favicon (Phase 6d, #95) ───────────────────────────────────────────────

    fn drain_favicon_updates(&self) -> Vec<buffr_engine::FaviconUpdate> {
        buffr_core::drain_favicon_updates(&self.favicon_sink())
            .into_iter()
            .map(|u| buffr_engine::FaviconUpdate {
                browser_id: u.browser_id,
                width: u.width,
                height: u.height,
                pixels: u.pixels,
            })
            .collect()
    }

    // ── Loading state / navigation capability (Phase 6f, #95) ───────────────

    fn is_loading(&self) -> bool {
        self.is_loading()
    }

    fn can_go_back(&self) -> bool {
        self.with_active(|t| t.browser.can_go_back() != 0)
            .unwrap_or(false)
    }

    fn can_go_forward(&self) -> bool {
        self.with_active(|t| t.browser.can_go_forward() != 0)
            .unwrap_or(false)
    }

    // ── Context menu (Phase 6f, #95) ─────────────────────────────────────────
    //
    // Override the default empty impl. Drain the internal CEF sink and map
    // `buffr_core::ContextMenuRequest` → `buffr_engine::ContextMenuRequest`.
    // `source_url` maps to both `image_url` and `media_url` so the apps
    // layer can use either field. `is_editable` is derived from the items.

    fn drain_context_menu_requests(&self) -> Vec<buffr_engine::ContextMenuRequest> {
        use buffr_engine::types::MediaType;
        self.drain_context_menu_requests()
            .into_iter()
            .map(|r| {
                let source = if r.source_url.is_empty() {
                    None
                } else {
                    Some(r.source_url)
                };
                let is_editable = r.items.iter().any(|i| {
                    matches!(
                        i,
                        buffr_core::ContextMenuItem::Cut | buffr_core::ContextMenuItem::Paste
                    )
                });
                let has_image = r
                    .items
                    .iter()
                    .any(|i| matches!(i, buffr_core::ContextMenuItem::CopyImage));
                let has_media_controls = r.items.iter().any(|i| {
                    matches!(
                        i,
                        buffr_core::ContextMenuItem::MediaPlayPause { .. }
                            | buffr_core::ContextMenuItem::MediaMute { .. }
                    )
                });
                let media_type = if has_image {
                    MediaType::Image
                } else if has_media_controls {
                    // CEF doesn't distinguish video/audio in the items list;
                    // use Video as the canonical non-image media bucket.
                    MediaType::Video
                } else {
                    MediaType::None
                };
                buffr_engine::ContextMenuRequest {
                    browser_id: r.browser_id,
                    x: r.x,
                    y: r.y,
                    page_url: String::new(),
                    frame_url: String::new(),
                    link_url: if r.link_url.is_empty() {
                        None
                    } else {
                        Some(r.link_url)
                    },
                    image_url: source.clone(),
                    media_url: source,
                    selection_text: if r.selection_text.is_empty() {
                        None
                    } else {
                        Some(r.selection_text)
                    },
                    is_editable,
                    has_image_contents: has_image,
                    media_type,
                }
            })
            .collect()
    }

    // ── Action dispatch (Phase 6f, #95) ──────────────────────────────────────

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        self.dispatch(action)
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        self.ime_set_composition(text, cursor)
    }

    fn ime_commit(&self, text: &str) {
        self.ime_commit(text)
    }

    fn ime_cancel(&self) {
        self.ime_cancel()
    }

    // ── Permissions (Phase 8a, #88) ───────────────────────────────────────────

    fn permissions_queue(&self) -> buffr_engine::PermissionsQueue {
        self.neutral_permissions_queue.clone()
    }

    fn resolve_permission(&self, resolve_id: Option<&str>, outcome: buffr_engine::PromptOutcome) {
        let Some(id) = resolve_id else {
            tracing::debug!("permissions: resolve_permission called with no resolve_id (no-op)");
            return;
        };
        let pending = match self.cef_callback_registry.lock() {
            Ok(mut reg) => reg.remove(id),
            Err(_) => {
                tracing::warn!(id, "permissions: callback registry mutex poisoned");
                return;
            }
        };
        let Some(pending) = pending else {
            tracing::debug!(
                id,
                "permissions: resolve_id not found in registry (already resolved?)"
            );
            return;
        };
        if let Err(err) = pending.resolve(outcome, &self.permissions) {
            tracing::warn!(error = %err, id, "permissions: resolve failed");
        }
    }
}

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
    // These exercise the PRODUCTION builders. They used to re-implement all
    // five media snippets verbatim inside this module (L2), so the tests
    // would still have passed if the real builders had been deleted.

    #[test]
    fn media_play_pause_js_contains_coords_and_toggle() {
        let js = super::media_op(100, 200, super::MEDIA_BODY_PLAY_PAUSE);
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
        let js = super::media_op(10, 20, super::MEDIA_BODY_MUTE);
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
        let js = super::media_op(0, 0, super::MEDIA_BODY_LOOP);
        assert!(js.contains("el.loop=!el.loop"), "loop toggle missing");
        assert!(js.contains("HTMLMediaElement"), "media type guard missing");
        assert!(
            js.contains("querySelector('video, audio')"),
            "overlay-sibling fallback missing"
        );
    }

    #[test]
    fn media_controls_js_contains_toggle() {
        let js = super::media_op(0, 0, super::MEDIA_BODY_CONTROLS);
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
        let js = super::media_pip_js(50, 60);
        assert!(js.contains("50"), "x coord missing");
        assert!(js.contains("60"), "y coord missing");
        assert!(js.contains("pictureInPictureElement"), "PiP check missing");
        assert!(js.contains("requestPictureInPicture"), "requestPiP missing");
        assert!(js.contains("exitPictureInPicture"), "exitPiP missing");
        assert!(js.contains("try{"), "try/catch missing");
        assert!(js.contains("HTMLVideoElement"), "video type guard missing");
        assert!(
            js.contains("querySelector('video')"),
            "overlay-sibling fallback missing"
        );
        // PiP must NOT use the shared HTMLMediaElement walk — PiP is
        // video-only, so a bare <audio> hit must not be promoted.
        assert!(!js.contains("HTMLMediaElement"));
    }

    #[test]
    fn media_op_shares_one_walk_across_bodies() {
        // The whole point of L2: every body reuses the identical prologue.
        let prologue = "var el=document.elementFromPoint(x,y);";
        for body in [
            super::MEDIA_BODY_PLAY_PAUSE,
            super::MEDIA_BODY_MUTE,
            super::MEDIA_BODY_LOOP,
            super::MEDIA_BODY_CONTROLS,
        ] {
            let js = super::media_op(1, 2, body);
            assert!(js.contains(prologue), "prologue missing for {body}");
            assert!(js.ends_with("})(1,2);"), "call tail wrong for {body}");
            assert!(js.contains(body));
        }
    }

    #[test]
    fn media_op_negative_coords_are_encoded_not_spliced() {
        let js = super::media_op(-5, -7, super::MEDIA_BODY_MUTE);
        assert!(js.ends_with("})(-5,-7);"), "{js}");
    }

    // ── L3: one edit-JS call builder ──────────────────────────────────────

    #[test]
    fn js_arg_quotes_and_escapes() {
        assert_eq!(super::js_arg("abc"), "\"abc\"");
        assert_eq!(super::js_arg("a\"b"), "\"a\\\"b\"");
        // A field id that tries to close the call and inject a statement
        // stays inside the string literal.
        let arg = super::js_arg("x\"); alert(1); (\"");
        assert!(!arg.contains("alert(1);\""), "{arg}");
        assert!(arg.starts_with('"') && arg.ends_with('"'));
    }

    #[test]
    fn js_arg_handles_newlines_and_unicode() {
        assert_eq!(super::js_arg("a\nb"), "\"a\\nb\"");
        assert_eq!(super::js_arg("こんに"), "\"こんに\"");
    }

    // ── M20: IME selection range is UTF-16, not bytes ─────────────────────

    #[test]
    fn ime_range_defaults_to_utf16_length_not_byte_length() {
        // "こんに" is 9 UTF-8 bytes but 3 UTF-16 code units. Passing the
        // byte length handed CEF an out-of-range selection on exactly the
        // input class IME exists for.
        let text = "こんに";
        assert_eq!(text.len(), 9);
        assert_eq!(super::utf16_selection_range(text, None), (3, 3));
    }

    #[test]
    fn ime_range_ascii_is_unchanged() {
        assert_eq!(super::utf16_selection_range("hello", None), (5, 5));
        assert_eq!(super::utf16_selection_range("hello", Some((1, 3))), (1, 3));
        assert_eq!(super::utf16_selection_range("", None), (0, 0));
    }

    #[test]
    fn ime_range_converts_byte_offsets() {
        let text = "こんに"; // 3 chars, 3 bytes each, 3 UTF-16 units
        assert_eq!(super::utf16_selection_range(text, Some((0, 9))), (0, 3));
        assert_eq!(super::utf16_selection_range(text, Some((3, 6))), (1, 2));
        assert_eq!(super::utf16_selection_range(text, Some((0, 3))), (0, 1));
    }

    #[test]
    fn ime_range_counts_surrogate_pairs_as_two_units() {
        // U+1F600 is one char, 4 UTF-8 bytes, TWO UTF-16 code units.
        let text = "a😀b";
        assert_eq!(text.len(), 6);
        assert_eq!(text.chars().count(), 3);
        assert_eq!(super::utf16_selection_range(text, None), (4, 4));
        assert_eq!(super::utf16_selection_range(text, Some((0, 5))), (0, 3));
    }

    #[test]
    fn ime_range_clamps_out_of_bounds_and_non_boundaries() {
        let text = "こんに";
        // Past the end clamps to the UTF-16 length.
        assert_eq!(super::utf16_selection_range(text, Some((0, 999))), (0, 3));
        // Byte 1 and 2 are inside the first codepoint — round down to 0.
        assert_eq!(super::utf16_selection_range(text, Some((1, 2))), (0, 0));
        assert_eq!(super::utf16_selection_range(text, Some((4, 9))), (1, 3));
    }

    #[test]
    fn ime_range_normalises_reversed_selection() {
        // A backwards selection must not produce from > to.
        let (from, to) = super::utf16_selection_range("hello", Some((4, 1)));
        assert!(from <= to);
        assert_eq!((from, to), (1, 4));
    }

    // ── backlog §10 item 1: active-index fixup after a close ──────────────

    #[test]
    fn active_after_close_shifts_down_when_closing_left_of_active() {
        // The §10-1 repro: [A,B,C] active=Some(2), close_index(0) → list is
        // [B,C] but the stored index used to stay Some(2), so the
        // hide-previous guard `prev < tabs.len()` (`2 < 2`) never fired and
        // the old active browser kept running as if foregrounded.
        assert_eq!(super::active_after_close(Some(2), 0), Some(1));
    }

    #[test]
    fn active_after_close_shifts_down_when_closing_immediately_left_of_active() {
        assert_eq!(super::active_after_close(Some(2), 1), Some(1));
    }

    #[test]
    fn active_after_close_unchanged_when_closing_active_tab() {
        // Closing the active tab itself — no shift; `set_active_index`
        // will overwrite the stored index with the new active anyway.
        assert_eq!(super::active_after_close(Some(2), 2), Some(2));
    }

    #[test]
    fn active_after_close_unchanged_when_closing_active_tab_at_zero() {
        assert_eq!(super::active_after_close(Some(0), 0), Some(0));
    }

    #[test]
    fn active_after_close_unchanged_when_closing_after_active() {
        assert_eq!(super::active_after_close(Some(1), 2), Some(1));
    }

    #[test]
    fn active_after_close_none_stays_none() {
        assert_eq!(super::active_after_close(None, 3), None);
    }
}
