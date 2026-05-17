//! GLib worker thread for the WPE WebKit backend.
//!
//! # Threading model
//!
//! ```text
//! WebKitEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  wpe_thread (mpsc::SyncSender)
//!                                ├─ glib::MainLoop::run()
//!                                ├─ timeout_add_local (10 ms poll) ──▶ drain commands
//!                                └─ WpeRuntime (WebView + FDO exportable)
//!   engine_state ──────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by GLib signal handlers on the worker thread)
//! ```
//!
//! WPE WebKit has no process-wide default MainContext ownership concept
//! (unlike GTK4), so the worker creates its own `glib::MainLoop::new(None, false)`
//! and immediately owns it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use buffr_core::cursor::{CursorState, SharedCursorState};
use buffr_core::edit::EditEventSink;
use buffr_core::favicon::{FaviconSink, new_favicon_sink};
use buffr_core::hint::{HintEventSink, new_hint_event_sink};
use buffr_downloads::Downloads;
use buffr_engine::permissions::{PermissionsQueue, new_queue as new_permissions_queue};
use buffr_engine::popup::{PopupQueue, new_popup_queue};
use buffr_engine::{PromptOutcome, SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use super::runtime::WpePermissionRequestPtr;

use super::error::WebKitError;
use super::ffi::{
    WebKitCookiePersistentStorage_WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE,
    webkit_cookie_manager_set_persistent_storage, webkit_network_session_get_cookie_manager,
    webkit_network_session_get_default,
};
use super::runtime::{
    TabInfo, WpeAudioEventQueue, WpeContextMenuSink, WpeRuntime, new_audio_event_queue,
    new_wpe_context_menu_sink,
};

// ── EngineState ───────────────────────────────────────────────────────────────

/// Thread-safe engine snapshot. Updated by the worker thread; read from any
/// thread behind `Mutex<EngineState>`.
pub(crate) struct EngineState {
    /// Open tabs in strip order.
    pub tabs: Vec<TabInfo>,
    /// Index of the active tab.
    pub active_idx: Option<usize>,
    /// Next tab id counter.
    pub next_id: u64,
    /// Current viewport width.
    pub width: u32,
    /// Current viewport height.
    pub height: u32,
    /// Set by signal handlers to signal a URL/title change.
    pub address_changed: bool,
    /// OSR sleep flag: true = skip repaints.
    pub osr_sleeping: Arc<AtomicBool>,
    /// Cached audio-active state (polled on the worker thread).
    pub audio_active: Arc<AtomicBool>,
}

impl EngineState {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            tabs: Vec::new(),
            active_idx: None,
            next_id: 1,
            width,
            height,
            address_changed: false,
            osr_sleeping: Arc::new(AtomicBool::new(false)),
            audio_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn active_tab_info(&self) -> Option<&TabInfo> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    pub(crate) fn tabs_summary(&self) -> Vec<TabSummary> {
        self.tabs.iter().map(|t| t.to_summary()).collect()
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

pub(crate) enum Command {
    OpenTab {
        url: String,
        reply: mpsc::SyncSender<Result<TabId, String>>,
        /// When true, the new tab is created in the background: no active-tab
        /// switch, no is_loading_atomic update, no needs_fresh reset.
        background: bool,
    },
    Navigate {
        url: String,
    },
    Resize {
        width: u32,
        height: u32,
    },
    OsrResize {
        width: u32,
        height: u32,
    },
    KeyEvent {
        ev: WpeKeyEvent,
    },
    MouseMove {
        x: i32,
        y: i32,
        modifiers: u32,
    },
    MouseClick {
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    MouseWheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    },
    Focus {
        #[allow(dead_code)]
        // Currently dispatched but unused; future tab-strip work consumes this.
        focused: bool,
    },
    OsrSleep {
        sleep: bool,
    },
    /// Close the active tab. Drops the WebView and clears engine_state.tabs.
    CloseActive {
        reply: mpsc::SyncSender<bool>,
    },
    /// Close the tab with the given id. If it's not the active tab, just
    /// removes it and adjusts active_idx. If it is active, falls back like
    /// close_active (picks the next tab in strip order).
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<bool>,
    },
    /// Switch the active tab to the one with this id. Flips the per-tab
    /// is_active flags so only the new active tab's paints / load-state
    /// updates reach the shared frame and the runtime is_loading atomic.
    SelectTab {
        id: TabId,
    },
    /// Run a JavaScript snippet in the active tab's main world. Used
    /// by `WebKitEngine::dispatch` to implement vim-style scrolling
    /// (`window.scrollBy(...)`) and other catch-all PageActions.
    EvalJs {
        script: String,
    },
    /// Adjust zoom level on the active tab.
    ///
    /// `delta = +0.1` for zoom_in, `-0.1` for zoom_out, `0.0` for reset.
    /// The new level is clamped to `0.25..=5.0` and written back to the
    /// shared `zoom_level` Arc.
    Zoom {
        delta: f64,
    },
    /// Reorder tabs: move the tab at `from` to the position `to` in the
    /// strip. Both indices are pre-move positions; `to` is adjusted for
    /// the removal shift when `to > from`.
    MoveTab {
        from: usize,
        to: usize,
    },
    /// Toggle the WebKit web inspector for the active tab and report
    /// success/failure via the reply channel.
    OpenDevtools {
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    /// Begin an in-page find session on the active tab.
    ///
    /// `query` is the search string; `forward = false` searches backwards.
    /// Options: CASE_INSENSITIVE | WRAP_AROUND (+ BACKWARDS when !forward).
    StartFind {
        query: String,
        forward: bool,
    },
    /// Cancel the active find session and remove highlights.
    StopFind,
    /// Execute a named WebKit editing command on the active tab.
    ///
    /// Standard names: `"Undo"`, `"Redo"`, `"Cut"`, `"Copy"`, `"Paste"`,
    /// `"PasteAsPlainText"`, `"SelectAll"`.
    ExecEditingCommand {
        command: &'static str,
    },
    /// Trigger a download of `url` on the active WebView.
    ///
    /// Calls `webkit_web_view_download_uri` on the active tab's WebView;
    /// the returned `*mut WebKitDownload` is discarded — the process-wide
    /// `download-started` signal handler (wired in `spawn`) picks it up and
    /// routes lifecycle events through the buffr-downloads pipeline.
    StartDownload {
        url: String,
    },
    /// Resolve a pending permission request.
    ///
    /// `resolve_id` matches a key in `WpeRuntime::pending_permissions`.
    /// `outcome` is mapped to `webkit_permission_request_allow/deny`.
    ResolvePermission {
        resolve_id: String,
        outcome: PromptOutcome,
    },
    /// Fire `MEDIA_PROBE_POLL_JS` on the active tab (#135). The result is
    /// delivered asynchronously via the `buffrMediaProbe` UCM handler, which
    /// stores the `video` flag in the runtime-wide `video_active` atomic.
    RunMediaProbe,
    /// Update the active tab's `BuffrViewWayland` subsurface position and
    /// render size (#153). Dispatched from `WebKitEngine::set_native_parent`
    /// via the worker command channel; executed on the GLib worker thread so
    /// `wl_subsurface_set_position` + `wpe_view_resized` happen in the right
    /// thread context.
    SetNativeRect {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    /// Push a preedit update to the active tab's `BuffrInputMethodContext`.
    ///
    /// `text` is the preedit string; `cursor` is an optional `(start, end)`
    /// byte range within `text` for the IME composition selection.  `None`
    /// collapses the selection to the end of the text.
    ImeSetComposition {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    /// Commit text to the focused editable in the active tab.
    ImeCommit {
        text: String,
    },
    /// Cancel any in-progress IME composition on the active tab.
    ImeCancel,
    Shutdown,
}

/// Neutral keyboard event carried over the command channel.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WpeKeyEvent {
    pub key_code: u32,
    /// X11 keycode equivalent. Carried for future input-fidelity work
    /// (shortcut routing in non-Latin layouts); not consumed yet.
    #[allow(dead_code)]
    pub hardware_key_code: u32,
    pub pressed: bool,
    pub modifiers: u32,
}

// ── WorkerHandle ──────────────────────────────────────────────────────────────

/// Handle to the GLib worker thread.
pub(crate) struct WorkerHandle {
    pub cmd_tx: mpsc::SyncSender<Command>,
    pub engine_state: Arc<Mutex<EngineState>>,
    /// One-slot hint event mailbox shared with the worker's WpeRuntime and
    /// every TabEntry's UCM script-message handler. `WebKitEngine` reads this
    /// via `pump_hint_events`.
    pub hint_sink: HintEventSink,
    /// Shared popup URL queue. Written by the `create` signal handler on each
    /// WebView when JS calls `window.open(url)` or a link has `target=_blank`.
    /// Drained by the apps layer via `BrowserEngine::popup_queue`.
    pub popup_queue: PopupQueue,
    /// Context-menu request sink. Written by the `context-menu` signal handler
    /// on each WebView; drained by the apps layer via `drain_context_menu_requests`.
    pub context_menu_sink: WpeContextMenuSink,
    /// Favicon decode sink. Written by the per-tab `buffrFavicon` UCM handler
    /// (background thread); drained by `WebKitEngine::drain_favicon_updates`.
    pub favicon_sink: FaviconSink,
    /// Permissions queue. Written by the `permission-request` signal handler on
    /// each WebView; drained by the apps layer via `BrowserEngine::permissions_queue`.
    pub permissions_queue: PermissionsQueue,
    /// Map resolve_id → g_object_ref'd WebKitPermissionRequest ptr.
    /// Written on the worker thread from the signal handler; consumed by
    /// `Command::ResolvePermission` on the worker thread.
    pub pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>>,
    /// Monotonic counter for minting resolve_ids.
    pub permission_next_id: Arc<AtomicU64>,
    /// Shared audio-event queue (#132). Written by per-tab `buffrAudio` UCM
    /// signal handlers; drained by `WebKitEngine::drain_audio_events`.
    pub audio_event_queue: WpeAudioEventQueue,
    /// Shared cursor state (#137). Written by per-tab `buffrCursor` UCM signal
    /// handlers; read by `WebKitEngine::take_cursor_change`.
    pub cursor_state: SharedCursorState,
    /// Runtime-wide video-active flag (#135). Written by per-tab `buffrMediaProbe`
    /// UCM signal handlers on the GLib worker thread; read by
    /// `WebKitEngine::any_video_active` on any thread.
    pub video_active: Arc<AtomicBool>,
    /// Edit-mode event sink (#134). Shared with `WebKitEngine::edit_sink`;
    /// per-tab `buffrEdit` UCM signal handlers push decoded `EditConsoleEvent`s
    /// into the inner `Option<EditEventSink>` when populated.
    pub edit_sink: Arc<Mutex<Option<EditEventSink>>>,
    /// `Option` so [`Drop`] can take + join the handle. Stays `Some`
    /// until either `shutdown_and_join` is called explicitly or the
    /// engine is dropped.
    thread: Option<thread::JoinHandle<()>>,
}

impl WorkerHandle {
    /// Send Command::Shutdown to the worker, then block until its GLib
    /// main loop exits and the thread joins. Idempotent: a second call
    /// returns immediately because the JoinHandle is already taken.
    ///
    /// Must be called before `std::process::exit` so WebKit's atexit
    /// destructors don't WTFCrash unwinding a half-initialised state.
    pub(crate) fn shutdown_and_join(&mut self) {
        let Some(handle) = self.thread.take() else {
            return;
        };
        // Best-effort: if the worker queue is full or already gone, the
        // worker has crashed; we still try to join below.
        let _ = self.cmd_tx.try_send(Command::Shutdown);
        if let Err(e) = handle.join() {
            tracing::warn!(?e, "webkit worker: thread panicked during shutdown");
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

/// Spawn the GLib worker thread and return a handle.
///
/// - `cookie_db_path` — when `Some`, the worker calls
///   `webkit_cookie_manager_set_persistent_storage` once at startup so
///   cookies survive browser restarts. `None` leaves WebKit's default
///   in-memory cookie store active.
/// - `downloads` — when `Some`, the worker wires the `download-started`
///   signal on the default `WebKitNetworkSession` and records lifecycle
///   events (`started` / `finished` / `failed`) into the shared store.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    initial_url: &str,
    width: u32,
    height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
    zoom_level: Arc<Mutex<f64>>,
    cookie_db_path: Option<String>,
    downloads: Option<std::sync::Arc<Downloads>>,
    can_go_back: Arc<AtomicBool>,
    can_go_forward: Arc<AtomicBool>,
    prefer_native: bool,
    wayland_handles: Option<buffr_engine::WaylandNativeHandles>,
) -> Result<WorkerHandle, WebKitError> {
    // No FDO bootstrap on the new wpe-platform path — the BuffrDisplay
    // subclass owns its own EGL display and view lifecycle. `wpe_loader_init`
    // / `wpe_fdo_initialize_*` are gone with the Phase 2 scaffold.

    let engine_state = Arc::new(Mutex::new(EngineState::new(width, height)));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Command>(64);

    // Shared hint event mailbox. Allocated here so both `WebKitEngine`
    // (reader) and the GLib worker thread (writer, via TabEntry's UCM
    // signal handler) share the same Arc.
    let hint_sink: HintEventSink = new_hint_event_sink();

    // Shared popup URL queue. Allocated here so both `WebKitEngine` (drainer)
    // and the GLib worker thread (writer, via TabEntry's `create` signal)
    // share the same Arc.
    let popup_queue: PopupQueue = new_popup_queue();

    // Shared context-menu request sink. Allocated here so both `WebKitEngine`
    // (drainer via `drain_context_menu_requests`) and the GLib worker thread
    // (writer, via TabEntry's `context-menu` signal) share the same Arc.
    let context_menu_sink: WpeContextMenuSink = new_wpe_context_menu_sink();

    // Shared favicon decode sink. Allocated here so both `WebKitEngine`
    // (drainer via `drain_favicon_updates`) and per-tab background fetch
    // threads (writers) share the same Arc.
    let favicon_sink: FaviconSink = new_favicon_sink();

    // Shared permissions queue + pending map + id counter. Allocated here so
    // both `WebKitEngine` (drainer via `permissions_queue`) and the GLib
    // worker thread (writer, via TabEntry's `permission-request` signal) share
    // the same Arcs. The map and counter are also used inside the worker's
    // `Command::ResolvePermission` handler.
    let permissions_queue: PermissionsQueue = new_permissions_queue();
    let pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let permission_next_id: Arc<AtomicU64> = Arc::new(AtomicU64::new(1));

    // Shared audio-event queue (#132). Allocated here so both `WebKitEngine`
    // (drainer via `drain_audio_events`) and per-tab UCM signal handlers
    // (writers via `AudioSignalCtx`) share the same Arc.
    let audio_event_queue: WpeAudioEventQueue = new_audio_event_queue();

    // Shared cursor state (#137). Allocated here so both `WebKitEngine`
    // (reader via `take_cursor_change`) and per-tab UCM signal handlers
    // (writers via `CursorSignalCtx`) share the same Arc.
    let cursor_state: SharedCursorState = Arc::new(CursorState::new());

    // Runtime-wide video-active flag (#135). Allocated here so both
    // `WebKitEngine` (reader via `any_video_active`) and per-tab UCM signal
    // handlers (writers via `MediaProbeSignalCtx`) share the same Arc.
    let video_active: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Edit-mode event sink (#134). Allocated here as an empty Option; the
    // apps layer calls `WebKitEngine::set_edit_sink` post-construction which
    // populates the inner Option. Per-tab `buffrEdit` UCM handlers hold a
    // clone of this Arc and push decoded events when the inner sink is Some.
    let edit_sink: Arc<Mutex<Option<EditEventSink>>> = Arc::new(Mutex::new(None));

    let initial_url = initial_url.to_owned();
    let es = Arc::clone(&engine_state);
    let hint_sink_worker = Arc::clone(&hint_sink);
    let popup_queue_worker = Arc::clone(&popup_queue);
    let context_menu_sink_worker = Arc::clone(&context_menu_sink);
    let favicon_sink_worker = Arc::clone(&favicon_sink);
    let can_go_back_worker = Arc::clone(&can_go_back);
    let can_go_forward_worker = Arc::clone(&can_go_forward);
    let permissions_queue_worker = Arc::clone(&permissions_queue);
    let pending_permissions_worker = Arc::clone(&pending_permissions);
    let permission_next_id_worker = Arc::clone(&permission_next_id);
    let audio_event_queue_worker = Arc::clone(&audio_event_queue);
    let cursor_state_worker = Arc::clone(&cursor_state);
    let video_active_worker = Arc::clone(&video_active);
    let edit_sink_worker = Arc::clone(&edit_sink);

    let thread = thread::Builder::new()
        .name("buffr-webkit-worker".into())
        .spawn(move || {
            tracing::info!("webkit worker: starting GLib main loop");

            // EGL display + GLES context, bound to this worker thread.
            // BuffrDisplay hands the raw EGLDisplay to WebKit via its
            // get_egl_display vmethod.
            let egl = match super::egl::EglWorker::new() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("webkit worker: EGL init failed: {e}");
                    return;
                }
            };
            if let Err(e) = egl.make_current() {
                tracing::error!("webkit worker: eglMakeCurrent failed: {e}");
                return;
            }
            tracing::info!("webkit worker: EGL ready");

            // Acquire the default GMainContext on this thread (the app's
            // main thread never pumps any GMainContext, so we own it).
            let main_context = glib::MainContext::default();
            let _ctx_guard = match main_context.acquire() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("webkit worker: cannot acquire default GMainContext: {e}");
                    return;
                }
            };

            // Build the GLib main loop bound to the default context.
            let main_loop = glib::MainLoop::new(None, false);

            use std::cell::RefCell;
            use std::rc::Rc;

            let runtime = match WpeRuntime::new(
                Arc::clone(&frame),
                Arc::clone(&view),
                Arc::clone(&es),
                egl,
                Arc::clone(&is_loading_atomic),
                Arc::clone(&zoom_level),
                hint_sink_worker,
                popup_queue_worker,
                context_menu_sink_worker,
                favicon_sink_worker,
                can_go_back_worker,
                can_go_forward_worker,
                permissions_queue_worker,
                pending_permissions_worker,
                permission_next_id_worker,
                audio_event_queue_worker,
                cursor_state_worker,
                video_active_worker,
                edit_sink_worker,
                prefer_native,
                wayland_handles,
            ) {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("webkit worker: WpeRuntime::new failed: {e}");
                    return;
                }
            };
            let runtime_rc = Rc::new(RefCell::new(runtime));

            // ── Persistent cookie store ───────────────────────────────────
            //
            // Wire sqlite cookie storage before the first WebView is created
            // so the persistent store is active for the initial navigation.
            // Must run on the GLib worker thread after WpeRuntime::new (which
            // sets up the display / EGL context WebKit needs) and before
            // open_tab fires from the idle handler below.
            //
            // Called at most once per engine instance; per-tab calls are NOT
            // needed — the default WebKitNetworkSession is shared by all
            // WebViews in the process.
            if let Some(path) = cookie_db_path {
                match std::ffi::CString::new(path.as_str()) {
                    Ok(path_c) => {
                        // SAFETY: webkit_network_session_get_default() returns
                        // a process-singleton that is always valid after WebKit
                        // is loaded. webkit_network_session_get_cookie_manager
                        // returns a borrowed pointer valid for the session's
                        // lifetime. webkit_cookie_manager_set_persistent_storage
                        // is idempotent and safe to call before any WebView
                        // exists.
                        unsafe {
                            let session = webkit_network_session_get_default();
                            if !session.is_null() {
                                let manager = webkit_network_session_get_cookie_manager(session);
                                if !manager.is_null() {
                                    webkit_cookie_manager_set_persistent_storage(
                                        manager,
                                        path_c.as_ptr(),
                                        WebKitCookiePersistentStorage_WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE,
                                    );
                                    tracing::info!(
                                        path,
                                        "webkit: persistent cookie store enabled"
                                    );
                                } else {
                                    tracing::warn!(
                                        "webkit: cookie manager is NULL — cookies remain in-memory"
                                    );
                                }
                            } else {
                                tracing::warn!(
                                    "webkit: default network session is NULL — cookies remain in-memory"
                                );
                            }
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            path,
                            "webkit: cookie DB path contains NUL byte — cookies remain in-memory"
                        );
                    }
                }
            }

            // ── Download tracking: NetworkSession `download-started` signal ──
            //
            // Wire the default NetworkSession's `download-started` signal to
            // record lifecycle events into the shared `Downloads` store.
            // Pattern mirrors the cookie wiring above: one connection at
            // worker-init time on the process-wide default session.
            //
            // Signal callbacks are module-level `unsafe extern "C" fn`s defined
            // below `spawn` so they can name the types they need.
            // user_data for the session signal is a `Box<Arc<Downloads>>` leaked
            // via `Box::into_raw` so the callback can clone the Arc safely.
            if let Some(ref dl_store) = downloads {
                // Connect `download-started` on the default NetworkSession.
                // SAFETY: webkit_network_session_get_default() is always valid
                // after WebKit is loaded; the process singleton lives for the
                // process lifetime.
                unsafe {
                    use super::ffi::webkit_network_session_get_default;
                    let session = webkit_network_session_get_default();
                    if !session.is_null() {
                        // Box the Arc so the callback can clone it via raw ptr.
                        let store_raw =
                            Box::into_raw(Box::new(Arc::clone(dl_store))) as *mut std::os::raw::c_void;
                        let sig = std::ffi::CString::new("download-started").unwrap();
                        super::ffi::g_signal_connect_data(
                            session as *mut _,
                            sig.as_ptr(),
                            Some(std::mem::transmute::<
                                unsafe extern "C" fn(
                                    *mut std::os::raw::c_void,
                                    *mut std::os::raw::c_void,
                                    *mut std::os::raw::c_void,
                                ),
                                unsafe extern "C" fn(),
                            >(on_download_started)),
                            store_raw,
                            Some(drop_boxed_arc_downloads),
                            0,
                        );
                        tracing::info!("webkit: download-started signal connected");
                    } else {
                        tracing::warn!(
                            "webkit: default network session NULL — download tracking unavailable"
                        );
                    }
                }
            }

            // ── Initial tab: open inside the main loop via idle callback ──
            //
            // WebKit WPE requires the GLib main loop to be running before
            // any WebView is created (it needs its internal Wayland display
            // compositor + D-Bus process manager to initialise). Using
            // idle_add_local ensures the main loop is pumping before
            // wpe_view_backend_exportable_fdo_create is called.
            {
                let rt = Rc::clone(&runtime_rc);
                glib::idle_add_local_once(move || {
                    if let Err(e) = rt.borrow_mut().open_tab(&initial_url, false) {
                        tracing::error!("webkit worker: initial open_tab failed: {e}");
                    }
                });
            }

            // ── Audio-activity poll (500 ms) ──────────────────────────────
            {
                let rt = Rc::clone(&runtime_rc);
                let es_audio = Arc::clone(&es);
                glib::timeout_add_local(Duration::from_millis(500), move || {
                    let any = rt.borrow().any_audio_active();
                    if let Ok(st) = es_audio.lock() {
                        st.audio_active.store(any, Ordering::Relaxed);
                    }
                    glib::ControlFlow::Continue
                });
            }

            // ── Command poll (10 ms) ─────────────────────────────────────
            {
                let ml = main_loop.clone();
                let rt = Rc::clone(&runtime_rc);
                glib::timeout_add_local(Duration::from_millis(10), move || {
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(cmd) => {
                                if handle_command(cmd, &mut rt.borrow_mut(), &ml) {
                                    return glib::ControlFlow::Break;
                                }
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                return glib::ControlFlow::Continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                tracing::info!("webkit worker: command channel closed");
                                ml.quit();
                                return glib::ControlFlow::Break;
                            }
                        }
                    }
                });
            }

            tracing::info!("webkit worker: entering GLib main loop");
            main_loop.run();
            tracing::info!("webkit worker: main loop exited");
        })
        .map_err(|e| WebKitError::InitFailed(format!("failed to spawn worker: {e}")))?;

    Ok(WorkerHandle {
        cmd_tx,
        engine_state,
        hint_sink,
        popup_queue,
        context_menu_sink,
        favicon_sink,
        permissions_queue,
        pending_permissions,
        permission_next_id,
        audio_event_queue,
        cursor_state,
        video_active,
        edit_sink,
        thread: Some(thread),
    })
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Returns `true` if the main loop should exit.
fn handle_command(cmd: Command, rt: &mut WpeRuntime, ml: &glib::MainLoop) -> bool {
    match cmd {
        Command::OpenTab {
            url,
            reply,
            background,
        } => {
            let res = rt.open_tab(&url, background).map_err(|e| e.to_string());
            let _ = reply.try_send(res);
        }
        Command::Navigate { url } => {
            rt.navigate(&url);
        }
        Command::Resize { width, height } | Command::OsrResize { width, height } => {
            rt.resize(width, height);
        }
        Command::KeyEvent { ev } => {
            rt.dispatch_keyboard(ev.key_code, ev.pressed, ev.modifiers);
        }
        Command::MouseMove { x, y, modifiers } => {
            rt.dispatch_pointer_motion(x, y, translate_modifiers(modifiers));
        }
        Command::MouseClick {
            x,
            y,
            button,
            pressed,
            modifiers,
        } => {
            rt.dispatch_pointer_button(x, y, button, pressed, translate_modifiers(modifiers));
        }
        Command::MouseWheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        } => {
            rt.dispatch_axis(x, y, delta_x, delta_y, translate_modifiers(modifiers));
        }
        Command::Focus { focused: _ } => {
            // Focus tracking moves to the BuffrView focus_in/focus_out API.
            // Stub: no-op until the platform path is wired.
        }
        Command::OsrSleep { sleep } => {
            if let Ok(st) = rt.engine_state.lock() {
                st.osr_sleeping.store(sleep, Ordering::Relaxed);
            }
        }
        Command::CloseActive { reply } => {
            let closed = rt.close_active();
            let _ = reply.try_send(closed);
        }
        Command::CloseTab { id, reply } => {
            let closed = rt.close_tab(id);
            let _ = reply.try_send(closed);
        }
        Command::SelectTab { id } => {
            rt.select_tab(id);
        }
        Command::EvalJs { script } => {
            rt.eval_js(&script);
        }
        Command::Zoom { delta } => {
            rt.set_zoom(delta);
        }
        Command::MoveTab { from, to } => {
            rt.move_tab(from, to);
        }
        Command::OpenDevtools { reply } => {
            let res = rt.open_devtools();
            let _ = reply.try_send(res);
        }
        Command::StartFind { query, forward } => {
            rt.start_find(&query, forward);
        }
        Command::StopFind => {
            rt.stop_find();
        }
        Command::ExecEditingCommand { command } => {
            rt.execute_editing_command(command);
        }
        Command::StartDownload { url } => {
            rt.start_download(&url);
        }
        Command::ResolvePermission {
            resolve_id,
            outcome,
        } => {
            rt.resolve_permission(&resolve_id, outcome);
        }
        Command::RunMediaProbe => {
            rt.run_media_probe();
        }
        Command::SetNativeRect { x, y, w, h } => {
            rt.set_native_rect(x, y, w, h);
        }
        Command::ImeSetComposition { text, cursor } => {
            rt.ime_set_composition(&text, cursor);
        }
        Command::ImeCommit { text } => {
            rt.ime_commit(&text);
        }
        Command::ImeCancel => {
            rt.ime_cancel();
        }
        Command::Shutdown => {
            ml.quit();
            return true;
        }
    }
    false
}

