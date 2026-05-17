//! Per-tab WPE WebKit state — lives on the worker thread.
//!
//! Phase 2 (FDO) was removed: the wpebackend-fdo exportable never delivers
//! frames on WPE WebKit 2.52 even with the EGL exportable path, because
//! WPE WebKit prefers the new wpe-platform backend whenever
//! `ENABLE_WPE_PLATFORM=ON` and the WebView's `display` property is set.
//!
//! The next iteration wires a custom [`WPEDisplay`] subclass (`BuffrDisplay`)
//! that hands WebKit our worker-thread EGL display + a [`WPEView`] subclass
//! whose `render_buffer` vmethod copies pixels into the shared [`OsrFrame`].
//! Until those subclasses land, `TabEntry::new` returns `None` and tabs
//! fail to open.

use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::sync::Mutex;

use buffr_core::hint::{HintConsoleEvent, HintEventSink, parse_console_event};
use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId};

use super::egl::EglWorker;
use super::ffi::*;
use super::worker::EngineState;
use super::wpe_subclass::{
    BuffrDisplayHandle, ViewCtx, ack_pending_buffer, attach_view_ctx, buffr_display_take_last_view,
};

// ── TabInfo (thread-safe snapshot) ───────────────────────────────────────────

/// Snapshot of one tab's state — updated from GLib signal handlers on the
/// worker thread; read from any thread by the engine via `Mutex<EngineState>`.
#[derive(Debug, Clone)]
pub(crate) struct TabInfo {
    pub id: TabId,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub progress: f64,
    pub is_pinned: bool,
}

impl TabInfo {
    pub(crate) fn new(id: TabId, url: &str) -> Self {
        Self {
            id,
            url: url.to_owned(),
            title: url.to_owned(),
            is_loading: false,
            progress: 0.0,
            is_pinned: false,
        }
    }

    pub(crate) fn to_summary(&self) -> buffr_engine::TabSummary {
        buffr_engine::TabSummary {
            id: self.id,
            browser_id: self.id.0 as i32,
            url: self.url.clone(),
            title: self.title.clone(),
            progress: self.progress as f32,
            is_loading: self.is_loading,
            pinned: self.is_pinned,
            private: false,
        }
    }
}

// ── GLib signal helpers (raw FFI without gtk4 dep) ────────────────────────────

// SAFETY: These symbols are provided by GLib (linked via wpe-webkit-2.0).
unsafe extern "C" {
    fn g_signal_handler_disconnect(instance: *mut std::os::raw::c_void, handler_id: u64);
    fn g_object_unref(object: *mut std::os::raw::c_void);
    fn g_free(ptr: *mut std::os::raw::c_void);
}

// SAFETY: jsc_value_to_string is exported by the JavaScriptCore library that
// wpe-webkit-2.0 depends on. It allocates a UTF-8 string that the caller
// must free with g_free. Symbol not in the bindgen allowlist because the
// JSCValue type was excluded; declared here manually for the script-message
// signal handler.
unsafe extern "C" {
    fn jsc_value_to_string(value: *mut std::os::raw::c_void) -> *mut std::os::raw::c_char;
}

// Console-bridge JS injected at document start. Overrides console.log to
// forward messages with the buffr hint sentinel to the native `buffrHint`
// script-message handler registered via WebKitUserContentManager.
//
// Pattern mirrors the webkitgtk backend (crates/buffr-webkitgtk).
const HINT_CONSOLE_BRIDGE_JS: &str = r#"
(function() {
  var orig = console.log;
  console.log = function() {
    orig.apply(console, arguments);
    var msg = arguments[0];
    if (typeof msg === 'string' && msg.indexOf('__buffr_hint__:') === 0) {
      try { window.webkit.messageHandlers.buffrHint.postMessage(msg); } catch(e) {}
    }
  };
})();
"#;

/// Per-tab heap context handed to WebKit's signal handlers via `user_data`.
/// Keeps an Arc clone of the shared engine state so handlers running on the
/// worker thread can update URL / title; loading state is tracked via a
/// dedicated lock-free flag (`is_loading_atomic`) so the load-finished
/// signal can never be lost to mutex contention with the main thread.
pub(crate) struct TabSignalCtx {
    tab_id: TabId,
    engine_state: Arc<Mutex<EngineState>>,
    is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
    /// Per-tab active flag, shared with this tab's `ViewCtx`. Inactive
    /// tabs must not write to the runtime-wide `is_loading_atomic` —
    /// otherwise a background tab finishing a navigation would clear
    /// the splash overlay while the foreground tab is still loading.
    /// TabInfo.is_loading still updates so the tabstrip's per-tab
    /// progress badge stays accurate for hidden tabs.
    is_active: Arc<std::sync::atomic::AtomicBool>,
}

impl TabSignalCtx {
    /// Mutate the per-tab info under `engine_state`. Uses `try_lock` so a
    /// signal handler running on the worker thread never blocks waiting
    /// for the main thread to release the lock — the main thread is
    /// continuously hitting `engine_state` (for `active_tab_live_url`,
    /// `tabs_summary`, etc.) and a contended `lock()` here can park the
    /// worker for long enough that the app feels frozen. A dropped
    /// signal update is recoverable: the next notify::* fires before
    /// the user notices.
    fn with_tab_info<F: FnOnce(&mut TabInfo)>(&self, f: F) {
        let Ok(mut st) = self.engine_state.try_lock() else {
            return;
        };
        if let Some(info) = st.tabs.iter_mut().find(|t| t.id == self.tab_id) {
            f(info);
            st.address_changed = true;
        }
    }
}

