//! [`WebKitEngine`] — Phase 2 [`BrowserEngine`] impl for WPE WebKit.
//!
//! Engine methods send [`Command`]s to the GLib worker thread via mpsc.
//! Tab state is read from the shared `Arc<Mutex<EngineState>>`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use buffr_core::cursor::SharedCursorState;
use buffr_core::edit::EditEventSink;
use buffr_core::hint::{
    DEFAULT_HINT_ALPHABET, DEFAULT_HINT_SELECTORS, HintAlphabet, HintConsoleEvent, HintSession,
    build_inject_script, take_hint_event,
};
use buffr_downloads::Downloads;
use buffr_engine::{
    BackendOpenOptions, BrowserEngine, ClipboardRead, ContextMenuRequest, EngineError, HintAction,
    HintStatus, MouseButton, NeutralKeyEvent, NewTabHtmlProvider, OsrFrame, OsrViewState,
    PromptOutcome, SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
    engine_id::EngineId,
    internal_server::InternalServer,
    permissions::PermissionsQueue,
    popup::{
        PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
    },
};

use super::runtime::WpePermissionRequestPtr;

use super::error::WebKitError;
use super::runtime::{WpeAudioEventQueue, WpeContextMenuSink};
use super::worker::{Command, WorkerHandle, WpeKeyEvent, spawn};

// ── WebKitEngine ──────────────────────────────────────────────────────────────

/// WPE WebKit browser engine.
///
/// `Send + Sync` — all mutable state is behind `Arc<Mutex<_>>` or sent as
/// commands to the GLib worker thread.
pub struct WebKitEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Shared OSR frame — written by the FDO SHM callback on the worker thread.
    frame: SharedOsrFrame,
    /// Shared OSR viewport state.
    view: SharedOsrViewState,
    /// Worker thread handle.
    worker: WorkerHandle,
    /// Popup sinks — empty in Phase 2.
    popup_queue: PopupQueue,
    popup_create_sink: PopupCreateSink,
    popup_close_sink: PopupCloseSink,
    /// Current live URL (updated via `pump_address_changes`).
    live_url: Mutex<String>,
    /// `buffr://new` HTML provider. Wired by buffr-app at registration so
    /// the page reflects current keybinds / palette / splash art. Used by
    /// the InternalServer `/new` route handler when set.
    newtab_html_provider: Mutex<Option<NewTabHtmlProvider>>,
    /// Optional shared loopback HTTP server. When set, `buffr://path`
    /// resolves to `http://127.0.0.1:<port>/<token>/path`; the server
    /// invokes per-route handlers wired by the host so internal pages get
    /// a real HTTP origin (fetch, modules, CSS imports all work). When
    /// unset, `buffr://` URLs fail visibly rather than falling back to a
    /// data: URL.
    internal_server: Mutex<Option<Arc<InternalServer>>>,
    /// Per-tab mapping from TabId to the *display* URL the user typed
    /// (e.g. `buffr://new`). Tracked separately from the translated URL
    /// actually loaded into WebKit so the omnibar shows the human URL.
    display_urls: Mutex<HashMap<TabId, String>>,
    /// Shared with `WpeRuntime` + every `TabSignalCtx`. Toggled to true
    /// in `open_tab` / `navigate` and back to false by the worker's
    /// `load-changed` signal handler on `WEBKIT_LOAD_FINISHED`. Read by
    /// `is_loading` below — never goes through the engine_state mutex
    /// so the host's animation gate can't get stuck.
    is_loading_atomic: Arc<AtomicBool>,
    /// Lock-free nav-state. Written by the GLib worker's `on_load_changed`
    /// signal handler (COMMITTED/FINISHED, active-tab only) and by
    /// `select_tab` so switching tabs refreshes state synchronously.
    /// Read by `can_go_back` / `can_go_forward` on the UI thread.
    can_go_back: Arc<AtomicBool>,
    can_go_forward: Arc<AtomicBool>,
    /// Shared with `WpeRuntime`. Written by the GLib worker on zoom commands;
    /// read from any thread via `active_zoom_level`. Initialised to 1.0.
    zoom_level: Arc<Mutex<f64>>,
    /// Alphabet used to mint hint labels. Default is
    /// `buffr_core::hint::DEFAULT_HINT_ALPHABET`.
    hint_alphabet: HintAlphabet,
    /// Active hint session for the current tab. `None` when not in hint mode.
    /// Mutated by `feed_hint_key`, `backspace_hint`, `cancel_hint`, and
    /// populated with the hint list from `pump_hint_events` when the renderer
    /// fires the `ready` event via the UCM script-message bridge.
    hint_session: Mutex<Option<HintSession>>,
    /// Context-menu request sink. Shared with `WpeRuntime` and every
    /// `TabEntry`'s `context-menu` signal handler. Drained by
    /// `drain_context_menu_requests`.
    context_menu_sink: WpeContextMenuSink,
    /// Favicon decode sink. Shared with `WpeRuntime` and the per-tab
    /// background fetch threads. Drained by `drain_favicon_updates`.
    favicon_sink: buffr_core::favicon::FaviconSink,
    /// System clipboard reader. `None` when clipboard initialisation failed at
    /// startup (e.g. headless / SSH without OSC-52 fallback).
    clipboard_reader: Option<std::sync::Arc<super::clipboard::WebKitClipboardReader>>,
    /// Shared permissions queue — cloned from the worker's queue so the apps
    /// layer drains the same VecDeque the signal handler pushes into.
    permissions_queue: PermissionsQueue,
    /// Map resolve_id → g_object_ref'd WebKitPermissionRequest ptr. Shared
    /// with the GLib worker thread. Kept here for the `resolve_permission`
    /// command dispatch path.
    #[allow(dead_code)]
    pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>>,
    /// Monotonic counter for resolve_ids. Shared with the GLib worker.
    #[allow(dead_code)]
    permission_next_id: Arc<AtomicU64>,
    /// Shared audio-event queue (#132). Written by per-tab `buffrAudio` UCM
    /// signal handlers on the GLib worker thread; drained here via
    /// `drain_audio_events`. Same Arc as the one in WorkerHandle.
    audio_event_queue: WpeAudioEventQueue,
    /// Shared cursor state (#137). Written by per-tab `buffrCursor` UCM signal
    /// handlers on the GLib worker thread; read via `take_cursor_change`.
    /// Same Arc as the one in WorkerHandle.
    cursor_state: SharedCursorState,
    /// Runtime-wide video-active flag (#135). Written by per-tab
    /// `buffrMediaProbe` UCM signal handlers on the GLib worker thread;
    /// read lock-free from any thread by `any_video_active`.
    /// Same Arc as the one in WorkerHandle.
    video_active: Arc<AtomicBool>,
    /// Edit-mode event sink (#134). Written by per-tab `buffrEdit` UCM signal
    /// handlers on the GLib worker thread; drained by the apps layer via
    /// `buffr_core::edit::drain_edit_events`. Populated by `set_edit_sink`
    /// after construction; `None` until the apps layer calls that setter.
    edit_sink: Arc<Mutex<Option<EditEventSink>>>,
    /// Native Wayland + EGL handles extracted from the host winit window
    /// (#151).  `None` on non-Wayland sessions or before the apps layer
    /// calls `set_native_wayland_handles`.  Stored here for the upcoming
    /// `BuffrDisplayWayland` C subclass (#152) to read; nothing consumes
    /// these pointers yet.
    wayland_handles: Mutex<Option<buffr_engine::WaylandNativeHandles>>,
    /// True when the runtime selected a native compositing display
    /// backend (`BuffrDisplayWayland` or stock `WPEDisplayWayland`) at
    /// construction; false when it fell back to OSR.  Read by the apps
    /// layer via `BrowserEngine::is_using_native_compositing` to gate
    /// behaviour that differs between the two pixel pipelines (loading
    /// animation overlay, chrome transparency, etc.).  Set once during
    /// `new_with_server` and never mutated thereafter.
    using_native: Arc<AtomicBool>,
}