// ── Download signal helpers ───────────────────────────────────────────────────
//
// Module-level `unsafe extern "C"` functions for the `download-started` chain
// on WebKitNetworkSession. Defined here so they can name `DlCtx` + `Downloads`.
//
// Ownership model:
//   - The `download-started` handler's `user_data` is a
//     `Box<Arc<Downloads>>` produced by `Box::into_raw` in `spawn`.
//     `drop_boxed_arc_downloads` reconstitutes + drops it on signal disconnect.
//   - Each per-download signal (progress / finished / failed) has its own
//     `user_data = Box<DlCtx>` (independent clone of the Arc inside).
//     `drop_dl_ctx` handles cleanup.

/// Per-download context shared across progress / finished / failed handlers.
pub(crate) struct DlCtx {
    pub id: buffr_downloads::DownloadId,
    pub store: Arc<Downloads>,
    pub suggested: String,
}

/// GClosureNotify: drop the `Box<Arc<Downloads>>` leaked in `spawn` for the
/// `download-started` signal connection.
pub(crate) unsafe extern "C" fn drop_boxed_arc_downloads(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut super::ffi::_GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data is a Box<Arc<Downloads>> produced by Box::into_raw.
        unsafe { drop(Box::from_raw(user_data as *mut Arc<Downloads>)) };
    }
}