/// `load-changed (WebKitWebView*, WebKitLoadEvent)`. Toggles
/// `TabInfo::is_loading` on STARTED / COMMITTED / FINISHED so buffr-app's
/// `is_loading` accessor returns the right thing — the loading animation
/// uses this to decide whether to keep itself on top of the page.
unsafe extern "C" fn on_load_changed(
    web_view: *mut WebKitWebView,
    event: WebKitLoadEvent,
    user_data: *mut std::os::raw::c_void,
) {
    let _ = web_view;
    if user_data.is_null() {
        return;
    }
    // SAFETY: user_data is a `*const TabSignalCtx` we stash via
    // `Arc::into_raw` when connecting the signal; lives until the
    // matching Arc::from_raw fires in drop_tab_signal_ctx.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    let started = event == WebKitLoadEvent_WEBKIT_LOAD_STARTED
        || event == WebKitLoadEvent_WEBKIT_LOAD_REDIRECTED;
    // Flip false on COMMITTED, not just FINISHED. WEBKIT_LOAD_FINISHED
    // never fires on pages with long-poll XHRs (google.com keeps an
    // open connection for instant-search), so a FINISHED-only gate
    // pins the splash overlay forever. COMMITTED = main resource
    // headers received and first paint is imminent — that's when
    // the user should see the page.
    let revealed = event == WebKitLoadEvent_WEBKIT_LOAD_COMMITTED
        || event == WebKitLoadEvent_WEBKIT_LOAD_FINISHED;
    let finished = event == WebKitLoadEvent_WEBKIT_LOAD_FINISHED;
    use std::sync::atomic::Ordering;
    // Only the active tab drives the runtime-wide splash gate. Background
    // tabs still emit load-changed (LOAD_STARTED on background nav, etc.)
    // but their state lives in TabInfo, not on the shared atomic.
    if ctx.is_active.load(Ordering::Relaxed) {
        if started {
            ctx.is_loading_atomic.store(true, Ordering::SeqCst);
        } else if revealed {
            ctx.is_loading_atomic.store(false, Ordering::SeqCst);
        }
    }
    tracing::debug!(
        ?event,
        started,
        revealed,
        finished,
        "webkit: load-changed signal"
    );
    // Mirror into TabInfo on a best-effort basis for tab-summary readers
    // (progress bar, etc.). Skipping when the main thread holds the lock
    // is fine — the next paint reads is_loading_atomic which is the
    // load-bearing input to the animation gate.
    ctx.with_tab_info(|info| {
        if started {
            info.is_loading = true;
            info.progress = 0.0;
        } else if finished {
            info.is_loading = false;
            info.progress = 1.0;
        } else if revealed {
            // COMMITTED: page begin paint, progress ~50%. Keep
            // is_loading=true so progress bar stays visible until
            // FINISHED, but overlay drops via atomic.
            info.progress = 0.5;
        }
    });
}

/// `notify::uri` — fires whenever WebKit's view URL changes (redirects,
/// SPA pushState, etc.). Mirrors it into `TabInfo.url` so the omnibar
/// stays in sync.
unsafe extern "C" fn on_notify_uri(
    web_view: *mut WebKitWebView,
    _pspec: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: see on_load_changed.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    // SAFETY: web_view is the signal-emitting object; valid for the call.
    let uri_ptr = unsafe { webkit_web_view_get_uri(web_view) };
    if uri_ptr.is_null() {
        return;
    }
    // SAFETY: WebKit guarantees a null-terminated UTF-8 URI string.
    let uri = unsafe { std::ffi::CStr::from_ptr(uri_ptr) }
        .to_string_lossy()
        .into_owned();
    ctx.with_tab_info(|info| info.url = uri);
}

/// `notify::estimated-load-progress` — fires continuously as sub-resources
/// finish loading (0.0 → 1.0). Mirrors the live value into `TabInfo.progress`
/// so the tab-strip progress bar reflects real per-resource progress rather
/// than the three-step STARTED (0.0) / COMMITTED (0.5) / FINISHED (1.0)
/// approximation that `on_load_changed` sets.
///
/// `on_load_changed` still resets progress to 0.0 on STARTED and pins it
/// to 1.0 on FINISHED; those are the authoritative bookends. This handler
/// fills in the continuous values in between.
///
/// Audio probe signal (`notify::is-playing-audio`) is omitted here.
/// `TabSummary` has no per-tab `is_playing_audio` field yet; the existing
/// `any_audio_active()` polls `webkit_web_view_is_playing_audio` directly
/// on a 500 ms timer, which is sufficient. Add the signal once
/// `TabSummary` gets the field.
unsafe extern "C" fn on_notify_estimated_load_progress(
    web_view: *mut WebKitWebView,
    _pspec: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: see on_load_changed.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    // SAFETY: web_view is the signal-emitting object; valid for the call.
    let progress = unsafe { webkit_web_view_get_estimated_load_progress(web_view) };
    ctx.with_tab_info(|info| info.progress = progress);
}

/// `notify::title` — mirrors WebKit's page title into `TabInfo.title`
/// so tab pills show the document title once it's parsed (instead of
/// the URL placeholder we set on tab creation).
unsafe extern "C" fn on_notify_title(
    web_view: *mut WebKitWebView,
    _pspec: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: see on_load_changed.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    // SAFETY: web_view is the signal-emitting object; valid for the call.
    let title_ptr = unsafe { webkit_web_view_get_title(web_view) };
    if title_ptr.is_null() {
        return;
    }
    // SAFETY: WebKit guarantees a null-terminated UTF-8 title string.
    let title = unsafe { std::ffi::CStr::from_ptr(title_ptr) }
        .to_string_lossy()
        .into_owned();
    if title.is_empty() {
        return;
    }
    ctx.with_tab_info(|info| info.title = title);
}

/// GLib `GClosureNotify` for the per-connection `Arc<TabSignalCtx>`
/// pointer we leak in TabEntry::new. Each signal connection holds one
/// Arc clone; this fires once per disconnect, decrementing the Arc.
/// The last decrement frees the `TabSignalCtx`.
unsafe extern "C" fn drop_tab_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Arc::into_raw in TabEntry::new.
        drop(unsafe { Arc::from_raw(user_data as *const TabSignalCtx) });
    }
}

// ── Hint script-message signal ────────────────────────────────────────────────

/// `script-message-received::buffrHint` handler on the WebKitUserContentManager.
///
/// Prototype (GLib signal): `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries the string the page posted via
/// `window.webkit.messageHandlers.buffrHint.postMessage(msg)`.
/// We call `jsc_value_to_string` (malloc'd, must g_free), parse the sentinel
/// prefix with `buffr_core::hint::parse_console_event`, and write the result
/// into the shared `HintEventSink`.
unsafe extern "C" fn on_hint_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: user_data is an Arc<Mutex<Option<HintConsoleEvent>>> leaked via
    // Arc::into_raw; we borrow it here without consuming (no from_raw).
    let sink = unsafe { &*(user_data as *const Mutex<Option<HintConsoleEvent>>) };

    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let raw_str = unsafe { CStr::from_ptr(raw_ptr) }.to_string_lossy();
    let raw: &str = &raw_str;
    tracing::debug!(raw, "webkit: buffrHint script-message received");

    match parse_console_event(raw) {
        Some(Ok(event)) => {
            if let Ok(mut guard) = sink.lock() {
                *guard = Some(event);
            }
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, raw, "webkit: malformed hint event");
        }
        None => {
            tracing::debug!("webkit: buffrHint message missing sentinel (ignored)");
        }
    }
    // SAFETY: g_free the malloc'd string from jsc_value_to_string.
    unsafe { g_free(raw_ptr as *mut _) };
}