impl WebKitEngine {
    /// Construct a new Phase 2 engine.
    ///
    /// Initialises the WPE loader, spawns the GLib worker thread, and opens
    /// the initial tab at `options.initial_url`. Equivalent to
    /// [`Self::new_with_server`] with `None`; useful when the embedder
    /// doesn't run a buffr internal-page server.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitError> {
        Self::new_with_server(options, None)
    }

    /// Construct an engine and bind it to a shared [`InternalServer`] in
    /// one shot, so the worker's very first `open_tab` (fired from the
    /// GLib idle handler before the embedder can call any setter) loads
    /// `buffr://*` URLs via the server instead of falling back to a data
    /// URL. The server keeps working for every subsequent navigate.
    pub fn new_with_server(
        options: &BackendOpenOptions<'_>,
        internal_server: Option<Arc<InternalServer>>,
    ) -> Result<Self, WebKitError> {
        let (width, height) = options.initial_size;
        // Translate `buffr://*` before the worker's idle handler loads the
        // initial URL. Prefer the loopback HTTP server when available so
        // the initial tab matches the URL we'll use for subsequent navs;
        // otherwise fall back to a self-contained data: URL.
        let initial_url_owned = if let Some(rest) = options.initial_url.strip_prefix("buffr://")
            && let Some(server) = internal_server.as_ref()
        {
            server.url_for(&format!("/{rest}"))
        } else {
            // Server not attached — pass the URL through. If it is a
            // `buffr://` URL the engine will fail visibly (no server, no
            // custom scheme handler) rather than masking the error with a
            // data: URL. Non-buffr URLs are navigated as-is.
            options.initial_url.to_owned()
        };
        let initial_url = initial_url_owned.as_str();

        tracing::info!("webkit: WebKitEngine::new {width}x{height}");

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = Arc::new(OsrViewState::new());

        // Set initial viewport dims on the view state.
        view.width.store(width, Ordering::Relaxed);
        view.height.store(height, Ordering::Relaxed);
        if options.frame_rate > 0 {
            view.frame_rate_hz
                .store(options.frame_rate as u32, Ordering::Relaxed);
        }

        // Start as `true` — the initial open_tab the worker fires from
        // its idle handler is a load in progress until WebKit reports
        // WEBKIT_LOAD_FINISHED. Without this the host's animation gate
        // would briefly observe is_loading=false at startup before the
        // signal flips it to true, which can race a paint.
        let is_loading_atomic = Arc::new(AtomicBool::new(true));
        let zoom_level = Arc::new(Mutex::new(1.0_f64));

        // Nav-state atomics — initialised to false (no history on a fresh tab).
        let can_go_back = Arc::new(AtomicBool::new(false));
        let can_go_forward = Arc::new(AtomicBool::new(false));

        // Build the cookie DB path.  See `compute_cookie_db_path` for
        // the per-engine namespacing rules; extracted as a pure fn so
        // the doubled-path regression has a unit test.
        //
        // W7: private (incognito) engines get `None`, which leaves WebKit's
        // default in-memory cookie store active. Previously `options.private`
        // was never read, so a `--private` window wrote cookies straight into
        // the real profile's `cookies.sqlite`.
        let private = options.private;
        let xdg_fallback = compute_xdg_data_home();
        let cookie_db_path = if private {
            tracing::info!("webkit: private mode — cookies stay in memory");
            None
        } else {
            compute_cookie_db_path(
                options.data_dir,
                options.engine_id.as_str(),
                xdg_fallback.as_deref(),
            )
        };

        // Downcast the downloads sink from BackendOpenOptions if provided.
        //
        // W9: `options.downloads` is `Arc<dyn Any + Send + Sync>`. The old
        // `any.downcast_ref::<Arc<Downloads>>()` auto-deref'd through the Arc
        // and asked the *inner* `dyn Any` whether it was an `Arc<Downloads>`
        // — which only holds if the caller erased an `Arc<Arc<Downloads>>`.
        // A caller passing the natural `Arc<Downloads>` silently got `None`
        // and every download became invisible to the store. Use
        // `Arc::downcast` (which inspects the erased type itself), keep the
        // legacy double-Arc shape working, and warn loudly when a non-`None`
        // sink matches neither.
        let downloads: Option<Arc<Downloads>> = match options.downloads.as_ref() {
            None => None,
            Some(any) => match Arc::clone(any).downcast::<Downloads>() {
                Ok(store) => Some(store),
                // Legacy shape: the caller erased an `Arc<Arc<Downloads>>`.
                Err(orig) => match orig.downcast_ref::<Arc<Downloads>>() {
                    Some(nested) => Some(Arc::clone(nested)),
                    None => {
                        tracing::warn!(
                            "webkit: BackendOpenOptions.downloads is not an Arc<Downloads> — \
                             download tracking disabled for this engine"
                        );
                        None
                    }
                },
            },
        };

        let using_native = Arc::new(AtomicBool::new(false));
        let worker = spawn(
            initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
            Arc::clone(&is_loading_atomic),
            Arc::clone(&zoom_level),
            cookie_db_path,
            downloads,
            Arc::clone(&can_go_back),
            Arc::clone(&can_go_forward),
            options.prefer_native,
            options.wayland_handles,
            Arc::clone(&using_native),
            private,
        )?;

        // Share the popup_queue that the worker already created and wired to
        // each TabEntry's `create` signal. Using the same Arc ensures the
        // apps layer drains the same queue the signals push into.
        let popup_queue = Arc::clone(&worker.popup_queue);

        // Share the context-menu sink that the worker wired to each TabEntry's
        // `context-menu` signal. Using the same Arc ensures the apps layer
        // drains the same queue the signals push into.
        let context_menu_sink = Arc::clone(&worker.context_menu_sink);

        // Share the favicon sink that the worker wired to each TabEntry's
        // `buffrFavicon` UCM handler. Using the same Arc ensures the engine
        // drains the same queue the background fetch threads push into.
        let favicon_sink = Arc::clone(&worker.favicon_sink);

        // Share the permissions queue + pending map + id counter created in spawn.
        // The apps layer calls permissions_queue() to get the Arc; the worker
        // thread's signal handler and Command::ResolvePermission handler both use
        // the pending_permissions and permission_next_id Arcs through WpeRuntime.
        let permissions_queue = Arc::clone(&worker.permissions_queue);
        let pending_permissions = Arc::clone(&worker.pending_permissions);
        let permission_next_id = Arc::clone(&worker.permission_next_id);

        // Share the audio-event queue created in spawn (#132).
        let audio_event_queue = Arc::clone(&worker.audio_event_queue);

        // Share the cursor state created in spawn (#137).
        let cursor_state = Arc::clone(&worker.cursor_state);

        // Share the video-active flag created in spawn (#135).
        let video_active = Arc::clone(&worker.video_active);

        // Share the edit-mode event sink created in spawn (#134).
        // The engine's `set_edit_sink` populates the inner Option at runtime.
        let edit_sink = Arc::clone(&worker.edit_sink);

        // Default hint alphabet. Fallback to a hard-coded 2-char alphabet
        // if DEFAULT_HINT_ALPHABET ever fails validation (it never does,
        // but the API returns Result).
        let hint_alphabet = HintAlphabet::from_str(DEFAULT_HINT_ALPHABET)
            .unwrap_or_else(|_| HintAlphabet::from_str("as").expect("fallback alphabet"));

        Ok(Self {
            engine_id: options.engine_id.clone(),
            frame,
            view,
            worker,
            popup_queue,
            popup_create_sink: new_popup_create_sink(),
            popup_close_sink: new_popup_close_sink(),
            live_url: Mutex::new(String::new()),
            newtab_html_provider: Mutex::new(None),
            internal_server: Mutex::new(internal_server),
            display_urls: Mutex::new({
                // The worker mints TabId(1) for the initial open_tab fired
                // from spawn's idle handler. Pre-record the display URL so
                // the omnibar shows `buffr://new` rather than the
                // translated http://127.0.0.1:.../data: URL from the very
                // first frame. W6: only for schemes we actually rewrite —
                // a plain https:// homepage must track live navigations.
                let mut m = HashMap::new();
                if should_record_display_url(options.initial_url) {
                    m.insert(TabId(1), options.initial_url.to_owned());
                }
                m
            }),
            is_loading_atomic,
            zoom_level,
            hint_alphabet,
            hint_session: Mutex::new(None),
            context_menu_sink,
            favicon_sink,
            can_go_back,
            can_go_forward,
            clipboard_reader: super::clipboard::WebKitClipboardReader::new(),
            permissions_queue,
            pending_permissions,
            permission_next_id,
            audio_event_queue,
            cursor_state,
            video_active,
            edit_sink,
            wayland_handles: Mutex::new(None),
            using_native,
        })
    }

    /// Attach a shared [`InternalServer`] so future `buffr://*` navigations
    /// resolve to authenticated localhost HTTP URLs instead of opaque
    /// `data:` URLs. Idempotent; later calls replace the previous server.
    pub fn set_internal_server(&self, server: Arc<InternalServer>) {
        if let Ok(mut guard) = self.internal_server.lock() {
            *guard = Some(server);
        }
    }

    /// Wire the host-side `buffr://new` HTML provider so future buffr:// loads
    /// pick up live keybind / palette / splash content. Safe to call multiple
    /// times — overrides the previous provider.
    pub fn set_newtab_html_provider(&self, provider: NewTabHtmlProvider) {
        if let Ok(mut guard) = self.newtab_html_provider.lock() {
            *guard = Some(provider);
        }
    }

    /// Wire the edit-mode event sink so `buffrEdit` UCM messages emitted by
    /// the injected `edit.js` are pushed into the apps layer's queue.
    ///
    /// Safe to call after construction (the worker's first `open_tab` fires
    /// from a GLib idle handler — i.e. after the current tick — so this setter
    /// always wins the race). Safe to call multiple times — replaces previous.
    pub fn set_edit_sink(&self, sink: EditEventSink) {
        if let Ok(mut g) = self.edit_sink.lock() {
            *g = Some(sink);
        }
    }

    /// Store raw Wayland + EGL handles from the host window (#151).
    ///
    /// Called by `buffr-app` after construction on Wayland sessions.  The
    /// handles are held here until the `BuffrDisplayWayland` C subclass (#152)
    /// reads them to open a shared `wl_display` connection.  No-op on
    /// non-Wayland sessions (apps layer gates the call on the display handle
    /// variant check).  Safe to call multiple times — replaces previous value.
    pub fn set_native_wayland_handles(&self, handles: buffr_engine::WaylandNativeHandles) {
        if let Ok(mut g) = self.wayland_handles.lock() {
            *g = Some(handles);
        }
        tracing::info!("webkit: native wayland handles received");
    }

    /// Translate a `buffr://` URL into something the engine can actually
    /// load. Prefers the shared [`InternalServer`] when one is attached
    /// (real HTTP origin, supports fetch/modules/CSS imports). When no server
    /// is attached, the URL is returned unchanged — the engine will fail
    /// visibly instead of masking the error with a data: URL. Non-buffr
    /// URLs are returned untouched.
    fn resolve_url(&self, url: &str) -> String {
        if let Some(rest) = url.strip_prefix("buffr://") {
            // Route everything past `buffr://` straight to the server. The
            // route table on the server side is what determines whether
            // `/<rest>` resolves to a known page or 404.
            if let Ok(guard) = self.internal_server.lock()
                && let Some(server) = guard.as_ref()
            {
                return server.url_for(&format!("/{rest}"));
            }
            // Server not attached — let the engine fail visibly rather than
            // masking the bind failure with a data: URL.
        }
        url.to_owned()
    }

    /// Remember that `tab_id` was opened with the display URL `original`.
    /// The omnibar reads this back via `active_tab_live_url` so users see
    /// `buffr://new` instead of the `http://127.0.0.1:.../…` or `data:…`
    /// that WebKit actually loaded.
    fn record_display_url(&self, tab_id: TabId, original: &str) {
        if let Ok(mut guard) = self.display_urls.lock() {
            guard.insert(tab_id, original.to_owned());
        }
    }

    /// Record `url` as `tab_id`'s display override when the scheme warrants
    /// one, otherwise drop any override the tab already carried.
    /// See [`should_record_display_url`] (W6).
    fn sync_display_url(&self, tab_id: TabId, url: &str) {
        if should_record_display_url(url) {
            self.record_display_url(tab_id, url);
        } else {
            self.forget_display_url(tab_id);
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

    /// [`TabId`] of the currently active tab, if any. `None` when there is no
    /// active tab or the engine-state mutex is poisoned.
    fn active_tab_id(&self) -> Option<TabId> {
        self.worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| t.id))
    }

    /// Apply our per-tab display URL on top of a worker-built [`TabSummary`].
    /// See [`apply_display_overrides_pure`] for the substitution rules; this
    /// method just wires the per-tab lookup.
    fn apply_display_overrides(&self, summary: TabSummary) -> TabSummary {
        let display = self.display_url_for(summary.id);
        apply_display_overrides_pure(summary, display.as_deref())
    }

    /// Inject hint.js into the active tab and initialise a `HintSession`.
    ///
    /// Mirrors `WebKitGtkEngine::enter_hint_mode` 1:1.
    ///
    /// H5: mints a fresh hint nonce for the active tab on every entry, so a
    /// nonce leaked to a hostile top-level document during one hint session is
    /// dead by the next one. `hint.js` is evaluated via
    /// `webkit_web_view_evaluate_javascript`, which targets the main frame in
    /// the default JS world — subframes never receive it, and therefore never
    /// learn the nonce.
    fn enter_hint_mode(&self, background: bool) {
        const LABEL_BUDGET: usize = 256;
        let Some(active) = self.active_tab_id() else {
            tracing::warn!("webkit: enter_hint_mode with no active tab");
            return;
        };
        let nonce = self.worker.console_nonces.rotate_hint(active.0 as i32);
        let labels = self.hint_alphabet.labels_for(LABEL_BUDGET);
        let alphabet_str = self.hint_alphabet.as_string();
        let script = build_inject_script(&alphabet_str, &labels, DEFAULT_HINT_SELECTORS, &nonce);

        let alphabet = self.hint_alphabet.clone();
        if let Ok(mut g) = self.hint_session.lock() {
            *g = Some(HintSession::new(alphabet, Vec::new(), background));
        }
        self.send(Command::EvalJs { script });
        tracing::info!(
            background,
            label_budget = LABEL_BUDGET,
            "webkit: hint mode injected"
        );
    }

    /// Send a fire-and-forget command to the worker thread.
    fn send(&self, cmd: Command) {
        if let Err(e) = self.worker.cmd_tx.try_send(cmd) {
            tracing::warn!("webkit: command send error: {e}");
        }
    }

    /// Open a tab synchronously via a reply channel. Records the original
    /// (untranslated) URL against the minted [`TabId`] so omnibar reads
    /// stay in `buffr://` space.
    ///
    /// `background = true` creates the tab without switching to it (no
    /// `active_idx` change, no `is_loading_atomic` update).
    fn open_tab_sync(&self, url: &str, background: bool) -> Result<TabId, EngineError> {
        let original = url.to_owned();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::OpenTab {
            url: self.resolve_url(url),
            reply: reply_tx,
            background,
        });
        let tab_id = reply_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| EngineError::Other("open_tab timed out".into()))?
            .map_err(EngineError::Other)?;
        self.sync_display_url(tab_id, &original);
        Ok(tab_id)
    }
}