/// GClosureNotify: drop the `Box<DlCtx>` leaked for per-download signal connections.
pub(crate) unsafe extern "C" fn drop_dl_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut super::ffi::_GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data is a Box<DlCtx> produced by Box::into_raw.
        unsafe { drop(Box::from_raw(user_data as *mut DlCtx)) };
    }
}

/// `notify::estimated-progress` on `WebKitDownload`.
/// Signal prototype: `(GObject*, GParamSpec*, gpointer)`.
pub(crate) unsafe extern "C" fn on_download_progress(
    download: *mut std::os::raw::c_void,
    _pspec: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if download.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: user_data is a Box<DlCtx> valid until drop_dl_ctx fires.
    let ctx = unsafe { &*(user_data as *const DlCtx) };
    let progress = unsafe {
        use super::ffi::{WebKitDownload, webkit_download_get_estimated_progress};
        webkit_download_get_estimated_progress(download as *mut WebKitDownload)
    };
    // Store progress as a 0..1_000_000 received_bytes approximation.
    let received = (progress * 1_000_000.0) as u64;
    let _ = ctx.store.update_progress(ctx.id, received, None);
    tracing::debug!(progress, "webkit: download progress");
}

/// `finished` on `WebKitDownload`.
/// Signal prototype: `(WebKitDownload*, gpointer)`.
pub(crate) unsafe extern "C" fn on_download_finished(
    download: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if download.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: user_data is a Box<DlCtx> valid until drop_dl_ctx fires.
    let ctx = unsafe { &*(user_data as *const DlCtx) };
    let dest = unsafe {
        use super::ffi::{WebKitDownload, webkit_download_get_destination};
        let dest_ptr = webkit_download_get_destination(download as *mut WebKitDownload);
        if dest_ptr.is_null() {
            std::path::PathBuf::from(&ctx.suggested)
        } else {
            std::path::PathBuf::from(
                std::ffi::CStr::from_ptr(dest_ptr)
                    .to_string_lossy()
                    .as_ref(),
            )
        }
    };
    if let Err(e) = ctx.store.record_completed(ctx.id, &dest) {
        tracing::warn!(error = %e, "webkit: record_completed failed");
    }
    tracing::debug!(id = ctx.id.0, dest = %dest.display(), "webkit: download finished");
}