/// GLib `GClosureNotify` for the `Arc<HintEventSink>` pointer leaked in
/// `TabEntry::new` for the `script-message-received::buffrHint` connection.
unsafe extern "C" fn drop_hint_sink_arc(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Arc::into_raw.
        drop(unsafe { Arc::from_raw(user_data as *const Mutex<Option<HintConsoleEvent>>) });
    }
}

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab. Owns the WebKitWebView GObject. The shared
/// BuffrDisplay lives on `WpeRuntime`; tabs hold a borrowed raw pointer
/// only — the WebView itself retains its own ref on the display, so it
/// stays alive for the WebView's lifetime even if WpeRuntime races a drop.
pub(crate) struct TabEntry {
    pub id: TabId,
    pub web_view: *mut WebKitWebView,
    /// Borrowed WPEView pointer that BuffrDisplay handed to WebKit. Owned
    /// by the WebView; we only keep it for input dispatch / lookup.
    pub wpe_view: *mut WPEView,
    load_changed_id: u64,
    notify_title_id: u64,
    notify_uri_id: u64,
    notify_estimated_load_progress_id: u64,
    hint_script_message_received_id: u64,
    /// Shared per-tab flag wired into both `ViewCtx` (pixel write gate)
    /// and `TabSignalCtx` (is_loading_atomic write gate). Owned by
    /// `WpeRuntime` so it can flip the active tab via `select_tab`.
    pub is_active: Arc<std::sync::atomic::AtomicBool>,
}

// SAFETY: TabEntry owns C pointers that are used only on the worker thread.
unsafe impl Send for TabEntry {}