// ── Pure helpers (extracted for unit testing) ────────────────────────────────

/// Prefix for buffr's view-source scheme. Mirrors
/// `buffr_cef::host::BUFFR_SRC_PREFIX` (that crate is not a dependency here).
pub(crate) const BUFFR_SRC_PREFIX: &str = "buffr-src:";

/// Should `url` be kept as a per-tab *display* override?
///
/// W6: only buffr's own schemes get an override. Those are rewritten before
/// they reach WebKit (`buffr://new` → `http://127.0.0.1:<port>/<token>/new`),
/// so the loaded URL is unusable in the omnibar and the typed form has to be
/// stashed. Every other scheme is loaded verbatim, and pinning the typed URL
/// there froze the omnibar on the entry URL forever — an in-page link click
/// still reported the original address because the entry was only dropped on
/// tab close.
///
/// Mirrors `buffr-cef`'s record/forget policy in `BrowserHost::navigate` /
/// `open_tab`.
pub(crate) fn should_record_display_url(url: &str) -> bool {
    url.starts_with("buffr://") || url.starts_with(BUFFR_SRC_PREFIX)
}

/// Apply a display URL on top of a [`TabSummary`].
///
/// Rules:
/// - `display.is_none()`: return `summary` untouched.
/// - `summary.url` is replaced with `display`.
/// - `summary.title` is replaced with `display` *only* when it still looks
///   like the engine-set placeholder (empty, equal to the previous
///   loaded URL, or already equal to the display URL). Once WebKit's
///   `notify::title` lands a real page title we keep it.
pub(crate) fn apply_display_overrides_pure(
    mut summary: TabSummary,
    display: Option<&str>,
) -> TabSummary {
    let Some(display) = display else {
        return summary;
    };
    let title_is_placeholder =
        summary.title.is_empty() || summary.title == summary.url || summary.title == display;
    summary.url = display.to_owned();
    if title_is_placeholder {
        summary.title = display.to_owned();
    }
    summary
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for WebKitEngine {
    // ── Lifecycle ─────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("webkit: close_all_browsers — sending Shutdown");
        self.send(Command::Shutdown);
    }

    // ── Tabs ──────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        self.open_tab_sync(url, false)
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError> {
        self.open_tab_sync(url, true)
    }

    fn open_tab_at(&self, url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        // Known limitation: insert_idx is ignored; new tab is always appended.
        self.open_tab_sync(url, false)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!(?id, "webkit: close_tab");
        self.forget_display_url(id);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::CloseTab {
            id,
            reply: reply_tx,
        });
        Ok(reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or(false))
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        // Drop the active tab's display-URL stash now; if close_active
        // succeeds the tab is gone, and if it fails the stash is no worse
        // than slightly stale.
        if let Some(active) = self
            .worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| t.id))
        {
            self.forget_display_url(active);
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::CloseActive { reply: reply_tx });
        Ok(reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or(false))
    }

    fn select_tab(&self, id: TabId) {
        self.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        let next = self.worker.engine_state.lock().ok().and_then(|st| {
            let idx = st.active_idx?;
            let len = st.tabs.len();
            if len == 0 {
                return None;
            }
            let new_idx = (idx + 1) % len;
            st.tabs.get(new_idx).map(|t| t.id)
        });
        if let Some(id) = next {
            self.send(Command::SelectTab { id });
        }
    }

    fn prev_tab(&self) {
        let prev = self.worker.engine_state.lock().ok().and_then(|st| {
            let idx = st.active_idx?;
            let len = st.tabs.len();
            if len == 0 {
                return None;
            }
            let new_idx = if idx == 0 { len - 1 } else { idx - 1 };
            st.tabs.get(new_idx).map(|t| t.id)
        });
        if let Some(id) = prev {
            self.send(Command::SelectTab { id });
        }
    }

    fn move_tab(&self, from: usize, to: usize) {
        self.send(Command::MoveTab { from, to });
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        let url = self.active_tab_live_url();
        self.open_tab(&url)
    }

    fn toggle_pin_active(&self) {}
    fn set_pinned(&self, _id: TabId, _pinned: bool) {}

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        Ok(None)
    }

    fn closed_stack_len(&self) -> usize {
        0
    }

    fn active_tab(&self) -> Option<TabSummary> {
        let st = self.worker.engine_state.lock().ok()?;
        let info = st.active_tab_info()?;
        Some(self.apply_display_overrides(info.to_summary()))
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        let summaries: Vec<TabSummary> = self
            .worker
            .engine_state
            .lock()
            .map(|st| st.tabs_summary())
            .unwrap_or_default();
        summaries
            .into_iter()
            .map(|s| self.apply_display_overrides(s))
            .collect()
    }

    fn tab_count(&self) -> usize {
        self.worker
            .engine_state
            .lock()
            .map(|st| st.tabs.len())
            .unwrap_or(0)
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        self.worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_idx)
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn is_loading(&self) -> bool {
        // Read the atomic the worker thread flips on load-changed.
        // Bypasses engine_state so the buffr-app animation gate can't
        // get pinned to true by mutex contention.
        self.is_loading_atomic.load(Ordering::SeqCst)
    }

    fn can_go_back(&self) -> bool {
        self.can_go_back.load(Ordering::Relaxed)
    }

    fn can_go_forward(&self) -> bool {
        self.can_go_forward.load(Ordering::Relaxed)
    }

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        // Mark loading immediately so the next paint sees the loading
        // animation while the new page fetches. The load-changed signal
        // will flip it back to false on WEBKIT_LOAD_FINISHED.
        self.is_loading_atomic.store(true, Ordering::SeqCst);
        // Update the per-tab display-URL stash before dispatching so the
        // omnibar reads the human-readable URL even if the navigation is
        // still in flight when the next paint queries us.
        if let Some(active) = self
            .worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| t.id))
        {
            // W6: buffr:// / buffr-src: keep the typed form (the loaded URL
            // is the rewritten loopback address); every other scheme clears
            // any stale override so in-page navigations are reported live.
            self.sync_display_url(active, url);
        }
        self.send(Command::Navigate {
            url: self.resolve_url(url),
        });
        Ok(())
    }

    fn active_tab_live_url(&self) -> String {
        // Prefer the user-typed display URL (e.g. `buffr://new`) over the
        // engine-loaded URL (e.g. the localhost+token URL or data: blob).
        let active = self
            .worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| (t.id, t.url.clone())));
        match active {
            Some((id, loaded)) => self.display_url_for(id).unwrap_or(loaded),
            None => String::new(),
        }
    }

    fn pump_address_changes(&self) -> bool {
        let changed = self
            .worker
            .engine_state
            .lock()
            .map(|mut st| {
                let c = st.address_changed;
                st.address_changed = false;
                c
            })
            .unwrap_or(false);
        if changed {
            let url = self.active_tab_live_url();
            if let Ok(mut lu) = self.live_url.lock() {
                *lu = url;
            }
        }
        changed
    }

    // ── Viewport ──────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.send(Command::Resize { width, height });
    }

    fn set_device_scale(&self, scale: f32) {
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, hz: u32) {
        self.view.frame_rate_hz.store(hz, Ordering::Relaxed);
    }

    fn notify_screen_info_changed(&self) {}

    fn osr_resize(&self, width: u32, height: u32) {
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.send(Command::OsrResize { width, height });
    }

    // ── Input ─────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        if let Some(ev) = neutral_key_to_wpe(event) {
            self.send(Command::KeyEvent { ev });
        }
    }

    fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        self.send(Command::MouseMove { x, y, modifiers });
    }

    fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: MouseButton,
        mouse_up: bool,
        _click_count: i32,
        modifiers: u32,
    ) {
        let btn = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Other(n) => n as u32,
        };
        self.send(Command::MouseClick {
            x,
            y,
            button: btn,
            pressed: !mouse_up,
            modifiers,
        });
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        // WPE has no explicit mouse-leave — send a motion to (-1, -1) convention.
        self.send(Command::MouseMove {
            x: -1,
            y: -1,
            modifiers: 0,
        });
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        self.send(Command::MouseWheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        });
    }

    fn osr_focus(&self, _focused: bool) {
        self.send(Command::Focus);
    }

    // ── OSR state ─────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        self.send(Command::ForceRepaintActive);
    }

    fn osr_sleep(&self, sleep: bool) {
        self.send(Command::OsrSleep { sleep });
    }

    fn osr_invalidate_view(&self) {}

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
    }

    // ── Find / zoom ───────────────────────────────────────────────────────────

    fn start_find(&self, query: &str, forward: bool) {
        self.send(Command::StartFind {
            query: query.to_owned(),
            forward,
        });
    }

    fn stop_find(&self) {
        self.send(Command::StopFind);
    }

    /// Run JavaScript in the active tab's main frame.
    ///
    /// `_url` is the DevTools source attribution — useful for stack traces
    /// but not load-bearing. We discard it; WebKit's `evaluate_javascript`
    /// accepts a source URI param but our current `Command::EvalJs` doesn't
    /// carry it, and the consumers (splash anim tick, edit IPC) work fine
    /// without it. Upgrade to carry the URI if a future caller needs proper
    /// source attribution in the inspector.
    ///
    /// Fire-and-forget: dispatched to the GLib worker via the same path as
    /// vim-style scroll JS, so a failed eval is logged but not surfaced.
    /// Returns Ok unconditionally — the only failure mode would be a NUL
    /// byte in the script, which the worker logs but doesn't propagate.
    fn run_main_frame_js(&self, code: &str, _url: &str) -> Result<(), EngineError> {
        self.send(Command::EvalJs {
            script: code.to_owned(),
        });
        Ok(())
    }

    /// Execute `code` in the active tab's main frame.
    ///
    /// Dispatches `Command::EvalJs` — same path as `run_main_frame_js` but
    /// without a source URI param. They intentionally stay separate so source
    /// URI threading can be added to each independently in the future.
    fn run_js(&self, code: &str) -> Result<(), EngineError> {
        self.send(Command::EvalJs {
            script: code.to_owned(),
        });
        Ok(())
    }

    // ── Frame editing commands ────────────────────────────────────────────────

    fn frame_undo(&self) {
        self.send(Command::ExecEditing { command: "Undo" });
    }

    fn frame_redo(&self) {
        self.send(Command::ExecEditing { command: "Redo" });
    }

    fn frame_cut(&self) {
        self.send(Command::ExecEditing { command: "Cut" });
    }

    fn frame_copy(&self) {
        self.send(Command::ExecEditing { command: "Copy" });
    }

    fn frame_paste(&self) {
        self.send(Command::ExecEditing { command: "Paste" });
    }

    fn frame_paste_plain(&self) {
        self.send(Command::ExecEditing {
            command: "PasteAsPlainText",
        });
    }

    fn frame_select_all(&self) {
        self.send(Command::ExecEditing {
            command: "SelectAll",
        });
    }

    // ── Media controls (#131) ────────────────────────────────────────────────
    //
    // Each helper walks up the DOM from elementFromPoint(x, y) to find the
    // nearest HTMLMediaElement, falls back to the first <video>/<audio> on
    // the page, then toggles the relevant property. Mirrors CEF's impl —
    // dispatched via run_main_frame_js → Command::EvalJs.

    fn media_play_pause(&self, x: i32, y: i32) {
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               if(el.paused)el.play();else el.pause();\
             }})({x},{y});"
        );
        let _ = self.run_main_frame_js(&js, "buffr://context-menu");
    }

    fn media_toggle_mute(&self, x: i32, y: i32) {
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.muted=!el.muted;\
             }})({x},{y});"
        );
        let _ = self.run_main_frame_js(&js, "buffr://context-menu");
    }

    fn media_toggle_loop(&self, x: i32, y: i32) {
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.loop=!el.loop;\
             }})({x},{y});"
        );
        let _ = self.run_main_frame_js(&js, "buffr://context-menu");
    }

    fn media_toggle_controls(&self, x: i32, y: i32) {
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               while(el&&!(el instanceof HTMLMediaElement))el=el.parentElement;\
               if(!el)el=document.querySelector('video, audio');\
               if(!el)return;\
               el.controls=!el.controls;\
             }})({x},{y});"
        );
        let _ = self.run_main_frame_js(&js, "buffr://context-menu");
    }

    fn media_picture_in_picture(&self, x: i32, y: i32) {
        // try/catch because some hosts disable PiP via Permissions Policy
        // or the element lacks the WebKit prefix-free PiP API on older
        // WebViews; we don't want a thrown exception to break the menu.
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
        let _ = self.run_main_frame_js(&js, "buffr://context-menu");
    }

    fn active_zoom_level(&self) -> f64 {
        self.zoom_level.lock().map(|g| *g).unwrap_or(1.0)
    }

    fn zoom_in(&self) {
        self.send(Command::Zoom { delta: 0.1 });
    }

    fn zoom_out(&self) {
        self.send(Command::Zoom { delta: -0.1 });
    }

    fn zoom_reset(&self) {
        self.send(Command::Zoom { delta: 0.0 });
    }

    fn open_devtools(&self, _tab: TabId) -> Result<(), EngineError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::OpenDevtools { reply: reply_tx });
        reply_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .map_err(|_| EngineError::Other("open_devtools timed out".into()))?
            .map_err(EngineError::Other)
    }

    /// "Inspect Element" — open devtools and scroll the element at (x, y)
    /// into view so the user sees the target node right away.
    ///
    /// WPE WebKit 2.52 only exposes `webkit_web_view_toggle_inspector` (no
    /// positional inspect API), so we (a) reuse `Command::OpenDevtools` to
    /// toggle the inspector on, and (b) inject a JS snippet that finds the
    /// element under the coordinates and scrolls it + flashes a brief outline
    /// so the user can locate it in the page. The inspector itself doesn't
    /// auto-select the node — that's the remaining capability gap vs CEF's
    /// `ShowDevToolsAt`, but acceptable for v1.
    fn show_dev_tools_at(&self, x: i32, y: i32) {
        // Fire-and-forget: open devtools (toggle), then highlight the target.
        // Drop the reply channel — we don't need to know if the toggle
        // succeeded, just trigger it. Future: a separate, non-reply variant.
        let (reply_tx, _reply_rx) = mpsc::sync_channel(1);
        self.send(Command::OpenDevtools { reply: reply_tx });
        let js = format!(
            "(function(x,y){{\
               var el=document.elementFromPoint(x,y);\
               if(!el)return;\
               try{{el.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}});}}\
               catch(_){{el.scrollIntoView();}}\
               var prev=el.style.outline;\
               el.style.outline='2px solid #ff00aa';\
               setTimeout(function(){{el.style.outline=prev;}},1500);\
             }})({x},{y});"
        );
        let _ = self.run_main_frame_js(&js, "buffr://inspect-at");
    }

    // ── Edit overlay DOM IPC (#134) ───────────────────────────────────────────
    //
    // Each method runs a one-liner in the active tab's main frame that invokes
    // the globals installed by `edit.js` (injected at document-end via the
    // buffrEdit UCM bridge). Mirrors CEF's `run_edit_attach/focus/detach/cycle`
    // impls — only the JS execution path differs (EvalJs command vs CEF's
    // synchronous frame.execute_java_script).

    fn run_edit_attach(&self, field_id: &str) {
        let escaped = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        let _ = self.run_main_frame_js(
            &format!("if (window.__buffrEditAttach) window.__buffrEditAttach({escaped})"),
            "buffr://edit",
        );
    }

    fn run_edit_focus(&self, field_id: &str) {
        let escaped = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        let _ = self.run_main_frame_js(
            &format!(
                "window.__buffrUserGesture = true; \
                 if (window.__buffrEditFocus) window.__buffrEditFocus({escaped})"
            ),
            "buffr://edit",
        );
    }

    fn run_edit_cycle(&self, forward: bool) {
        let arg = if forward { "true" } else { "false" };
        let _ = self.run_main_frame_js(
            &format!("if (window.__buffrCycleInput) window.__buffrCycleInput({arg})"),
            "buffr://edit",
        );
    }

    fn run_edit_detach(&self, field_id: &str) {
        let escaped = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".to_string());
        let _ = self.run_main_frame_js(
            &format!("if (window.__buffrEditDetach) window.__buffrEditDetach({escaped})"),
            "buffr://edit",
        );
    }

    // ── Audio / video ─────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        self.worker
            .engine_state
            .lock()
            .map(|st| st.audio_active.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    fn any_video_active(&self) -> bool {
        self.video_active.load(Ordering::Relaxed)
    }

    fn run_media_probe(&self) {
        self.send(Command::RunMediaProbe);
    }

    fn drain_audio_events(&self) -> Vec<buffr_engine::AudioEvent> {
        if let Ok(mut q) = self.audio_event_queue.lock() {
            q.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    // ── Cursor tracking (#137) ────────────────────────────────────────────────

    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        self.cursor_state.take()
    }

    // ── Permissions (#138) ────────────────────────────────────────────────────

    fn permissions_queue(&self) -> PermissionsQueue {
        self.permissions_queue.clone()
    }

    fn resolve_permission(&self, resolve_id: Option<&str>, outcome: PromptOutcome) {
        let Some(id) = resolve_id else {
            tracing::debug!("webkit: resolve_permission called with no resolve_id (no-op)");
            return;
        };
        self.send(Command::ResolvePermission {
            resolve_id: id.to_owned(),
            outcome,
        });
    }

    // ── Popup sinks ───────────────────────────────────────────────────────────

    fn popup_queue(&self) -> PopupQueue {
        self.popup_queue.clone()
    }

    fn popup_create_sink(&self) -> PopupCreateSink {
        self.popup_create_sink.clone()
    }

    fn popup_close_sink(&self) -> PopupCloseSink {
        self.popup_close_sink.clone()
    }

    fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {}
    fn popup_close(&self, _browser_id: i32) {}

    fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        Vec::new()
    }

    fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        Vec::new()
    }

    fn popup_history_back(&self, _browser_id: i32) {}
    fn popup_history_forward(&self, _browser_id: i32) {}
    fn popup_osr_focus(&self, _browser_id: i32, _focused: bool) {}

    // ── Context menu ──────────────────────────────────────────────────────────

    fn drain_context_menu_requests(&self) -> Vec<ContextMenuRequest> {
        match self.context_menu_sink.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
    fn popup_osr_key_event(&self, _browser_id: i32, _event: NeutralKeyEvent) {}

    fn popup_osr_mouse_click(
        &self,
        _browser_id: i32,
        _x: i32,
        _y: i32,
        _button: MouseButton,
        _mouse_up: bool,
        _click_count: i32,
        _modifiers: u32,
    ) {
    }

    fn popup_osr_mouse_move(&self, _browser_id: i32, _x: i32, _y: i32, _modifiers: u32) {}

    fn popup_osr_mouse_wheel(
        &self,
        _browser_id: i32,
        _x: i32,
        _y: i32,
        _delta_x: i32,
        _delta_y: i32,
        _modifiers: u32,
    ) {
    }

    // ── Favicon ───────────────────────────────────────────────────────────────

    fn drain_favicon_updates(&self) -> Vec<buffr_engine::FaviconUpdate> {
        buffr_core::favicon::drain_favicon_updates(&self.favicon_sink)
            .into_iter()
            .map(|u| buffr_engine::FaviconUpdate {
                browser_id: u.browser_id,
                width: u.width,
                height: u.height,
                pixels: u.pixels,
            })
            .collect()
    }

    // ── Hint mode ─────────────────────────────────────────────────────────────

    fn is_hint_mode(&self) -> bool {
        self.hint_session
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    fn hint_status(&self) -> Option<HintStatus> {
        let g = self.hint_session.lock().ok()?;
        let s = g.as_ref()?;
        Some(HintStatus {
            typed: s.typed.clone(),
            match_count: s.match_count(),
            background: s.background,
        })
    }

    fn pump_hint_events(&self) -> bool {
        let Some(event) = take_hint_event(&self.worker.hint_sink) else {
            return false;
        };
        match event {
            HintConsoleEvent::Ready { hints, alphabet: _ } => {
                let alphabet = self.hint_alphabet.clone();
                if let Ok(mut g) = self.hint_session.lock()
                    && let Some(existing) = g.as_mut()
                {
                    let background = existing.background;
                    *existing = HintSession::new(alphabet, hints, background);
                }
                true
            }
            HintConsoleEvent::Error { message } => {
                tracing::warn!(message, "webkit: hint mode renderer error");
                self.cancel_hint();
                true
            }
        }
    }

    fn feed_hint_key(&self, ch: char) -> Option<HintAction> {
        let mut commit_id: Option<u32> = None;
        let mut filter_typed: Option<String> = None;
        let mut clear = false;
        let mut cancel = false;

        let action = {
            let mut g = self.hint_session.lock().ok()?;
            let session = g.as_mut()?;
            let action = session.feed(ch);
            let typed = session.typed.clone();
            match &action {
                HintAction::Filter => filter_typed = Some(typed),
                HintAction::Click(id) | HintAction::OpenInBackground(id) => {
                    commit_id = Some(*id);
                    clear = true;
                }
                HintAction::Cancel => cancel = true,
            }
            action
        };

        if let Some(typed) = filter_typed {
            let js = format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                serde_json::to_string(&typed).unwrap_or_else(|_| "\"\"".into())
            );
            self.send(Command::EvalJs { script: js });
        }
        if let Some(id) = commit_id {
            let js = format!("if (window.__buffrHintCommit) window.__buffrHintCommit({id})");
            self.send(Command::EvalJs { script: js });
        }
        if clear && let Ok(mut g) = self.hint_session.lock() {
            *g = None;
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    fn backspace_hint(&self) -> Option<HintAction> {
        let mut filter_typed: Option<String> = None;
        let mut cancel = false;

        let action = {
            let mut g = self.hint_session.lock().ok()?;
            let session = g.as_mut()?;
            let action = session.backspace();
            let typed = session.typed.clone();
            match &action {
                HintAction::Filter => filter_typed = Some(typed),
                HintAction::Cancel => cancel = true,
                _ => {}
            }
            action
        };

        if let Some(typed) = filter_typed {
            let js = format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                serde_json::to_string(&typed).unwrap_or_else(|_| "\"\"".into())
            );
            self.send(Command::EvalJs { script: js });
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    fn cancel_hint(&self) {
        self.send(Command::EvalJs {
            script: "if (window.__buffrHintCancel) window.__buffrHintCancel()".into(),
        });
        if let Ok(mut g) = self.hint_session.lock() {
            *g = None;
        }
    }

    /// Vim-style PageAction dispatcher. CEF implements this as a big
    /// match that pokes the browser host directly. For WPE we route the
    /// few user-visible variants (scrolling, history, reload) through
    /// methods we already have (`Navigate`, `EvalJs`, `webkit_web_view_*`).
    /// Anything we don't recognise stays a no-op — the buffr-app side
    /// already routes Tab*, Zoom*, OpenDevTools, EnterMode etc. through
    /// dedicated trait methods.
    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;
        tracing::debug!(?action, "webkit: dispatch");
        // Per-action JS snippet. `n` is the count multiplier from the
        // keymap engine (e.g. `5j` → ScrollDown(5)). Pixel-per-line and
        // page fraction match Chromium / Firefox defaults so user
        // muscle memory carries over.
        let script: Option<String> = match action {
            A::ScrollDown(n) => Some(format!("window.scrollBy(0, {});", scroll_lines_to_px(*n))),
            A::ScrollUp(n) => Some(format!("window.scrollBy(0, -{});", scroll_lines_to_px(*n))),
            A::ScrollRight(n) => Some(format!("window.scrollBy({}, 0);", scroll_lines_to_px(*n))),
            A::ScrollLeft(n) => Some(format!("window.scrollBy(-{}, 0);", scroll_lines_to_px(*n))),
            A::ScrollPageDown => Some("window.scrollBy(0, window.innerHeight - 60);".into()),
            A::ScrollPageUp => Some("window.scrollBy(0, -(window.innerHeight - 60));".into()),
            A::ScrollFullPageDown => Some("window.scrollBy(0, window.innerHeight);".into()),
            A::ScrollFullPageUp => Some("window.scrollBy(0, -window.innerHeight);".into()),
            A::ScrollHalfPageDown => Some("window.scrollBy(0, window.innerHeight / 2);".into()),
            A::ScrollHalfPageUp => Some("window.scrollBy(0, -window.innerHeight / 2);".into()),
            A::ScrollTop => Some("window.scrollTo(window.scrollX, 0);".into()),
            A::ScrollBottom => Some(
                "window.scrollTo(window.scrollX, document.documentElement.scrollHeight);".into(),
            ),
            _ => None,
        };
        if let Some(script) = script {
            self.send(Command::EvalJs { script });
            return;
        }
        // History + reload have dedicated WebKit API on the WebView,
        // but they're set on the worker thread so route via a new
        // EvalJs that calls history.go(). Saves a Command variant per
        // history slot and keeps semantics identical to the page's own
        // `back` button.
        match action {
            A::HistoryBack => self.send(Command::EvalJs {
                script: "history.back();".into(),
            }),
            A::HistoryForward => self.send(Command::EvalJs {
                script: "history.forward();".into(),
            }),
            A::Reload => self.send(Command::EvalJs {
                script: "location.reload();".into(),
            }),
            A::ReloadHard => self.send(Command::EvalJs {
                script: "location.reload();".into(),
            }),
            A::StopLoading => self.send(Command::EvalJs {
                script: "window.stop();".into(),
            }),
            A::EnterHintMode => self.enter_hint_mode(false),
            A::EnterHintModeBackground => self.enter_hint_mode(true),
            A::FocusFirstInput => {
                // edit.js's focusin handler blurs any focus that arrives
                // without a user-gesture flag set, so flip the flag
                // before injecting the focus script — otherwise the
                // page would self-cancel the focus we just requested.
                self.send(Command::EvalJs {
                    script: "window.__buffrUserGesture = true;".into(),
                });
                self.send(Command::EvalJs {
                    script: buffr_core::scripts::FOCUS_FIRST_INPUT.into(),
                });
            }
            A::ExitInsertMode => {
                // Blur whatever the page has focused.  The DOM blur
                // propagates to edit.js, which posts an `edit:blur`
                // console event that drain_edit_focus_events consumes
                // on the apps layer.
                self.send(Command::EvalJs {
                    script: buffr_core::scripts::EXIT_INSERT.into(),
                });
            }
            _ => tracing::debug!(?action, "webkit: dispatch: no mapping yet"),
        }
    }

    // ── Clipboard ─────────────────────────────────────────────────────────────

    fn clipboard_handle(&self) -> Option<buffr_engine::ClipboardReader> {
        self.clipboard_reader
            .clone()
            .map(|arc| arc as buffr_engine::ClipboardReader)
    }

    fn clipboard_text(&self) -> Option<String> {
        self.clipboard_reader.as_ref().and_then(|r| r.read_text())
    }

    fn clipboard_set_text(&self, text: &str) -> bool {
        use hjkl_clipboard::{MimeType, Selection};
        let Some(reader) = self.clipboard_reader.as_ref() else {
            return false;
        };
        match reader
            .inner
            .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
        {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "webkit: clipboard_set_text failed");
                false
            }
        }
    }

    // ── Downloads / image copy ────────────────────────────────────────────────

    fn start_download(&self, url: &str) {
        self.send(Command::StartDownload {
            url: url.to_owned(),
        });
    }

    fn copy_image_url_to_clipboard(&self, url: &str) {
        buffr_core::image_copy::copy_image_to_clipboard(url.to_owned());
    }

    // ── Native compositing (#143) ────────────────────────────────────────────
    //
    // Whether the native compositing path is actually live is reported by
    // is_using_native_compositing, which reflects whether WPEDisplayWayland
    // was actually constructed vs the OSR fallback.

    fn is_using_native_compositing(&self) -> bool {
        // Published by the GLib worker after WpeRuntime::new chose its
        // display backend.  True only when BuffrDisplayWayland (#152) or
        // stock WPEDisplayWayland was actually constructed; false when
        // the runtime fell back to the OSR readback path.
        self.using_native.load(Ordering::Relaxed)
    }

    // ── IME composition (#IME) ────────────────────────────────────────────────

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        self.send(Command::ImeSetComposition {
            text: text.to_owned(),
            cursor,
        });
    }

    fn ime_commit(&self, text: &str) {
        self.send(Command::ImeCommit {
            text: text.to_owned(),
        });
    }

    fn ime_cancel(&self) {
        self.send(Command::ImeCancel);
    }
}