/// `failed` on `WebKitDownload`.
/// Signal prototype: `(WebKitDownload*, GError*, gpointer)`.
pub(crate) unsafe extern "C" fn on_download_failed(
    _download: *mut std::os::raw::c_void,
    _error: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: user_data is a Box<DlCtx> valid until drop_dl_ctx fires.
    let ctx = unsafe { &*(user_data as *const DlCtx) };
    if let Err(e) = ctx.store.record_failed(ctx.id, "download failed") {
        tracing::warn!(error = %e, "webkit: record_failed failed");
    }
    tracing::debug!(id = ctx.id.0, "webkit: download failed");
}

/// `download-started` on `WebKitNetworkSession`.
/// Signal prototype: `(WebKitNetworkSession*, WebKitDownload*, gpointer)`.
/// `user_data` is a `Box<Arc<Downloads>>` (see `drop_boxed_arc_downloads`).
pub(crate) unsafe extern "C" fn on_download_started(
    _session: *mut std::os::raw::c_void,
    download: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if download.is_null() || user_data.is_null() {
        return;
    }
    // Clone the Arc from the Box so we can hand it to DlCtx instances.
    // SAFETY: user_data is a Box<Arc<Downloads>> valid until drop_boxed_arc_downloads.
    let store: Arc<Downloads> = unsafe { Arc::clone(&*(user_data as *const Arc<Downloads>)) };

    use super::ffi::{
        WebKitDownload, webkit_download_get_request, webkit_download_get_response,
        webkit_download_set_destination, webkit_uri_request_get_uri,
        webkit_uri_response_get_suggested_filename,
    };
    use std::ffi::{CStr, CString};

    let dl = download as *mut WebKitDownload;

    // Derive URL from request.
    let url = unsafe {
        let req = webkit_download_get_request(dl);
        if req.is_null() {
            String::new()
        } else {
            let uri_ptr = webkit_uri_request_get_uri(req);
            if uri_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(uri_ptr).to_string_lossy().into_owned()
            }
        }
    };

    // Derive suggested filename from response (Content-Disposition) or URL.
    let suggested: String = unsafe {
        let resp = webkit_download_get_response(dl);
        let from_resp = if resp.is_null() {
            None
        } else {
            let sfn = webkit_uri_response_get_suggested_filename(resp);
            if sfn.is_null() {
                None
            } else {
                let s = CStr::from_ptr(sfn).to_string_lossy().into_owned();
                if s.is_empty() { None } else { Some(s) }
            }
        };
        from_resp.unwrap_or_else(|| url.split('/').next_back().unwrap_or("download").to_owned())
    };

    tracing::debug!(url, "webkit: download-started");

    // Insert a row in the Downloads store. Use cef_id=0; the store is
    // idempotent per (cef_id, url, suggested_name) but we're always adding
    // a fresh download here so any distinct cef_id would work.
    let dl_id = match store.record_started(0, &url, &suggested, None, None) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "webkit: record_started failed");
            return;
        }
    };

    // Set destination to $HOME/Downloads/<suggested>.
    unsafe {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
        let dir = std::path::Path::new(&home).join("Downloads");
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join(&suggested);
        if let Some(s) = dest.to_str() {
            if let Ok(c) = CString::new(s) {
                webkit_download_set_destination(dl, c.as_ptr());
            }
        }
    }

    // Wire `notify::estimated-progress`.
    {
        let ctx_ptr = Box::into_raw(Box::new(DlCtx {
            id: dl_id,
            store: Arc::clone(&store),
            suggested: suggested.clone(),
        })) as *mut std::os::raw::c_void;
        let sig = CString::new("notify::estimated-progress").unwrap();
        unsafe {
            super::ffi::g_signal_connect_data(
                download,
                sig.as_ptr(),
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(
                        *mut std::os::raw::c_void,
                        *mut std::os::raw::c_void,
                        *mut std::os::raw::c_void,
                    ),
                    unsafe extern "C" fn(),
                >(on_download_progress)),
                ctx_ptr,
                Some(drop_dl_ctx),
                0,
            );
        }
    }

    // Wire `finished`.
    {
        let ctx_ptr = Box::into_raw(Box::new(DlCtx {
            id: dl_id,
            store: Arc::clone(&store),
            suggested: suggested.clone(),
        })) as *mut std::os::raw::c_void;
        let sig = CString::new("finished").unwrap();
        unsafe {
            super::ffi::g_signal_connect_data(
                download,
                sig.as_ptr(),
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(*mut std::os::raw::c_void, *mut std::os::raw::c_void),
                    unsafe extern "C" fn(),
                >(on_download_finished)),
                ctx_ptr,
                Some(drop_dl_ctx),
                0,
            );
        }
    }

    // Wire `failed`.
    {
        let ctx_ptr = Box::into_raw(Box::new(DlCtx {
            id: dl_id,
            store,
            suggested,
        })) as *mut std::os::raw::c_void;
        let sig = CString::new("failed").unwrap();
        unsafe {
            super::ffi::g_signal_connect_data(
                download,
                sig.as_ptr(),
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(
                        *mut std::os::raw::c_void,
                        *mut std::os::raw::c_void,
                        *mut std::os::raw::c_void,
                    ),
                    unsafe extern "C" fn(),
                >(on_download_failed)),
                ctx_ptr,
                Some(drop_dl_ctx),
                0,
            );
        }
    }
}

/// Map CEF EVENTFLAG_* bitmask → WPE `wpe_input_modifier`.
///
/// CEF flags (from `cef_event_flags_t`):
///   SHIFT=1<<1=2, CONTROL=1<<2=4, ALT=1<<3=8, META=1<<5=32
/// WPE modifiers:
///   control=1, shift=2, alt=4, meta=8
fn translate_modifiers(cef: u32) -> u32 {
    let mut wpe = 0u32;
    // EVENTFLAG_SHIFT_DOWN = 0x02
    if cef & 0x02 != 0 {
        wpe |= 2; // wpe shift
    }
    // EVENTFLAG_CONTROL_DOWN = 0x04
    if cef & 0x04 != 0 {
        wpe |= 1; // wpe control
    }
    // EVENTFLAG_ALT_DOWN = 0x08
    if cef & 0x08 != 0 {
        wpe |= 4; // wpe alt
    }
    // EVENTFLAG_COMMAND_DOWN = 0x20
    if cef & 0x20 != 0 {
        wpe |= 8; // wpe meta
    }
    wpe
}