impl TabEntry {
    /// Build a WebView through the wpe-platform path: create a BuffrDisplay
    /// (hands WebKit our EGL display + a fake screen), then construct the
    /// WebView with the `display` property. WebKit calls our display's
    /// `create_view` vmethod, which we then attach the per-frame ViewCtx
    /// to so the render callback can find the shared OsrFrame.
    /// Construct a WebView bound to the shared BuffrDisplay owned by
    /// [`WpeRuntime`]. The display ref count is bumped by WebKit (one ref
    /// per WebView via the `display` construct property), and dropped when
    /// the WebView is unreffed — independent of WpeRuntime's own ref.
    pub(crate) fn new(
        id: TabId,
        url: &str,
        display: *mut WPEDisplay,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
        is_active: Arc<std::sync::atomic::AtomicBool>,
        hint_sink: HintEventSink,
    ) -> Option<Self> {
        if display.is_null() {
            tracing::error!("webkit: TabEntry::new called with NULL display");
            return None;
        }

        // WebKitWebView via the platform path. WebKit calls our display's
        // create_view vmethod during construction; we recover the view
        // pointer via the stash below.
        let web_view = unsafe {
            let key = CString::new("display").unwrap();
            let view_obj = g_object_new(
                webkit_web_view_get_type(),
                key.as_ptr(),
                display,
                std::ptr::null::<u8>(),
            );
            if view_obj.is_null() {
                tracing::error!("webkit: g_object_new(WebKitWebView, display=…) returned NULL");
                return None;
            }
            view_obj as *mut WebKitWebView
        };

        let wpe_view = unsafe { buffr_display_take_last_view() };
        if wpe_view.is_null() {
            tracing::error!("webkit: BuffrDisplay never called create_view");
            unsafe { g_object_unref(web_view as *mut _) };
            return None;
        }
        attach_view_ctx(
            wpe_view,
            ViewCtx {
                frame,
                view,
                last_ingest_us: std::sync::atomic::AtomicU64::new(0),
                is_active: Arc::clone(&is_active),
                pending_buffer: Mutex::new(None),
            },
        );
        tracing::info!("webkit: ViewCtx attached to WPEView");

        // Connect load-changed / notify::uri / notify::title so the engine
        // state's TabInfo stays in sync. Without this:
        //   - is_loading is set to true on navigate but never cleared, so
        //     the loading animation overlay never deactivates (page hides
        //     behind it for the entirety of the tab's lifetime).
        //   - the omnibar shows the URL we asked WebKit to load, never
        //     the URL it ended up on after redirects / pushState.
        //   - tab pills keep the URL placeholder until they're reopened.
        //
        // Shared per-tab handler context. Each signal connection holds
        // one Arc clone (leaked into a raw pointer) and the matching
        // GClosureNotify drops it on disconnect. Using Arc instead of a
        // single Box prevents a double-free: 3 signals × 1 destroy_notify
        // each = 3 drops, which on a Box would tear up the same allocation
        // three times.
        let ctx = Arc::new(TabSignalCtx {
            tab_id: id,
            engine_state,
            is_loading_atomic,
            is_active: Arc::clone(&is_active),
        });
        let connect = |signal: &str, cb: unsafe extern "C" fn()| -> u64 {
            let signal_c = CString::new(signal).unwrap();
            // Hand WebKit a per-connection Arc clone via Arc::into_raw;
            // GLib calls drop_tab_signal_ctx on disconnect which
            // reconstitutes + drops it.
            let arc_clone = Arc::clone(&ctx);
            let user_data = Arc::into_raw(arc_clone) as *mut std::os::raw::c_void;
            // SAFETY: web_view is a live WebKitWebView; signal_c lives
            // until the end of this block; cb is a 'static C fn pointer;
            // user_data is leaked + freed via drop_tab_signal_ctx.
            unsafe {
                g_signal_connect_data(
                    web_view as *mut _,
                    signal_c.as_ptr(),
                    Some(cb),
                    user_data,
                    Some(drop_tab_signal_ctx),
                    0,
                )
            }
        };
        let load_changed_id = connect("load-changed", unsafe {
            // SAFETY: transmute matches the fn pointer ABI shape required
            // by g_signal_connect_data (a callable C fn). The actual
            // arity-correct signature is enforced when WebKit invokes it.
            std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    WebKitLoadEvent,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_load_changed)
        });
        let notify_uri_id = connect("notify::uri", unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    *mut std::os::raw::c_void,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_notify_uri)
        });
        let notify_title_id = connect("notify::title", unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    *mut std::os::raw::c_void,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_notify_title)
        });
        let notify_estimated_load_progress_id =
            connect("notify::estimated-load-progress", unsafe {
                std::mem::transmute::<
                    unsafe extern "C" fn(
                        *mut WebKitWebView,
                        *mut std::os::raw::c_void,
                        *mut std::os::raw::c_void,
                    ),
                    unsafe extern "C" fn(),
                >(on_notify_estimated_load_progress)
            });

        // Enable developer extras on every WebView so the inspector can be
        // toggled at any time. Must be set before the inspector is shown;
        // calling it here (before load_uri) ensures that precondition is met.
        unsafe {
            let settings = webkit_web_view_get_settings(web_view);
            if !settings.is_null() {
                webkit_settings_set_enable_developer_extras(settings, 1);
            }
        }

        // ── Hint mode: UCM script-message handler ─────────────────────────────
        //
        // 1. Grab the WebView's default UserContentManager.
        // 2. Register the `buffrHint` native handler name. JS running in the page
        //    can then call `window.webkit.messageHandlers.buffrHint.postMessage(msg)`.
        // 3. Inject HINT_CONSOLE_BRIDGE_JS at document-start so that any
        //    `console.log('__buffr_hint__:…')` from hint.js is forwarded to the
        //    native handler without needing to modify hint.js.
        // 4. Wire the `script-message-received::buffrHint` GLib signal so the
        //    Rust side parses the event and writes it into `hint_sink`.
        //
        // IPC mechanism: webkit_user_content_manager_register_script_message_handler
        // + `script-message-received::buffrHint` signal — confirmed present in
        // WPE 2.52 bindings (see build/buffr-webkit-*/out/wpe_bindings.rs).
        let hint_script_message_received_id: u64 = unsafe {
            use super::ffi::{
                WebKitUserContentInjectedFrames_WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES as INJECT_ALL,
                WebKitUserScriptInjectionTime_WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START as INJECT_START,
                webkit_user_content_manager_add_script,
                webkit_user_content_manager_register_script_message_handler,
                webkit_user_script_new, webkit_user_script_unref,
                webkit_web_view_get_user_content_manager,
            };

            let ucm = webkit_web_view_get_user_content_manager(web_view);
            if ucm.is_null() {
                tracing::warn!(
                    "webkit: TabEntry::new — UserContentManager is NULL, hint IPC unavailable"
                );
                0
            } else {
                // Register the native message handler name.
                let handler_name = CString::new("buffrHint").unwrap();
                let _ok = webkit_user_content_manager_register_script_message_handler(
                    ucm,
                    handler_name.as_ptr(),
                    std::ptr::null(),
                );

                // Inject console bridge at document-start on all frames.
                let source_c = CString::new(HINT_CONSOLE_BRIDGE_JS).unwrap();
                let script = webkit_user_script_new(
                    source_c.as_ptr(),
                    INJECT_ALL,
                    INJECT_START,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if !script.is_null() {
                    webkit_user_content_manager_add_script(ucm, script);
                    webkit_user_script_unref(script);
                }

                // Connect `script-message-received::buffrHint` on the UCM.
                // The signal detail ("buffrHint") ensures we only receive
                // messages from that named handler, not all script messages.
                // Signal prototype:
                //   void script_message_received(WebKitUserContentManager*,
                //                                JSCValue* js_value, gpointer)
                let sink_arc = Arc::into_raw(Arc::clone(&hint_sink)) as *mut std::os::raw::c_void;
                let signal_name = CString::new("script-message-received::buffrHint").unwrap();
                g_signal_connect_data(
                    ucm as *mut _,
                    signal_name.as_ptr(),
                    Some(std::mem::transmute::<
                        unsafe extern "C" fn(
                            *mut std::os::raw::c_void,
                            *mut std::os::raw::c_void,
                            *mut std::os::raw::c_void,
                        ),
                        unsafe extern "C" fn(),
                    >(on_hint_script_message)),
                    sink_arc,
                    Some(drop_hint_sink_arc),
                    0,
                )
            }
        };

        let url_c = CString::new(url).unwrap_or_default();
        unsafe { webkit_web_view_load_uri(web_view, url_c.as_ptr()) };
        tracing::info!("webkit: created WebView id={id:?} url={url}");

        Some(TabEntry {
            id,
            web_view,
            wpe_view,
            load_changed_id,
            notify_title_id,
            notify_uri_id,
            notify_estimated_load_progress_id,
            hint_script_message_received_id,
            is_active,
        })
    }

    /// Navigate to a new URL.
    pub(crate) fn load_uri(&self, url: &str) {
        let c = CString::new(url).unwrap_or_default();
        // SAFETY: web_view is valid for the tab's lifetime; c is null-terminated.
        unsafe { webkit_web_view_load_uri(self.web_view, c.as_ptr()) };
    }

    /// Resize the toplevel that owns this tab's view. WebKit propagates the
    /// new size through to the WebProcess via our resize_vfunc → resized →
    /// view_resized chain.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        if self.wpe_view.is_null() {
            return;
        }
        unsafe {
            let tl = wpe_view_get_toplevel(self.wpe_view);
            if !tl.is_null() {
                wpe_toplevel_resize(tl, width as i32, height as i32);
            }
        }
    }

    /// Current URL from the WebView (may lag by one GLib tick).
    pub(crate) fn current_url(&self) -> String {
        // SAFETY: web_view is valid for the tab's lifetime.
        let ptr = unsafe { webkit_web_view_get_uri(self.web_view) };
        if ptr.is_null() {
            String::new()
        } else {
            // SAFETY: ptr is a valid null-terminated UTF-8 string.
            unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Whether the tab is currently playing audio.
    pub(crate) fn is_playing_audio(&self) -> bool {
        // SAFETY: web_view is valid for the tab's lifetime.
        unsafe { webkit_web_view_is_playing_audio(self.web_view) != 0 }
    }

    /// Mark this tab inactive at the WebKit level.
    ///
    /// We deliberately do NOT call `wpe_view_set_visible(false)` — WPE's
    /// AcceleratedBackingStore treats that as one-way (the matching
    /// `set_visible(true)` does not reliably re-arm render emission).
    /// Instead, the actual rendering pause is driven by the `is_active`
    /// gate in `buffr_rust_render_buffer`: once a tab is flagged inactive,
    /// the next buffer WebKit emits is parked in `ViewCtx::pending_buffer`
    /// WITHOUT acking. WPE's pool semantics make WebKit block on the ack
    /// before emitting more, so the view goes truly idle (no layout / no
    /// paint scheduling, no CPU cost). This method exists for symmetry +
    /// future hook surface; the flag flip is done by the caller.
    pub(crate) fn hide(&self) {
        // No FFI work needed — pause is implicit via the is_active gate +
        // parked-buffer mechanism. The caller has already flipped
        // `self.is_active` to false before calling hide().
    }

    /// Activate this tab at the WebKit level.
    ///
    /// Three-step kick:
    /// 1. Ack any buffer that `buffr_rust_render_buffer` parked while the
    ///    tab was inactive. Unblocks WebKit's emission loop — WPE recycles
    ///    the pool slot and schedules a new paint.
    /// 2. `wpe_view_set_visible(true)` — idempotent for tabs we kept
    ///    visible at the WebKit level, but cheap.
    /// 3. Resize-wiggle: `wpe_view_resized(w, h-1)` then `wpe_view_resized(w, h)`.
    ///    A same-dim resize is a no-op in WPE 2.52, so for a static page
    ///    whose last paint was acked long ago (no parked buffer to ack and
    ///    no scheduled work), the activate path would otherwise produce
    ///    zero new frames and the shared OsrFrame stays frozen on the
    ///    previous tab's pixels. The 1-pixel intermediate dim forces
    ///    WebKit's AcceleratedBackingStore to reflow + repaint, yielding
    ///    a fresh render_buffer on the way back to the original dim.
    pub(crate) fn show(&self, width: u32, height: u32) {
        if self.wpe_view.is_null() {
            return;
        }
        // (1) Release any parked buffer.
        let acked = ack_pending_buffer(self.wpe_view);
        // (2) + (3) — visible + resize-wiggle to force a fresh paint
        // for static pages that have nothing else to render.
        let wiggle_h = (height.saturating_sub(1)).max(1);
        // SAFETY: wpe_view is a live WPEView owned by the WebView; calls
        // are thread-bound to the GLib worker thread.
        unsafe {
            wpe_view_set_visible(self.wpe_view, 1);
            wpe_view_resized(self.wpe_view, width as i32, wiggle_h as i32);
            wpe_view_resized(self.wpe_view, width as i32, height as i32);
        }
        tracing::debug!(
            width,
            height,
            acked_pending = acked,
            "webkit: show — ack + resize-wiggle kick"
        );
    }
}

impl Drop for TabEntry {
    fn drop(&mut self) {
        // Release any buffer parked by the inactive-tab gate so WPE can
        // return the pool slot. The view is still alive at this point
        // (Drop runs before g_object_unref below); after unref the
        // attached ViewCtx is finalised by drop_view_ctx, which would
        // otherwise leak the held buffer ref.
        if !self.wpe_view.is_null() {
            ack_pending_buffer(self.wpe_view);
        }
        // SAFETY: GLib owns the signal lifetime; disconnect before unref to
        // prevent callbacks firing on a freed object.
        unsafe {
            if self.load_changed_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.load_changed_id);
            }
            if self.notify_title_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.notify_title_id);
            }
            if self.notify_uri_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.notify_uri_id);
            }
            if self.notify_estimated_load_progress_id != 0 {
                g_signal_handler_disconnect(
                    self.web_view as *mut _,
                    self.notify_estimated_load_progress_id,
                );
            }
            // Disconnect the hint script-message signal from the UCM. The UCM
            // is owned by the WebView and will be freed on g_object_unref below;
            // disconnecting first prevents the destroy notify firing on freed mem.
            if self.hint_script_message_received_id != 0 {
                let ucm = super::ffi::webkit_web_view_get_user_content_manager(self.web_view);
                if !ucm.is_null() {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.hint_script_message_received_id,
                    );
                }
            }
            g_object_unref(self.web_view as *mut _);
        }
    }
}