/// 60 px per "line" of vim-style scroll, matching Chromium/Firefox
/// default `Smooth Scrolling` wheel deltas. Bound to a sensible minimum
/// so `5j` always feels responsive even on tall pages.
fn scroll_lines_to_px(count: u32) -> u32 {
    count.saturating_mul(60).max(60)
}

/// Resolve the XDG data home for the cookie-path fallback branch.
/// `$XDG_DATA_HOME` wins; `$HOME/.local/share` is the freedesktop
/// default; `.` (cwd) is the last-resort when neither env var is set
/// (e.g. tiny init container, headless test).  Returned as an owned
/// `PathBuf` rather than a `&Path` so the caller can hold it across
/// multiple `compute_cookie_db_path` calls without re-querying env.
fn compute_xdg_data_home() -> Option<std::path::PathBuf> {
    if let Ok(v) = std::env::var("XDG_DATA_HOME")
        && !v.is_empty()
    {
        return Some(std::path::PathBuf::from(v));
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return Some(std::path::PathBuf::from(h).join(".local/share"));
    }
    None
}

/// Compute the on-disk cookie SQLite path for a webkit engine instance.
///
/// `data_dir` is the per-engine namespace that `apps/buffr-app` builds
/// from its config (canonically `<data_root>/engines/<id>/profile/`);
/// it is ALREADY namespaced, so we only append `cookies.sqlite` to it.
/// Re-namespacing inside this function is what produced the doubled
/// path `engines/<id>/profile/engines/<id>/cookies.sqlite` that
/// libsoup logged as "Can't open …" in production.
///
/// `xdg_fallback` is the resolved XDG data home (typically
/// `$HOME/.local/share`) used when `data_dir` is unset — this branch
/// must namespace under `buffr/engines/<id>/` itself because the
/// fallback is shared across engines.
///
/// Returns `None` when the chosen path is not valid UTF-8 (libsoup's
/// C API requires `*const c_char`), in which case the engine logs a
/// warning and runs with in-memory cookies.
fn compute_cookie_db_path(
    data_dir: Option<&std::path::Path>,
    engine_id: &str,
    xdg_fallback: Option<&std::path::Path>,
) -> Option<String> {
    let path: std::path::PathBuf = match data_dir {
        Some(d) => d.join("cookies.sqlite"),
        None => {
            let base = xdg_fallback
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            base.join("buffr")
                .join("engines")
                .join(engine_id)
                .join("cookies.sqlite")
        }
    };
    path.to_str().map(|s| s.to_owned()).or_else(|| {
        tracing::warn!("webkit: cookie DB path is not valid UTF-8 — cookies remain in-memory");
        None
    })
}