// ── WpeRuntime ────────────────────────────────────────────────────────────────

/// Top-level runtime that lives on the GLib worker thread. Owns the active
/// tab(s), the worker-thread EGL display, and the single shared
/// BuffrDisplay that every WebView is constructed against.
pub(crate) struct WpeRuntime {
    /// All open tabs in strip order. Mirrors `EngineState.tabs` and uses
    /// the same `TabId` for cross-referencing. At most one is "active"
    /// (its `is_active` flag is true) — that's the tab whose paints
    /// reach the shared `OsrFrame` and whose load-state drives the
    /// runtime-wide splash gate.
    pub tabs: Vec<TabEntry>,
    /// Index into `tabs` of the active entry. `None` only between
    /// `close_active` and the next `open_tab`.
    pub active_idx: Option<usize>,
    pub engine_state: Arc<Mutex<EngineState>>,
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub egl: EglWorker,
    /// Shared display. Lives for the runtime's lifetime; each WebView
    /// bumps its ref via the `display` construct property.
    display: BuffrDisplayHandle,
    /// Cross-thread flag for the active tab's load state. The
    /// load-changed signal handler stores into this; `WebKitEngine`'s
    /// `BrowserEngine::is_loading` impl reads it. Kept outside the
    /// engine_state mutex so worker-thread signal handlers never race
    /// (or worse, drop the update) against main-thread reads.
    pub is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
    /// Shared zoom level. Written by the worker on every zoom command;
    /// read from any thread via `WebKitEngine::active_zoom_level`.
    pub zoom_level: Arc<Mutex<f64>>,
    /// One-slot hint event mailbox. Written by the `buffrHint`
    /// script-message handler on the GLib worker thread when the injected
    /// `hint.js` fires a `console.log('__buffr_hint__:…')` message.
    /// Read (and cleared) by `WebKitEngine::pump_hint_events` on any thread.
    pub hint_sink: HintEventSink,
}