/// Pure mapping from the apps-layer's `NeutralKeyEvent` to the WPE
/// `WpeKeyEvent` (or `None` to drop the event).  Extracted out of the
/// `osr_key_event` trait impl so it's unit-testable without spinning up
/// a real GLib worker.
///
/// The apps-layer key encoder emits up to three events per physical
/// keystroke: `RawDown` (keystroke), `Char` (text-bearing for printable
/// keys), `Up` (release).  CEF wants all three.  WPE's
/// `wpe_view_event` surface takes ONE press + ONE release per physical
/// key, so we must drop one of {RawDown, Char} for printables —
/// otherwise WebKit inserts the character twice (regression: typing
/// "h" produced "Hh" because the windows_key_code on RawDown is the
/// uppercase keysym 72='H' while Char carried character=104='h').
///
/// Returns `None` when the event should be dropped at the WPE boundary.
fn neutral_key_to_wpe(event: NeutralKeyEvent) -> Option<WpeKeyEvent> {
    use buffr_engine::KeyEventKind;
    let pressed = match event.kind {
        KeyEventKind::Up => false,
        KeyEventKind::Char => true,
        KeyEventKind::RawDown => {
            // Drop the RawDown for printables — the matching Char
            // event will carry the correct insert.  Pure shortcut
            // RawDown (Esc, Enter, F-keys, modifiers — character==0)
            // still passes through.
            if event.character != 0 || event.unmodified_character != 0 {
                return None;
            }
            true
        }
    };
    // Prefer the Char event's text-bearing `character` over the
    // VK code for the WPE keysym.  Falls back to windows_key_code
    // for non-text events (shortcut keys, releases).
    let key_code = if event.character != 0 {
        event.character as u32
    } else {
        event.windows_key_code as u32
    };
    Some(WpeKeyEvent {
        key_code,
        pressed,
        modifiers: event.modifiers,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use buffr_engine::TabId;

    fn summary(id: u64, url: &str, title: &str) -> TabSummary {
        TabSummary {
            id: TabId(id),
            browser_id: id as i32,
            title: title.to_owned(),
            url: url.to_owned(),
            progress: 0.0,
            is_loading: false,
            pinned: false,
            private: false,
        }
    }

    #[test]
    fn override_swaps_url() {
        // The engine loaded http://127.0.0.1:.../<token>/new but the user
        // asked for buffr://new — the omnibar should show the buffr URL.
        let s = summary(
            1,
            "http://127.0.0.1:1234/abc/new",
            "http://127.0.0.1:1234/abc/new",
        );
        let out = apply_display_overrides_pure(s, Some("buffr://new"));
        assert_eq!(out.url, "buffr://new");
    }

    #[test]
    fn override_swaps_title_when_placeholder_equals_url() {
        // Default TabInfo.title == url right after open_tab; the tab pill
        // would otherwise show the long localhost URL.
        let s = summary(
            1,
            "http://127.0.0.1:1234/abc/new",
            "http://127.0.0.1:1234/abc/new",
        );
        let out = apply_display_overrides_pure(s, Some("buffr://new"));
        assert_eq!(out.title, "buffr://new");
    }

    #[test]
    fn override_swaps_title_when_empty() {
        let s = summary(1, "http://127.0.0.1:1234/abc/new", "");
        let out = apply_display_overrides_pure(s, Some("buffr://new"));
        assert_eq!(out.title, "buffr://new");
    }

    #[test]
    fn override_preserves_real_title() {
        // WebKit eventually fires notify::title with "New Tab" — keep that
        // human title, only swap the URL.
        let s = summary(1, "http://127.0.0.1:1234/abc/new", "New Tab");
        let out = apply_display_overrides_pure(s, Some("buffr://new"));
        assert_eq!(out.title, "New Tab", "must not clobber a real title");
        assert_eq!(out.url, "buffr://new");
    }

    #[test]
    fn override_is_idempotent_when_title_already_matches_display() {
        // Second pass shouldn't change anything.
        let s = summary(1, "buffr://new", "buffr://new");
        let out = apply_display_overrides_pure(s, Some("buffr://new"));
        assert_eq!(out.url, "buffr://new");
        assert_eq!(out.title, "buffr://new");
    }

    #[test]
    fn no_display_returns_input_untouched() {
        let s = summary(1, "https://example.com", "Example");
        let out = apply_display_overrides_pure(s.clone(), None);
        assert_eq!(out.url, s.url);
        assert_eq!(out.title, s.title);
    }

    // ── scroll_lines_to_px ───────────────────────────────────────────────

    #[test]
    fn scroll_lines_single_line() {
        // `j` (count=1) scrolls one line — 60 px matches Chromium default.
        assert_eq!(scroll_lines_to_px(1), 60);
    }

    #[test]
    fn scroll_lines_multi_line() {
        // `5j` → 5×60 = 300 px.
        assert_eq!(scroll_lines_to_px(5), 300);
    }

    #[test]
    fn scroll_lines_zero_count_floors_to_one_line() {
        // Defensive: keymap engine never sends 0, but if it does we still
        // want a visible nudge. Floor matches the single-line case.
        assert_eq!(scroll_lines_to_px(0), 60);
    }

    #[test]
    fn scroll_lines_saturates_on_overflow() {
        // u32::MAX * 60 would overflow; saturating_mul caps at u32::MAX
        // then .max(60) is a no-op. Confirms no panic on attacker-typed
        // huge prefix counts.
        assert_eq!(scroll_lines_to_px(u32::MAX), u32::MAX);
    }

    // ── neutral_key_to_wpe — double-input regression guard ────────────────────

    /// Build a `NeutralKeyEvent` representing the RawDown half of a
    /// printable keystroke.  apps-layer fills `character` /
    /// `unmodified_character` on the RawDown too (mirrors CEF's
    /// `windows_key_code`-driven shape), which is what made the
    /// "Hh" double-input bug subtle.
    fn raw_down(vk: i32, ch: u16) -> NeutralKeyEvent {
        NeutralKeyEvent {
            kind: buffr_engine::KeyEventKind::RawDown,
            windows_key_code: vk,
            native_key_code: 0,
            character: ch,
            unmodified_character: ch,
            modifiers: 0,
            is_system_key: false,
            focus_on_editable_field: true,
        }
    }
    fn char_ev(vk: i32, ch: u16) -> NeutralKeyEvent {
        NeutralKeyEvent {
            kind: buffr_engine::KeyEventKind::Char,
            windows_key_code: vk,
            native_key_code: 0,
            character: ch,
            unmodified_character: ch,
            modifiers: 0,
            is_system_key: false,
            focus_on_editable_field: true,
        }
    }
    fn key_up(vk: i32) -> NeutralKeyEvent {
        NeutralKeyEvent {
            kind: buffr_engine::KeyEventKind::Up,
            windows_key_code: vk,
            native_key_code: 0,
            character: 0,
            unmodified_character: 0,
            modifiers: 0,
            is_system_key: false,
            focus_on_editable_field: true,
        }
    }

    #[test]
    fn neutral_key_to_wpe_drops_printable_rawdown() {
        // Regression: production reproducer.  Typing 'h' delivered
        // RawDown vk=72 ch=104 followed by Char vk=104 ch=104; before
        // the fix, both reached WPE and the field rendered "Hh".  The
        // RawDown of a printable must be filtered at this boundary.
        let dropped = neutral_key_to_wpe(raw_down(72, 104));
        assert!(
            dropped.is_none(),
            "RawDown for a printable character must not reach WPE"
        );
    }

    #[test]
    fn neutral_key_to_wpe_dispatches_char_with_text_keysym() {
        // The Char event carries the actual lowercase code point in
        // `character`.  WPE keysym must come from that, not from
        // windows_key_code (which holds the uppercase VK).
        let ev =
            neutral_key_to_wpe(char_ev(72, 104)).expect("Char event for printable must dispatch");
        assert_eq!(
            ev.key_code, 104,
            "Char dispatch must use the text-bearing character, not the VK"
        );
        assert!(ev.pressed, "Char must dispatch as a press");
    }

    #[test]
    fn neutral_key_to_wpe_passes_shortcut_rawdown_through() {
        // Esc (VK 27) has no text payload; the apps-layer leaves
        // character == 0.  WebKit needs the press to receive Esc, so
        // the RawDown must NOT be dropped.
        let ev = neutral_key_to_wpe(raw_down(27, 0))
            .expect("RawDown for a shortcut key (Esc) must reach WPE");
        assert_eq!(ev.key_code, 27);
        assert!(ev.pressed);
    }

    #[test]
    fn neutral_key_to_wpe_passes_modifier_rawdown_through() {
        // Pure modifier presses (Shift, Ctrl, Alt) come in as
        // RawDown with character == 0.  They must reach WPE so the
        // chord state on the engine side stays in sync with the host.
        let ev = neutral_key_to_wpe(raw_down(16 /* VK_SHIFT */, 0))
            .expect("RawDown for a modifier key must reach WPE");
        assert_eq!(ev.key_code, 16);
        assert!(ev.pressed);
    }

    #[test]
    fn neutral_key_to_wpe_emits_release_on_up() {
        // The Up event has no character payload; we use
        // windows_key_code as the keysym so WebKit can match the
        // release with whichever physical key it was for.
        let ev = neutral_key_to_wpe(key_up(72)).expect("Up event must dispatch");
        assert_eq!(ev.key_code, 72);
        assert!(!ev.pressed, "Up must dispatch as a release");
    }

    // ── compute_cookie_db_path — doubled-path regression guard ────────────────

    #[test]
    fn cookie_path_uses_data_dir_as_is_without_renamespacing() {
        // Regression: apps/buffr-app passes data_dir already namespaced
        // as `<data_root>/engines/<id>/profile/`.  The previous impl
        // appended another `engines/<id>/` on top, producing
        // `…/engines/webkit/profile/engines/webkit/cookies.sqlite`
        // which libsoup couldn't open.  data_dir must be the FINAL
        // namespace — only `cookies.sqlite` is appended.
        let data_dir =
            std::path::PathBuf::from("/home/u/.local/share/buffr-debug/engines/webkit/profile");
        let got =
            compute_cookie_db_path(Some(&data_dir), "webkit", None).expect("path must resolve");
        assert_eq!(
            got,
            "/home/u/.local/share/buffr-debug/engines/webkit/profile/cookies.sqlite"
        );
        assert!(
            !got.contains("engines/webkit/profile/engines/webkit"),
            "regression: engine namespace appears twice in the path"
        );
    }

    #[test]
    fn cookie_path_fallback_branch_namespaces_under_xdg() {
        // When data_dir is None (rare, unsupported configuration) we
        // build the path from the XDG fallback.  This branch DOES
        // namespace itself because the XDG root is shared across
        // engines; landing every engine's cookies in a single file
        // would corrupt state.
        let xdg = std::path::PathBuf::from("/home/u/.local/share");
        let got = compute_cookie_db_path(None, "webkit", Some(&xdg))
            .expect("path must resolve from XDG fallback");
        assert_eq!(
            got,
            "/home/u/.local/share/buffr/engines/webkit/cookies.sqlite"
        );
    }

    // ── Dispatch mapping presence — regression guard ─────────────────────────

    #[test]
    fn focus_scripts_compile_into_dispatch_catalogue() {
        // Regression: pressing `i` (FocusFirstInput) logged
        // "webkit: dispatch: no mapping yet" because the engine's
        // dispatch match didn't carry an arm for FocusFirstInput nor
        // for ExitInsertMode.  The fix calls into the shared
        // buffr_core::scripts constants, which are include_str!()
        // assets — a missing or empty asset would silently land an
        // empty JS string in the worker queue.
        //
        // This test pins both invariants at compile + load time:
        //   1. The constants exist (referencing them compiles).
        //   2. Each constant has non-trivial content (covers an
        //      include_str!() that pointed at the wrong file).
        let focus = buffr_core::scripts::FOCUS_FIRST_INPUT;
        let exit = buffr_core::scripts::EXIT_INSERT;
        assert!(
            !focus.trim().is_empty(),
            "FOCUS_FIRST_INPUT must carry a real JS body"
        );
        assert!(
            !exit.trim().is_empty(),
            "EXIT_INSERT must carry a real JS body"
        );
    }

    // ── UCM JS bridges — object-payload regression guard ─────────────────────

    /// The three object-payload UCM channels (`buffrCursor`,
    /// `buffrAudio`, `buffrFavicon`) used to call `postMessage({…})`
    /// with a JS object literal; the C-side `jsc_value_to_string`
    /// then returned `"[object Object]"` instead of JSON, which
    /// serde_json rejected with "expected value at line 1 column 2"
    /// at every mousemove.  Fix wraps every payload in
    /// `JSON.stringify(...)` so the C side gets real JSON text.
    ///
    /// This test pins the contract: each bridge's source must contain
    /// `postMessage(JSON.stringify(`.  A future edit that removes
    /// the wrapper will fail the test before it reaches production.
    #[test]
    fn ucm_object_payloads_are_json_stringified() {
        use super::super::runtime::{AUDIO_BRIDGE_JS, CURSOR_BRIDGE_JS, FAVICON_BRIDGE_JS};
        for (name, src) in [
            ("buffrCursor", CURSOR_BRIDGE_JS),
            ("buffrAudio", AUDIO_BRIDGE_JS),
            ("buffrFavicon", FAVICON_BRIDGE_JS),
        ] {
            assert!(
                src.contains("postMessage(JSON.stringify("),
                "{name} bridge JS must wrap its object payload in JSON.stringify(…); \
                 raw objects serialise to \"[object Object]\" via jsc_value_to_string"
            );
        }
    }

    #[test]
    fn cookie_path_fallback_uses_cwd_when_xdg_missing() {
        // No data_dir AND no XDG home — last resort is the current
        // working directory.  Confirms no panic and no surprise
        // absolute path injection.
        let got = compute_cookie_db_path(None, "webkit", None)
            .expect("path must resolve from cwd fallback");
        assert_eq!(got, "./buffr/engines/webkit/cookies.sqlite");
    }

    #[test]
    fn neutral_key_to_wpe_preserves_modifiers() {
        // Modifier bitmask flows verbatim through the boundary so
        // shortcut keystrokes (Ctrl-T, Shift-Tab, …) reach WebKit
        // with the right chord state.
        let e = NeutralKeyEvent {
            modifiers: 0b1010, // arbitrary mask
            ..raw_down(13, 0)  // Enter, shortcut-style
        };
        let ev = neutral_key_to_wpe(e).expect("Enter RawDown must dispatch");
        assert_eq!(ev.modifiers, 0b1010);
    }

    // ── should_record_display_url (W6) ───────────────────────────────────────

    #[test]
    fn display_url_recorded_for_buffr_schemes() {
        assert!(should_record_display_url("buffr://new"));
        assert!(should_record_display_url("buffr://settings"));
        assert!(should_record_display_url("buffr-src:https://example.com"));
    }

    /// W6 regression: recording an override for a normal web URL pinned the
    /// omnibar to the entry URL forever — the entry was only dropped on tab
    /// close, so clicking through to `/foo` still reported the landing page.
    #[test]
    fn display_url_not_recorded_for_web_schemes() {
        assert!(!should_record_display_url("https://example.com"));
        assert!(!should_record_display_url("https://example.com/foo"));
        assert!(!should_record_display_url("http://127.0.0.1:9/tok/new"));
        assert!(!should_record_display_url("file:///tmp/x.html"));
        assert!(!should_record_display_url("data:text/html,<p>hi"));
        assert!(!should_record_display_url("about:blank"));
        assert!(!should_record_display_url(""));
    }

    /// Near-misses must not be mistaken for the real schemes.
    #[test]
    fn display_url_policy_is_prefix_exact() {
        assert!(!should_record_display_url("https://buffr://new"));
        assert!(!should_record_display_url("buffr:new"));
        assert!(!should_record_display_url("xbuffr://new"));
        assert!(!should_record_display_url("buffr-source:https://x"));
    }

    /// The end-to-end shape of the fix: with no override the summary tracks
    /// whatever WebKit loaded, which is what an https:// tab needs.
    #[test]
    fn web_url_summary_tracks_the_live_url_after_the_policy_change() {
        let typed = "https://example.com";
        assert!(!should_record_display_url(typed));
        let s = TabSummary {
            id: TabId(1),
            browser_id: 1,
            url: "https://example.com/foo".into(),
            title: "Foo".into(),
            progress: 1.0,
            is_loading: false,
            pinned: false,
            private: false,
        };
        // No override recorded → summary passes through untouched.
        let out = apply_display_overrides_pure(s.clone(), None);
        assert_eq!(out.url, "https://example.com/foo");
        assert_eq!(out.title, "Foo");
        // The old behaviour, for contrast: the typed URL wins forever.
        let stale = apply_display_overrides_pure(s, Some(typed));
        assert_eq!(stale.url, typed, "this is exactly W6");
    }
}