impl WpeRuntime {
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        egl: EglWorker,
        is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
        zoom_level: Arc<Mutex<f64>>,
        hint_sink: HintEventSink,
    ) -> Result<Self, String> {
        let (width, height, hz) = {
            let st = engine_state
                .lock()
                .map_err(|e| format!("mutex poison: {e}"))?;
            (
                st.width,
                st.height,
                view.frame_rate_hz
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(1),
            )
        };

        let display = BuffrDisplayHandle::new(egl.raw_display(), width, height, 1.0, hz)
            .ok_or_else(|| "BuffrDisplayHandle::new returned None".to_string())?;

        // Connect + publish as primary once. wpe_display_get_primary calls
        // inside WebKit now resolve here for the whole runtime lifetime.
        unsafe {
            let mut error: *mut GError = std::ptr::null_mut();
            let ok = wpe_display_connect(display.raw, &mut error);
            if ok == 0 {
                return Err("wpe_display_connect failed".into());
            }
            wpe_display_set_primary(display.raw);
        }
        tracing::info!("webkit: BuffrDisplay created + connected (shared)");

        Ok(Self {
            tabs: Vec::new(),
            active_idx: None,
            engine_state,
            frame,
            view,
            egl,
            display,
            is_loading_atomic,
            zoom_level,
            hint_sink,
        })
    }

    /// The currently active tab, if any.
    fn active_tab(&self) -> Option<&TabEntry> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    /// Open a new tab navigated to `url`.
    ///
    /// When `background` is true the current active tab is NOT deactivated,
    /// `is_loading_atomic` and `frame.needs_fresh` are NOT touched, and
    /// `active_idx` stays pointing at the existing tab. The new tab is
    /// appended to both `self.tabs` and `engine_state.tabs` so it appears in
    /// the tab strip, but its WPEView is created with `is_active = false` and
    /// immediately hidden via `hide()`.
    ///
    /// **Ordering contract (CRITICAL #1 rollback):**
    /// The `TabInfo` is pushed to `engine_state.tabs` *before* `TabEntry::new`
    /// so that the `load-changed` signal (which fires synchronously from
    /// `webkit_web_view_load_uri` inside `TabEntry::new`) can find the entry
    /// via `with_tab_info`. If `TabEntry::new` fails, the pushed `TabInfo` and
    /// any `active_idx` update are rolled back before returning an error.
    pub(crate) fn open_tab(&mut self, url: &str, background: bool) -> Result<TabId, String> {
        let id = {
            let mut st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex poison: {e}"))?;
            let id = TabId(st.next_id);
            st.next_id += 1;
            id
        };

        // For foreground tabs: deactivate the current active tab BEFORE
        // creating the new WebView. WebKit emits LOAD_STARTED + initial paint
        // events synchronously from webkit_web_view_load_uri inside
        // TabEntry::new — if the previous tab is still flagged active its
        // ViewCtx would clobber the shared frame and its TabSignalCtx would
        // clobber is_loading_atomic during the switch.
        let prev_active_idx = self.active_idx;
        if !background {
            if let Some(prev) = self.active_tab() {
                prev.is_active
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                prev.hide();
            }
        }

        // TabInfo must exist before signal handlers fire — load-changed
        // can race the return from TabEntry::new because webkit_web_view_load_uri
        // emits LOAD_STARTED synchronously. Push it now; roll back on failure.
        let info = TabInfo::new(id, url);
        let pushed_es_idx = {
            let mut st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex poison (push): {e}"))?;
            st.tabs.push(info);
            let pushed = st.tabs.len() - 1;
            if !background {
                st.active_idx = Some(pushed);
            }
            pushed
        };

        let is_active_flag = !background;
        let is_active = Arc::new(std::sync::atomic::AtomicBool::new(is_active_flag));
        let entry = TabEntry::new(
            id,
            url,
            self.display.raw,
            Arc::clone(&self.frame),
            Arc::clone(&self.view),
            Arc::clone(&self.engine_state),
            Arc::clone(&self.is_loading_atomic),
            Arc::clone(&is_active),
            Arc::clone(&self.hint_sink),
        );

        let entry = match entry {
            Some(e) => e,
            None => {
                // ── Rollback ──────────────────────────────────────────────────
                // Remove the TabInfo we pre-pushed and restore active_idx /
                // previous-tab is_active so the state is exactly as it was
                // before this call.
                if let Ok(mut st) = self.engine_state.lock() {
                    if pushed_es_idx < st.tabs.len() {
                        st.tabs.remove(pushed_es_idx);
                    }
                    if !background {
                        st.active_idx = prev_active_idx;
                    }
                }
                // Re-activate the previous tab if we deactivated it.
                if !background {
                    if let Some(idx) = prev_active_idx {
                        if let Some(prev) = self.tabs.get(idx) {
                            prev.is_active
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            // Read dims inside a fresh lock to avoid borrowing
                            // self while self.tabs is also borrowed.
                            let (w, h) = self
                                .engine_state
                                .lock()
                                .map(|st| (st.width, st.height))
                                .unwrap_or((800, 600));
                            prev.show(w, h);
                        }
                    }
                }
                return Err("TabEntry::new returned None".to_string());
            }
        };

        // Only after successful TabEntry creation: update atomics + frame for
        // foreground tabs.
        if !background {
            // Reset the loading flag so the signal handler doesn't have to win
            // the race against the main thread observing is_loading=false from
            // the previous tab.
            self.is_loading_atomic
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut frame) = self.frame.lock() {
                frame.needs_fresh = true;
            }
        } else {
            // Background tab: hide its view immediately so it doesn't produce
            // pixels into the shared frame.
            entry.hide();
        }

        self.tabs.push(entry);
        if !background {
            self.active_idx = Some(self.tabs.len() - 1);
        }
        Ok(id)
    }

    /// Switch the active tab to the one with `id`. No-op if `id` isn't
    /// open or is already active. On switch, the previous active tab's
    /// `is_active` flag flips to false (its paints stop reaching the
    /// shared frame); the new active tab's flag flips to true and
    /// `frame.needs_fresh=true` makes the renderer wait for its first
    /// paint instead of compositing the previous tab's last frame.
    pub(crate) fn select_tab(&mut self, id: TabId) -> bool {
        let Some(new_idx) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        if self.active_idx == Some(new_idx) {
            return false;
        }
        if let Some(prev) = self.active_tab() {
            prev.is_active
                .store(false, std::sync::atomic::Ordering::SeqCst);
            prev.hide();
        }
        let new_tab = &self.tabs[new_idx];
        new_tab
            .is_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Calling show() sets visible=true AND immediately calls wpe_view_resized,
        // which forces the AcceleratedBackingStore to recomposite and emit a fresh
        // render_buffer even when the page DOM hasn't changed since this tab was
        // last hidden. Without the forced resized call, WebKit has no reason to
        // repaint a static page, so the shared OsrFrame stays stuck on the
        // previous tab's last frame until something triggers a DOM mutation.
        let (cur_w, cur_h) = self
            .engine_state
            .lock()
            .map(|st| (st.width, st.height))
            .unwrap_or((800, 600));
        new_tab.show(cur_w, cur_h);
        self.active_idx = Some(new_idx);
        // Update active_idx and read is_loading in a single lock acquisition
        // so the main thread never observes the new active_idx while the
        // is_loading_atomic still reflects the previous tab's state.
        let new_is_loading = if let Ok(mut st) = self.engine_state.lock() {
            st.active_idx = Some(new_idx);
            st.tabs.get(new_idx).map(|t| t.is_loading).unwrap_or(false)
        } else {
            false
        };
        self.is_loading_atomic
            .store(new_is_loading, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut frame) = self.frame.lock() {
            frame.needs_fresh = true;
        }
        tracing::info!(?id, new_idx, "webkit: select_tab");
        true
    }

    /// Close the active tab. Returns true if a tab was actually closed.
    /// Drops the WebView (which releases its display ref) and reassigns
    /// active to the previous tab (or none if this was the last).
    pub(crate) fn close_active(&mut self) -> bool {
        let Some(idx) = self.active_idx else {
            return false;
        };
        if idx >= self.tabs.len() {
            return false;
        }
        // Drop the WebView (disconnects signals + g_object_unref).
        let _ = self.tabs.remove(idx);

        // Pick a new active: prefer the tab now at the same index
        // (the one that used to be after us); if we removed the last,
        // fall back to the new last.
        let new_idx = if self.tabs.is_empty() {
            None
        } else if idx < self.tabs.len() {
            Some(idx)
        } else {
            Some(self.tabs.len() - 1)
        };
        self.active_idx = new_idx;
        if let Some(i) = new_idx {
            let next = &self.tabs[i];
            next.is_active
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let (cur_w, cur_h) = self
                .engine_state
                .lock()
                .map(|st| (st.width, st.height))
                .unwrap_or((800, 600));
            next.show(cur_w, cur_h);
        }

        if let Ok(mut st) = self.engine_state.lock() {
            if idx < st.tabs.len() {
                st.tabs.remove(idx);
            }
            st.active_idx = new_idx;
        }
        if let Ok(mut frame) = self.frame.lock() {
            frame.needs_fresh = true;
        }
        true
    }

    /// Close the tab identified by `id`. Unlike `close_active` this correctly
    /// handles closing a background tab without disturbing the active one.
    ///
    /// - If the closed tab IS active: fall back to `close_active` logic
    ///   (pick the next tab in strip order as the new active).
    /// - If the closed tab is NOT active but has a lower index than
    ///   `active_idx`: decrement `active_idx` by one so it keeps pointing at
    ///   the same logical tab after the removal shifts everything left.
    /// - If it doesn't exist: return false.
    pub(crate) fn close_tab(&mut self, id: TabId) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        let was_active = self.active_idx == Some(idx);
        if was_active {
            // Delegate to the existing close_active logic which handles
            // picking the replacement tab and waking it.
            return self.close_active();
        }
        // Closing a background tab: drop the entry, leave the active tab
        // untouched, and adjust active_idx if needed.
        let _ = self.tabs.remove(idx);
        // If the closed index was below the active index, the active entry
        // has shifted left by one — correct for that.
        if let Some(ref mut active) = self.active_idx {
            if idx < *active {
                *active -= 1;
            }
        }
        // Mirror removal into engine_state.
        if let Ok(mut st) = self.engine_state.lock() {
            if idx < st.tabs.len() {
                st.tabs.remove(idx);
            }
            // Adjust engine_state.active_idx by the same rule.
            if let Some(ref mut a) = st.active_idx {
                if idx < *a {
                    *a -= 1;
                }
            }
        }
        tracing::info!(?id, idx, "webkit: close_tab (background tab)");
        true
    }

    pub(crate) fn navigate(&mut self, url: &str) {
        if let Some(tab) = self.active_tab() {
            tab.load_uri(url);
        }
        if let Ok(mut st) = self.engine_state.lock() {
            if let Some(idx) = st.active_idx {
                if let Some(tab_info) = st.tabs.get_mut(idx) {
                    tab_info.url = url.to_owned();
                    tab_info.is_loading = true;
                }
            }
        }
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if let Ok(mut st) = self.engine_state.lock() {
            st.width = width;
            st.height = height;
        }
        // Resize every tab's toplevel — hidden tabs need correct dims
        // so when they're activated their next paint matches the
        // shared frame's size (otherwise is_osr_frame_fresh rejects).
        for tab in &self.tabs {
            tab.resize(width, height);
        }
        if let Ok(mut frame) = self.frame.lock() {
            let new_len = (width * height * 4) as usize;
            frame.width = width;
            frame.height = height;
            frame.pixels.resize(new_len, 0);
            frame.needs_fresh = true;
        }
    }

    /// Monotonic millisecond clock used for `wpe_event_*_new` time stamps.
    /// WebKit reads it back via `wpe_event_get_time`; only the relative
    /// ordering matters, so a process-start epoch is fine.
    fn event_time_ms() -> u32 {
        static ONCE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let epoch = *ONCE.get_or_init(std::time::Instant::now);
        epoch.elapsed().as_millis() as u32
    }

    /// Construct and dispatch a `WPEEvent` for this runtime's active tab.
    /// `make` is invoked with the live `WPEView*` and the current time and
    /// must return a freshly-owned event (`wpe_event_*_new` already adds
    /// one ref; we unref after `wpe_view_event` takes its own).
    fn dispatch_event<F>(&self, make: F)
    where
        F: FnOnce(*mut WPEView, u32) -> *mut WPEEvent,
    {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let view = tab.wpe_view;
        if view.is_null() {
            return;
        }
        let time = Self::event_time_ms();
        // SAFETY: view is a live WPEView held by TabEntry; the factory
        // closure receives it for the duration of this call. Both new()
        // and wpe_view_event are thread-bound to the GLib main loop —
        // dispatch_* always runs on the worker thread.
        let event = make(view, time);
        if event.is_null() {
            return;
        }
        unsafe {
            wpe_view_event(view, event);
            wpe_event_unref(event);
        }
    }

    /// Run `script` in the active tab's main JavaScript world. Fire and
    /// forget: WebKit posts the call to its UI process and returns
    /// immediately; the result (if any) lands on an async callback that
    /// we don't currently consume. Used by `WebKitEngine::dispatch` to
    /// implement vim-style scrolling and any other catch-all action.
    pub(crate) fn eval_js(&self, script: &str) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.web_view.is_null() {
            return;
        }
        let Ok(script_c) = CString::new(script) else {
            tracing::warn!("webkit: eval_js: script contained NUL byte");
            return;
        };
        // SAFETY: web_view is held by TabEntry for the tab's lifetime;
        // script_c lives until the end of this call (length=-1 means
        // strlen, so WebKit reads up to the NUL terminator). All other
        // ptrs are NULL (default JS world, no source URI, no callback).
        unsafe {
            webkit_web_view_evaluate_javascript(
                tab.web_view,
                script_c.as_ptr(),
                -1,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                None,
                std::ptr::null_mut(),
            );
        }
    }

    pub(crate) fn dispatch_keyboard(&self, key_code: u32, pressed: bool, modifiers: u32) {
        let event_type = if pressed {
            WPEEventType_WPE_EVENT_KEYBOARD_KEY_DOWN
        } else {
            WPEEventType_WPE_EVENT_KEYBOARD_KEY_UP
        };
        self.dispatch_event(|view, time| unsafe {
            wpe_event_keyboard_new(
                event_type,
                view,
                WPEInputSource_WPE_INPUT_SOURCE_KEYBOARD,
                time,
                modifiers,
                // `keycode` is the hardware code; `keyval` is the XKB
                // keysym. For Phase-2 buffr passes a single value
                // through `key_code`; route it to both so chrome key
                // handling reads either field correctly.
                key_code,
                key_code,
            )
        });
    }

    pub(crate) fn dispatch_pointer_motion(&self, x: i32, y: i32, modifiers: u32) {
        self.dispatch_event(|view, time| unsafe {
            wpe_event_pointer_move_new(
                WPEEventType_WPE_EVENT_POINTER_MOVE,
                view,
                WPEInputSource_WPE_INPUT_SOURCE_MOUSE,
                time,
                modifiers,
                x as f64,
                y as f64,
                // delta_x/delta_y are the relative motion since the last
                // POINTER_MOVE; buffr-app sends absolute positions so we
                // don't carry per-tab deltas yet. WebKit copes fine — it
                // primarily uses x/y for hit-testing.
                0.0,
                0.0,
            )
        });
    }

    pub(crate) fn dispatch_pointer_button(
        &self,
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    ) {
        let event_type = if pressed {
            WPEEventType_WPE_EVENT_POINTER_DOWN
        } else {
            WPEEventType_WPE_EVENT_POINTER_UP
        };
        // WPE asserts `!pressCount || type == WPE_EVENT_POINTER_DOWN`:
        //   - POINTER_DOWN: press_count > 0 (1 for single click; 2 for double).
        //   - POINTER_UP:   press_count must be 0.
        // Passing 1 on UP raised a CRITICAL assertion in the GTK log.
        // Single-click is fine; double-click detection is the chrome layer's
        // job and we'd need to track a per-button timer here for true counts.
        let press_count = if pressed { 1 } else { 0 };
        self.dispatch_event(|view, time| unsafe {
            wpe_event_pointer_button_new(
                event_type,
                view,
                WPEInputSource_WPE_INPUT_SOURCE_MOUSE,
                time,
                modifiers,
                button,
                x as f64,
                y as f64,
                press_count,
            )
        });
    }

    pub(crate) fn dispatch_axis(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        // Pure horizontal+vertical zero → treat as "scroll stop" so WebKit
        // can release momentum state.
        let is_stop = (delta_x == 0 && delta_y == 0) as gboolean;
        self.dispatch_event(|view, time| unsafe {
            wpe_event_scroll_new(
                view,
                WPEInputSource_WPE_INPUT_SOURCE_MOUSE,
                time,
                modifiers,
                delta_x as f64,
                delta_y as f64,
                // `precise_deltas = TRUE` for trackpad-style smooth
                // scrolling. CEF input is already in pixel units, so this
                // matches the scale WebKit's smooth-scroll code expects.
                1,
                is_stop,
                x as f64,
                y as f64,
            )
        });
    }

    pub(crate) fn any_audio_active(&self) -> bool {
        self.tabs.iter().any(|t| t.is_playing_audio())
    }

    /// Adjust zoom on the active tab.
    ///
    /// `delta == 0.0` → reset to 1.0; otherwise the current level has
    /// `delta` added and is clamped to `[0.25, 5.0]`. The result is written
    /// back to the shared `zoom_level` so the main thread's
    /// `active_zoom_level()` returns the updated value immediately.
    pub(crate) fn set_zoom(&mut self, delta: f64) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let new_level = if delta == 0.0 {
            1.0_f64
        } else {
            let current = self.zoom_level.lock().map(|g| *g).unwrap_or(1.0);
            (current + delta).clamp(0.25, 5.0)
        };
        // SAFETY: web_view is valid for the tab's lifetime; all calls are on
        // the GLib worker thread.
        unsafe {
            webkit_web_view_set_zoom_level(tab.web_view, new_level);
        }
        if let Ok(mut guard) = self.zoom_level.lock() {
            *guard = new_level;
        }
        tracing::debug!(new_level, delta, "webkit: zoom updated");
    }

    /// Reorder tabs: move the entry at `from` to position `to`.
    ///
    /// Both indices are pre-move positions. When `to > from` the destination
    /// is decremented by 1 to account for the removal shift before inserting.
    /// The same mutation is applied to `engine_state.tabs` and `active_idx`
    /// is recomputed by locating the previously-active `TabId` in the new
    /// order.
    pub(crate) fn move_tab(&mut self, from: usize, to: usize) {
        let len = self.tabs.len();
        if from >= len || to >= len {
            tracing::debug!(from, to, len, "webkit: move_tab: index out of bounds");
            return;
        }
        if from == to {
            return;
        }
        // Remember which tab is active so we can re-locate it after the move.
        let active_id = self.active_idx.and_then(|i| self.tabs.get(i)).map(|t| t.id);

        // Remove-then-insert in the worker's tab vec.
        let entry = self.tabs.remove(from);
        let insert_at = if to > from { to - 1 } else { to };
        self.tabs.insert(insert_at, entry);

        // Fix active_idx by finding the active tab's id in the new order.
        self.active_idx = active_id.and_then(|id| self.tabs.iter().position(|t| t.id == id));

        // Mirror the same reorder into engine_state.tabs and active_idx.
        if let Ok(mut st) = self.engine_state.lock() {
            if from < st.tabs.len() && to < st.tabs.len() {
                let info = st.tabs.remove(from);
                let insert_at_es = if to > from { to - 1 } else { to };
                st.tabs.insert(insert_at_es, info);
            }
            st.active_idx = self.active_idx;
        }

        tracing::info!(from, to, insert_at, "webkit: move_tab");
    }

    // ── Find-in-page (webkit_find_controller_*) ───────────────────────────────

    /// Begin an in-page find session on the active tab's WebView.
    ///
    /// Uses CASE_INSENSITIVE | WRAP_AROUND always; adds BACKWARDS when
    /// `forward == false`. `max_match_count` is set to `u32::MAX` so
    /// WebKit highlights every occurrence.
    pub(crate) fn start_find(&self, query: &str, forward: bool) {
        let Some(tab) = self.active_tab() else {
            tracing::debug!("webkit: start_find — no active tab");
            return;
        };
        if tab.web_view.is_null() {
            return;
        }
        let Ok(query_c) = CString::new(query) else {
            tracing::warn!("webkit: start_find: query contains NUL byte");
            return;
        };
        // WebKitFindOptions bitmask (from wpe_bindings.rs):
        //   NONE=0, CASE_INSENSITIVE=1, AT_WORD_STARTS=2,
        //   TREAT_MEDIAL_CAPITAL_AS_WORD_START=4, BACKWARDS=8, WRAP_AROUND=16
        const CASE_INSENSITIVE: u32 = 1;
        const BACKWARDS: u32 = 8;
        const WRAP_AROUND: u32 = 16;
        let opts = if forward {
            CASE_INSENSITIVE | WRAP_AROUND
        } else {
            CASE_INSENSITIVE | BACKWARDS | WRAP_AROUND
        };
        // SAFETY: web_view is valid for the tab's lifetime; all calls are on
        // the GLib worker thread.  webkit_web_view_get_find_controller returns
        // a borrowed ref that stays valid as long as the WebView is alive.
        unsafe {
            let fc = webkit_web_view_get_find_controller(tab.web_view);
            if fc.is_null() {
                tracing::warn!("webkit: start_find — null FindController");
                return;
            }
            webkit_find_controller_search(fc, query_c.as_ptr(), opts, u32::MAX);
        }
        tracing::debug!(query, forward, "webkit: start_find");
    }

    /// Cancel the active find session and remove all match highlights.
    pub(crate) fn stop_find(&self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.web_view.is_null() {
            return;
        }
        // SAFETY: see start_find.
        unsafe {
            let fc = webkit_web_view_get_find_controller(tab.web_view);
            if !fc.is_null() {
                webkit_find_controller_search_finish(fc);
            }
        }
        tracing::debug!("webkit: stop_find");
    }

    /// Toggle the WebKit web inspector for the active tab.
    ///
    /// Returns `Err` when there is no active tab. On success the inspector
    /// window opens (or closes if already open — WebKit toggles).
    pub(crate) fn open_devtools(&self) -> Result<(), String> {
        let Some(tab) = self.active_tab() else {
            return Err("no active tab".to_string());
        };
        if tab.web_view.is_null() {
            return Err("active tab has no WebView".to_string());
        }
        // SAFETY: web_view is valid for the tab's lifetime; all calls are on
        // the GLib worker thread. webkit_settings_set_enable_developer_extras
        // was already called in TabEntry::new, so the inspector is ready.
        unsafe {
            webkit_web_view_toggle_inspector(tab.web_view);
        }
        tracing::info!("webkit: open_devtools: inspector toggled");
        Ok(())
    }
}
