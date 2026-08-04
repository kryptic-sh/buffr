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

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use std::collections::VecDeque;

use buffr_core::cursor::SharedCursorState;
use buffr_core::hint::{HintEventSink, parse_console_event};
use buffr_engine::permissions::{PendingPermission, PermissionsQueue};
use buffr_engine::popup::PopupQueue;
use buffr_engine::types::MediaType;
use buffr_engine::{
    AudioEvent as EngineAudioEvent, ContextMenuRequest, PromptOutcome, SharedOsrFrame,
    SharedOsrViewState, TabId,
};

/// Shared audio-event queue for the WPE backend.
///
/// Written by the per-tab `buffrAudio` UCM signal handler on the GLib worker
/// thread; drained by `WebKitEngine::drain_audio_events` on any thread.
pub(crate) type WpeAudioEventQueue = Arc<Mutex<VecDeque<EngineAudioEvent>>>;

pub(crate) fn new_audio_event_queue() -> WpeAudioEventQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Thread-safe context-menu request queue for the WPE backend.
///
/// Uses `buffr_engine::ContextMenuRequest` (the neutral type holding raw flags
/// like `link_url`, `media_type`, etc.) so the apps layer can call
/// `build_context_menu_items_from_neutral` with live `can_go_back` / `is_loading`
/// values at display time rather than building the item list inside the signal handler.
pub(crate) type WpeContextMenuSink = Arc<Mutex<std::collections::VecDeque<ContextMenuRequest>>>;

pub(crate) fn new_wpe_context_menu_sink() -> WpeContextMenuSink {
    Arc::new(Mutex::new(std::collections::VecDeque::new()))
}

use super::egl::EglWorker;
use super::ffi::*;
use super::worker::EngineState;
use super::wpe_subclass::{
    BuffrDisplayHandle, BuffrDisplayWaylandHandle, ViewCtx, WpeDisplayKind, ack_pending_buffer,
    attach_view_ctx, buffr_display_take_last_view, buffr_display_wayland_new,
    buffr_input_method_context_cancel, buffr_input_method_context_commit,
    buffr_input_method_context_new, buffr_input_method_context_set_preedit,
    buffr_view_wayland_set_rect,
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
    /// True when the owning engine runs in private (incognito) mode. Copied
    /// from `EngineState::private` at construction and surfaced in
    /// [`TabInfo::to_summary`] — this used to be hardcoded `false` (W7).
    pub is_private: bool,
}

impl TabInfo {
    pub(crate) fn new(id: TabId, url: &str, is_private: bool) -> Self {
        Self {
            id,
            url: url.to_owned(),
            title: url.to_owned(),
            is_loading: false,
            progress: 0.0,
            is_pinned: false,
            is_private,
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
            private: self.is_private,
        }
    }
}

// ── GLib signal helpers (raw FFI without gtk4 dep) ────────────────────────────

// SAFETY: These symbols are provided by GLib (linked via wpe-webkit-2.0).
unsafe extern "C" {
    fn g_signal_handler_disconnect(instance: *mut std::os::raw::c_void, handler_id: u64);
    fn g_object_unref(object: *mut std::os::raw::c_void);
    fn g_object_ref(object: *mut std::os::raw::c_void) -> *mut std::os::raw::c_void;
    fn g_free(ptr: *mut std::os::raw::c_void);
    /// Schedule a one-shot idle callback on the GLib default main context.
    /// GSourceFunc: returns 0 (G_SOURCE_REMOVE) to run once.
    fn g_idle_add(
        function: unsafe extern "C" fn(*mut std::os::raw::c_void) -> i32,
        data: *mut std::os::raw::c_void,
    ) -> u32;
}

// SAFETY: jsc_value_to_string is exported by the JavaScriptCore library that
// wpe-webkit-2.0 depends on. It allocates a UTF-8 string that the caller
// must free with g_free. Symbol not in the bindgen allowlist because the
// JSCValue type was excluded; declared here manually for the script-message
// signal handler.
unsafe extern "C" {
    fn jsc_value_to_string(value: *mut std::os::raw::c_void) -> *mut std::os::raw::c_char;
}

// SAFETY: g_memory_input_stream_new_from_data is a GLib/GIO symbol not in the
// bindgen allowlist (GIO stream types were excluded from the allowlist).
// Takes (data, len, destroy_func) and returns a GInputStream*.
// When destroy_func is non-NULL, GLib calls it with the data pointer when the
// stream is fully consumed — for heap-allocated bytes we pass g_free so GLib
// takes ownership.
unsafe extern "C" {
    fn g_memory_input_stream_new_from_data(
        data: *const std::os::raw::c_void,
        len: isize,
        destroy: Option<unsafe extern "C" fn(*mut std::os::raw::c_void)>,
    ) -> *mut super::ffi::GInputStream;
}

// SAFETY: g_type_check_instance_is_a is a GLib type system function not in
// the bindgen allowlist (the GTypeInstance introspection helpers were excluded).
// Returns TRUE when `instance` is-a `iface_type` (including inherited types).
// Equivalent to G_TYPE_CHECK_INSTANCE_TYPE / G_IS_OBJECT macros.
unsafe extern "C" {
    fn g_type_check_instance_is_a(
        instance: *mut std::os::raw::c_void,
        iface_type: super::ffi::GType,
    ) -> super::ffi::gboolean;
}

// ── WPE Wayland FFI note (#144) ───────────────────────────────────────────────
//
// `wpe_display_wayland_new`, `wpe_display_wayland_connect`, and
// `wpe_view_wayland_get_wl_surface` are generated by bindgen from the
// wpe-wayland umbrella header added to build.rs and arrive via
// `use super::ffi::*` above. No manual `extern "C"` declarations needed here.
//
// `WPEDisplayWayland` is a GObject subclass of `WPEDisplay`; the bindgen-
// emitted function signatures use `*mut WPEDisplayWayland` but we can safely
// upcast/downcast between `*mut WPEDisplayWayland` and `*mut WPEDisplay`
// because GObject subclasses are memory-layout-compatible at the head.

// ── WpePermissionRequestPtr ───────────────────────────────────────────────────

/// Opaque wrapper around a `*mut WebKitPermissionRequest` that is safe to
/// send across threads.
///
/// Safety contract (mirrors `WpeBufferPtr` in wpe_subclass.rs):
/// - The pointer is produced by `g_object_ref` in the signal handler; the ref
///   is released exactly once in `resolve_permission` (or the engine drop path)
///   via `g_object_unref`.
/// - Raw pointer access is only ever performed on the GLib worker thread.
/// - The `Arc<Mutex<HashMap<…, WpePermissionRequestPtr>>>` (`pending_permissions`)
///   is the sole owner between signal fire and resolve; `Arc<AtomicU64>` is used
///   only to mint IDs with no pointer access.
#[repr(transparent)]
pub(crate) struct WpePermissionRequestPtr(pub(crate) *mut super::ffi::WebKitPermissionRequest);
unsafe impl Send for WpePermissionRequestPtr {}
unsafe impl Sync for WpePermissionRequestPtr {}

// Console-bridge JS injected at document start. Overrides console.log to
// forward messages with the buffr hint sentinel to the native `buffrHint`
// script-message handler registered via WebKitUserContentManager.
//
// Pattern: scrape sentinel-prefixed console.log lines from the JS side.
//
// H5 / nonce note: this bridge deliberately matches on the BARE sentinel and
// forwards the WHOLE line, nonce included. It is untrusted transport — it runs
// in the page's own JS context, and `window.webkit.messageHandlers.buffrHint`
// is registered on the UCM so *any* frame can postMessage into it directly,
// bridge or no bridge. Authentication therefore happens on the Rust side, in
// `on_hint_script_message`, via `hint::parse_console_event(line, nonce)`.
// Splicing the nonce into this bridge instead would move the check into
// page-controlled JS and buy nothing, and would force a re-injection of the
// bridge on every nonce rotation.
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

/// Clipboard bridge JS injected at document start.
///
/// Intercepts `copy` and `cut` DOM events, captures the selected text, and
/// forwards it to the native `buffrClipboard` script-message handler so the
/// host can push it to the system clipboard via `hjkl-clipboard`.
///
/// Pattern mirrors hint mode's `HINT_CONSOLE_BRIDGE_JS`.
const CLIPBOARD_BRIDGE_JS: &str = r#"
(function() {
  function sendSel() {
    var text = window.getSelection ? window.getSelection().toString() : '';
    if (text) {
      try { window.webkit.messageHandlers.buffrClipboard.postMessage(text); } catch(e) {}
    }
  }
  document.addEventListener('copy', function() { setTimeout(sendSel, 0); }, true);
  document.addEventListener('cut',  function() { setTimeout(sendSel, 0); }, true);
})();
"#;

/// Favicon bridge JS injected at document-end on top-frame only.
///
/// Picks the best `<link rel="icon">` URL and posts `{ url, origin }` to
/// the native `buffrFavicon` UCM handler. Fires once at DOMContentLoaded
/// and again whenever `<head>` mutates (SPAs that swap icons mid-session).
pub(crate) const FAVICON_BRIDGE_JS: &str = r#"
(() => {
  const pick = () => {
    const links = Array.from(document.querySelectorAll('link[rel~="icon"], link[rel="shortcut icon"]'));
    if (links.length === 0) {
      return new URL('/favicon.ico', location.href).href;
    }
    const score = (link) => {
      const sizes = link.getAttribute('sizes') || '';
      const max = sizes.split(/\s+/).map(s => parseInt(s.split('x')[0], 10) || 0).reduce((a, b) => Math.max(a, b), 0);
      return max || 16;
    };
    links.sort((a, b) => score(b) - score(a));
    return new URL(links[0].href, location.href).href;
  };
  const send = () => {
    try {
      const url = pick();
      if (url) window.webkit.messageHandlers.buffrFavicon.postMessage(JSON.stringify({ url: url, origin: location.origin }));
    } catch(e) {}
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', send);
  } else {
    send();
  }
  new MutationObserver(send).observe(document.head || document.documentElement, { childList: true, subtree: true });
})();
"#;

/// Clipboard paste bridge JS injected at document-start on every frame.
///
/// Overrides `navigator.clipboard.readText` to call `fetch('buffr-clipboard:read')`
/// and intercepts DOM `paste` events on editable elements to insert host
/// clipboard text when no clipboardData text is already present.
///
/// The `buffr-clipboard` custom URI scheme is registered in `WpeRuntime::new`
/// via `webkit_web_context_register_uri_scheme`; the callback reads the host
/// clipboard via `hjkl_clipboard` and returns the bytes as `text/plain`.
const CLIPBOARD_PASTE_BRIDGE_JS: &str = r#"
(() => {
  // Async readText helper backed by the buffr-clipboard URI scheme.
  const readClipboard = async () => {
    try {
      const resp = await fetch('buffr-clipboard:read');
      if (!resp.ok) return '';
      return await resp.text();
    } catch (_) { return ''; }
  };

  // Override navigator.clipboard.readText so modern Clipboard API works.
  if (navigator.clipboard) {
    try {
      Object.defineProperty(navigator.clipboard, 'readText', {
        value: readClipboard,
        writable: true,
        configurable: true,
      });
    } catch (_) {
      // Some pages freeze navigator.clipboard — best effort.
    }
  }

  // Intercept paste events on editable elements so Ctrl+V works in
  // <input>, <textarea>, and contenteditable surfaces.
  document.addEventListener('paste', async (ev) => {
    const target = ev.target;
    if (!target) return;
    // Only handle when we have an editable target.
    const isEditable = target.matches && (
      target.matches('input:not([type=button]):not([type=submit]):not([type=reset])') ||
      target.matches('textarea') ||
      (target.isContentEditable === true)
    );
    if (!isEditable) return;
    // Skip if clipboardData already has text — means another source filled it.
    const existing = ev.clipboardData && ev.clipboardData.getData('text/plain');
    if (existing) return;
    ev.preventDefault();
    const text = await readClipboard();
    if (!text) return;
    // Insert at caret. execCommand('insertText') is deprecated but still
    // the most reliable cross-element insertion for our use case.
    document.execCommand('insertText', false, text);
  }, true);
})();
"#;

/// Audio bridge JS injected at document-start on every frame.
///
/// Listens for `play`, `pause`, `ended`, `emptied`, `abort` events on
/// `document` with capture=true so they bubble from any `<video>`/`<audio>`
/// element, recomputes the aggregate "any playing" state, and posts
/// `{ active: bool }` to the native `buffrAudio` UCM handler whenever the
/// aggregate flips.
///
/// The `readyState >= 2` (HAVE_CURRENT_DATA) guard prevents transient false
/// positives from elements that fired `play` but haven't loaded media yet.
pub(crate) const AUDIO_BRIDGE_JS: &str = r#"
(() => {
  let last = false;
  const compute = () => Array.from(document.querySelectorAll('video, audio'))
    .some(m => !m.paused && !m.ended && m.readyState >= 2);
  const tick = () => {
    const cur = compute();
    if (cur !== last) {
      last = cur;
      try { window.webkit.messageHandlers.buffrAudio.postMessage(JSON.stringify({ active: cur })); } catch (_) {}
    }
  };
  ['play', 'pause', 'ended', 'emptied', 'abort'].forEach(ev => {
    document.addEventListener(ev, tick, { capture: true, passive: true });
  });
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', tick);
  } else {
    tick();
  }
})();
"#;

/// Cursor bridge JS injected at document-start on every frame (#137).
///
/// Watches the hover cursor via `mousemove` (throttled to 50 ms / 20 Hz) and
/// posts `{ cursor: "<css-keyword>" }` to the native `buffrCursor` UCM handler
/// whenever the computed cursor style changes. A `mouseleave` event resets to
/// `"default"`. Custom `url()` cursors are stripped — the fallback keyword
/// from the value list (last item) is used instead.
pub(crate) const CURSOR_BRIDGE_JS: &str = r#"
(() => {
  let last = '';
  let lastT = 0;
  const send = (cursor) => {
    if (cursor === last) return;
    last = cursor;
    try {
      window.webkit.messageHandlers.buffrCursor.postMessage(JSON.stringify({ cursor }));
    } catch (_) {}
  };
  document.addEventListener('mousemove', (e) => {
    const now = performance.now();
    if (now - lastT < 50) return;
    lastT = now;
    const el = e.target || document.elementFromPoint(e.clientX, e.clientY);
    if (!el) return;
    // computed cursor may be "auto" — resolve via documentElement default ("default")
    let c = getComputedStyle(el).cursor;
    // strip url() prefix (custom cursors); fall back to fallback keyword if present
    if (c.startsWith('url(')) {
      const parts = c.split(',').map(s => s.trim());
      c = parts[parts.length - 1] || 'default';
    }
    send(c);
  }, { capture: true, passive: true });
  // Default cursor when the mouse leaves the document
  document.addEventListener('mouseleave', () => send('default'), { capture: true });
})();
"#;

/// Console.log shim for the media probe (#135).
///
/// Intercepts `console.log` calls whose first argument starts with the
/// `__buffr_media__:` sentinel emitted by the media-probe poll script and
/// forwards the **whole line** (sentinel + nonce + JSON) via the
/// `buffrMediaProbe` UCM handler. All other calls pass through to the
/// original `console.log` unchanged.
///
/// Installed at document-start on the top frame only (the poll script also
/// runs in the top frame). A try/catch wrapper prevents any breakage if the
/// UCM handler isn't registered yet.
///
/// H5 / nonce note: matches on the BARE sentinel and does not strip. See
/// `HINT_CONSOLE_BRIDGE_JS` — the nonce is verified in Rust by
/// `media_probe::parse(line, nonce)`, not here.
const MEDIA_PROBE_CONSOLE_SHIM_JS: &str = r#"
(() => {
  const orig = console.log.bind(console);
  console.log = function(...args) {
    try {
      if (typeof args[0] === 'string' && args[0].startsWith('__buffr_media__:')) {
        window.webkit.messageHandlers.buffrMediaProbe.postMessage(args[0]);
      }
    } catch (_) {}
    return orig.apply(console, args);
  };
})();
"#;

/// Console.log shim for edit mode (#134).
///
/// Intercepts `console.log` calls whose first argument starts with the
/// `__buffr_edit__:` sentinel emitted by `edit.js` and forwards the **whole
/// line** (sentinel + nonce + JSON) to the native `buffrEdit` UCM handler.
/// All other calls pass through to the original `console.log`.
///
/// Installed at document-start so the shim is in place before `edit.js`
/// (injected at document-end) fires its first event.
///
/// H5 / nonce note: matches on the BARE sentinel and does not strip. See
/// `HINT_CONSOLE_BRIDGE_JS` — the nonce is verified in Rust by
/// `edit::parse_console_event(line, nonce)`, not here.
const EDIT_CONSOLE_SHIM_JS: &str = r#"
(() => {
  const orig = console.log.bind(console);
  console.log = function(...args) {
    try {
      if (typeof args[0] === 'string' && args[0].startsWith('__buffr_edit__:')) {
        window.webkit.messageHandlers.buffrEdit.postMessage(args[0]);
      }
    } catch (_) {}
    return orig.apply(console, args);
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
    /// Runtime-wide nav-state atomics written by `on_load_changed` for the
    /// ACTIVE tab on COMMITTED / FINISHED. Read lock-free from the UI thread
    /// by `WebKitEngine::can_go_back` / `can_go_forward`.
    can_go_back: Arc<std::sync::atomic::AtomicBool>,
    can_go_forward: Arc<std::sync::atomic::AtomicBool>,
    /// Raw pointer to the WebView owning this ctx. Used by `on_load_changed`
    /// to query `webkit_web_view_can_go_back/forward` on COMMITTED/FINISHED.
    /// Valid for the signal handler's lifetime because the handler is
    /// disconnected (in `TabEntry::Drop`) before the WebView is unreffed.
    web_view: *mut WebKitWebView,
    /// Shared permissions queue — permission-request signal handler pushes
    /// `PendingPermission` entries here for the apps layer to drain.
    permissions_queue: PermissionsQueue,
    /// Map of resolve_id → g_object_ref'd WebKitPermissionRequest ptr.
    /// Written by the signal handler; consumed by Command::ResolvePermission.
    pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>>,
    /// Monotonic counter for minting resolve_ids. Starts at 1.
    permission_next_id: Arc<AtomicU64>,
}

// SAFETY: TabSignalCtx carries a raw `*mut WebKitWebView` that is only
// dereferenced inside GLib signal callbacks, which always run on the GLib
// worker thread. The pointer stays valid for the signal's lifetime because
// `TabEntry::drop` disconnects every signal before calling `g_object_unref`.
unsafe impl Send for TabSignalCtx {}
unsafe impl Sync for TabSignalCtx {}

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
        // Refresh can_go_back / can_go_forward on COMMITTED or FINISHED.
        // These are the first moments a navigation has committed so the
        // WebKit session history is in its final state for this load.
        if revealed && !ctx.web_view.is_null() {
            // SAFETY: web_view is valid for the signal handler's lifetime;
            // TabEntry::drop disconnects the signal before unreffing.
            let back = unsafe { webkit_web_view_can_go_back(ctx.web_view) != 0 };
            let fwd = unsafe { webkit_web_view_can_go_forward(ctx.web_view) != 0 };
            ctx.can_go_back.store(back, Ordering::Relaxed);
            ctx.can_go_forward.store(fwd, Ordering::Relaxed);
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

/// Per-tab heap context for the `buffrHint` UCM signal handler.
///
/// Carries the shared one-slot sink plus everything needed to authenticate an
/// inbound line: the nonce table and the tab it belongs to (H5).
pub(crate) struct HintSignalCtx {
    tab_id: TabId,
    sink: HintEventSink,
    console_nonces: buffr_core::ConsoleNonces,
}

/// `script-message-received::buffrHint` handler on the WebKitUserContentManager.
///
/// Prototype (GLib signal): `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries the full console line the page posted via
/// `window.webkit.messageHandlers.buffrHint.postMessage(msg)`.
/// We call `jsc_value_to_string` (malloc'd, must g_free), verify
/// sentinel + nonce with `buffr_core::hint::parse_console_event`, and write the
/// result into the shared `HintEventSink`.
///
/// H5: the nonce check is the authentication boundary. The UCM handler name is
/// registered on the whole web view, so any frame can postMessage here; only a
/// frame that was injected with the current hint nonce can produce a line that
/// verifies.
unsafe extern "C" fn on_hint_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: user_data is a Box<HintSignalCtx> owned until
    // drop_hint_signal_ctx fires on disconnect.
    let ctx = unsafe { &*(user_data as *const HintSignalCtx) };

    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let raw_str = unsafe { CStr::from_ptr(raw_ptr) }.to_string_lossy();
    let raw: &str = &raw_str;
    tracing::debug!(raw, "webkit: buffrHint script-message received");

    let nonce = ctx.console_nonces.hint(ctx.tab_id.0 as i32);
    match parse_console_event(raw, &nonce) {
        Some(Ok(event)) => {
            if let Ok(mut guard) = ctx.sink.lock() {
                *guard = Some(event);
            }
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, raw, "webkit: malformed hint event");
        }
        None => {
            // Either not one of our lines at all, or a forgery from a frame
            // that never learned the nonce. Indistinguishable by design, and
            // logging the body would hand any page a log-spam primitive.
            tracing::debug!("webkit: buffrHint message failed sentinel/nonce check (ignored)");
        }
    }
    // SAFETY: g_free the malloc'd string from jsc_value_to_string.
    unsafe { g_free(raw_ptr as *mut _) };
}

/// GLib `GClosureNotify` for the `Box<HintSignalCtx>` pointer leaked in
/// `TabEntry::new` for the `script-message-received::buffrHint` connection.
unsafe extern "C" fn drop_hint_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut HintSignalCtx) });
    }
}

// ── Clipboard script-message signal ──────────────────────────────────────────

/// `script-message-received::buffrClipboard` handler on the UCM.
///
/// Prototype: `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries the selected text the page posted via
/// `window.webkit.messageHandlers.buffrClipboard.postMessage(text)`.
/// We call `jsc_value_to_string`, then push the text to the system clipboard
/// via `hjkl_clipboard`.
///
/// `user_data` is a raw `*const Arc<hjkl_clipboard::Clipboard>` produced by
/// `Box::into_raw`; the matching `GClosureNotify` (`drop_clipboard_box`)
/// drops it on disconnect. The `Arc` points at the process-wide handle from
/// `super::clipboard::shared_clipboard()` (W10).
unsafe extern "C" fn on_clipboard_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let text_str = unsafe { std::ffi::CStr::from_ptr(raw_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { g_free(raw_ptr as *mut _) };

    if text_str.is_empty() {
        return;
    }
    tracing::debug!(
        len = text_str.len(),
        "webkit: buffrClipboard — pushing selection to system clipboard"
    );

    // SAFETY: user_data is a `*const Arc<hjkl_clipboard::Clipboard>` owned by
    // a Box that lives until `drop_clipboard_box` fires on disconnect.
    let cb = unsafe { &*(user_data as *const Arc<hjkl_clipboard::Clipboard>) };
    use hjkl_clipboard::{MimeType, Selection};
    if let Err(e) = cb.set(Selection::Clipboard, MimeType::Text, text_str.as_bytes()) {
        tracing::warn!(error = %e, "webkit: clipboard_set_text failed");
    }
}

/// GLib `GClosureNotify` for the `Box<Arc<hjkl_clipboard::Clipboard>>` pointer
/// leaked in `TabEntry::new` for the clipboard script-message connection.
/// Dropping the Box only releases this tab's `Arc` ref; the shared handle
/// outlives every tab.
unsafe extern "C" fn drop_clipboard_box(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut Arc<hjkl_clipboard::Clipboard>) });
    }
}

// ── Favicon script-message signal ────────────────────────────────────────────

/// Per-connection user_data for the `buffrFavicon` UCM signal.
struct FaviconSignalCtx {
    tab_id: TabId,
    favicon_sink: buffr_core::favicon::FaviconSink,
}

/// `script-message-received::buffrFavicon` handler on the UCM.
///
/// Prototype: `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries `{ url, origin }` posted by `FAVICON_BRIDGE_JS`.
/// We parse the URL, spawn a background thread to fetch + decode the
/// image, and push a `FaviconUpdate` into the shared sink.
unsafe extern "C" fn on_favicon_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let json_str = unsafe { std::ffi::CStr::from_ptr(raw_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { g_free(raw_ptr as *mut _) };

    if json_str.is_empty() {
        return;
    }

    // Parse { url, origin } from the JS object serialised to JSON.
    let url = match serde_json::from_str::<serde_json::Value>(&json_str) {
        Ok(v) => v
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_owned(),
        Err(e) => {
            tracing::warn!(error = %e, json = json_str, "webkit: buffrFavicon — JSON parse failed");
            return;
        }
    };

    if url.is_empty() {
        return;
    }
    tracing::debug!(url, "webkit: buffrFavicon — fetching icon");

    // SAFETY: user_data is a Box<FaviconSignalCtx> owned until
    // drop_favicon_signal_ctx fires on disconnect.
    let ctx = unsafe { &*(user_data as *const FaviconSignalCtx) };
    let browser_id = ctx.tab_id.0 as i32;
    let sink = Arc::clone(&ctx.favicon_sink);

    std::thread::spawn(move || {
        // Cap response to 1 MiB — favicons are tiny.
        const MAX_BYTES: u64 = 1024 * 1024;
        let mut resp = match ureq::get(&url).call() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(url, error = %e, "webkit: favicon fetch failed");
                return;
            }
        };
        if resp.status().as_u16() != 200 {
            tracing::warn!(
                url,
                status = resp.status().as_u16(),
                "webkit: favicon non-200"
            );
            return;
        }
        let buf = match resp.body_mut().with_config().limit(MAX_BYTES).read_to_vec() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(url, error = %e, "webkit: favicon read body failed");
                return;
            }
        };

        let img = match image::load_from_memory(&buf) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(url, error = %e, "webkit: favicon image decode failed");
                return;
            }
        };

        // Resize to 32x32.
        let img = img.resize_exact(32, 32, image::imageops::FilterType::Nearest);
        let rgba = img.to_rgba8();
        let width = rgba.width();
        let height = rgba.height();

        // Convert RGBA to packed BGRA u32 (`0xAA_RR_GG_BB` in little-endian
        // memory = `0xBB_GG_RR_AA` as a u32 value). The buffr-core doc says
        // `0xAA_RR_GG_BB` packed, which in Rust numeric literal means:
        //   bits [31:24] = A, [23:16] = R, [15:8] = G, [7:0] = B
        // Same encoding as CEF's favicon callback.
        let pixels: Vec<u32> = rgba
            .chunks_exact(4)
            .map(|p| {
                let (r, g, b, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
                (a << 24) | (r << 16) | (g << 8) | b
            })
            .collect();

        let update = buffr_core::favicon::FaviconUpdate {
            browser_id,
            width,
            height,
            pixels,
        };
        if let Ok(mut guard) = sink.lock() {
            guard.push_back(update);
        }
        tracing::debug!(url, browser_id, "webkit: favicon pushed to sink");
    });
}

/// GLib `GClosureNotify` for the `Box<FaviconSignalCtx>` pointer leaked in
/// `TabEntry::new` for the favicon script-message connection.
unsafe extern "C" fn drop_favicon_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut FaviconSignalCtx) });
    }
}

// ── Audio bridge: UCM script-message signal ───────────────────────────────────

/// Per-tab heap context for the `buffrAudio` UCM signal handler.
pub(crate) struct AudioSignalCtx {
    /// The tab's `browser_id` (= `TabId.0 as i32`) used as the
    /// `AudioEvent::browser_id` field so the apps layer can correlate events
    /// back to the tab.
    pub browser_id: i32,
    /// Shared audio-event queue written here; drained by
    /// `WebKitEngine::drain_audio_events`.
    pub audio_event_queue: WpeAudioEventQueue,
    /// Per-tab last-reported state. Prevents re-pushing redundant events when
    /// the JS sends the same aggregate value twice (e.g. two simultaneous
    /// `pause` fires resolve to the same false state).
    pub last_active: std::sync::atomic::AtomicBool,
}

/// GLib `GClosureNotify` for `Box<AudioSignalCtx>` leaked for the
/// `script-message-received::buffrAudio` connection.
unsafe extern "C" fn drop_audio_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut AudioSignalCtx) });
    }
}

/// `script-message-received::buffrAudio` handler on the UCM.
///
/// Prototype: `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries `{ active: bool }` posted by `AUDIO_BRIDGE_JS`.
/// On aggregate flip, pushes `EngineAudioEvent { browser_id, active }` to
/// the shared queue.
unsafe extern "C" fn on_audio_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let json_str = unsafe { std::ffi::CStr::from_ptr(raw_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { g_free(raw_ptr as *mut _) };

    if json_str.is_empty() {
        return;
    }

    let active: bool = match serde_json::from_str::<serde_json::Value>(&json_str) {
        Ok(v) => v.get("active").and_then(|a| a.as_bool()).unwrap_or(false),
        Err(e) => {
            tracing::warn!(
                error = %e,
                json = json_str,
                "webkit: buffrAudio — JSON parse failed"
            );
            return;
        }
    };

    // SAFETY: user_data is a Box<AudioSignalCtx> owned until
    // drop_audio_signal_ctx fires on disconnect.
    let ctx = unsafe { &*(user_data as *const AudioSignalCtx) };

    // Deduplicate: only push when the aggregate flips.
    use std::sync::atomic::Ordering;
    let prev = ctx.last_active.swap(active, Ordering::Relaxed);
    if prev == active {
        return;
    }

    tracing::debug!(
        browser_id = ctx.browser_id,
        active,
        "webkit: buffrAudio — audio state changed"
    );

    if let Ok(mut q) = ctx.audio_event_queue.lock() {
        q.push_back(EngineAudioEvent {
            browser_id: ctx.browser_id,
            active,
        });
    }
}

// ── Cursor bridge: UCM script-message signal (#137) ──────────────────────────

/// Per-tab heap context for the `buffrCursor` UCM signal handler.
pub(crate) struct CursorSignalCtx {
    /// The tab's `browser_id` (= `TabId.0 as i32`) stored via
    /// `CursorState::store` so the apps layer can route to the right window.
    pub browser_id: i32,
    /// Shared cursor state written here; read by
    /// `WebKitEngine::take_cursor_change`.
    pub cursor_state: SharedCursorState,
}

/// Map a CSS cursor keyword to a CEF `cef_cursor_type_t` raw discriminant.
///
/// Values mirror `cef_cursor_type_t` from CEF 147, which is also what
/// `apps/buffr-app/src/cef_translate.rs::cef_cursor_to_icon` expects.
pub(crate) fn css_cursor_to_cef_raw(css: &str) -> u32 {
    match css {
        "default" | "auto" => 0, // POINTER
        "crosshair" => 1,
        "pointer" => 2, // HAND
        "text" => 3,    // IBEAM
        "wait" => 4,
        "help" => 5,
        "e-resize" => 6,
        "n-resize" => 7,
        "ne-resize" => 8,
        "nw-resize" => 9,
        "s-resize" => 10,
        "se-resize" => 11,
        "sw-resize" => 12,
        "w-resize" => 13,
        "ns-resize" => 14,
        "ew-resize" => 15,
        "nesw-resize" => 16,
        "nwse-resize" => 17,
        "col-resize" => 18,
        "row-resize" => 19,
        "move" => 20,
        "vertical-text" => 21,
        "cell" => 22,
        "context-menu" => 23,
        "alias" => 24,
        "progress" => 25,
        "no-drop" => 26,
        "copy" => 27,
        "none" => 28,
        "not-allowed" => 29,
        "zoom-in" => 30,
        "zoom-out" => 31,
        "grab" => 32,
        "grabbing" => 33,
        _ => 0, // default
    }
}

/// GLib `GClosureNotify` for `Box<CursorSignalCtx>` leaked for the
/// `script-message-received::buffrCursor` connection.
unsafe extern "C" fn drop_cursor_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut CursorSignalCtx) });
    }
}

/// `script-message-received::buffrCursor` handler on the UCM.
///
/// Prototype: `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries `{ cursor: String }` posted by `CURSOR_BRIDGE_JS`.
/// Parses the CSS cursor keyword, maps it to a CEF discriminant, and calls
/// `cursor_state.store(browser_id, raw)`.
unsafe extern "C" fn on_cursor_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let json_str = unsafe { CStr::from_ptr(raw_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { g_free(raw_ptr as *mut _) };

    if json_str.is_empty() {
        return;
    }

    let cursor_css: String = match serde_json::from_str::<serde_json::Value>(&json_str) {
        Ok(v) => v
            .get("cursor")
            .and_then(|c| c.as_str())
            .unwrap_or("default")
            .to_owned(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                json = json_str,
                "webkit: buffrCursor — JSON parse failed"
            );
            return;
        }
    };

    let raw = css_cursor_to_cef_raw(&cursor_css);
    tracing::debug!(
        cursor = cursor_css,
        raw,
        "webkit: buffrCursor — cursor changed"
    );

    // SAFETY: user_data is a Box<CursorSignalCtx> owned until
    // drop_cursor_signal_ctx fires on disconnect.
    let ctx = unsafe { &*(user_data as *const CursorSignalCtx) };
    ctx.cursor_state.store(ctx.browser_id, raw);
}

// ── Media probe: UCM script-message signal (#135) ────────────────────────────

/// Per-tab heap context for the `buffrMediaProbe` UCM signal handler.
pub(crate) struct MediaProbeSignalCtx {
    /// Runtime-wide flag. Set to the `video` field from the last poll JSON.
    /// Last-writer-wins across tabs — correct for the "any tab has video"
    /// aggregate that `any_video_active` exposes.
    pub video_active: Arc<std::sync::atomic::AtomicBool>,
    /// Tab this handler belongs to — the key into `console_nonces` (H5).
    pub tab_id: TabId,
    /// Nonce table used to authenticate inbound lines (H5).
    pub console_nonces: buffr_core::ConsoleNonces,
}

/// GLib `GClosureNotify` for `Box<MediaProbeSignalCtx>` leaked for the
/// `script-message-received::buffrMediaProbe` connection.
unsafe extern "C" fn drop_media_probe_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut MediaProbeSignalCtx) });
    }
}

/// `script-message-received::buffrMediaProbe` handler on the UCM.
///
/// Prototype: `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries the JSON string forwarded by `MEDIA_PROBE_CONSOLE_SHIM_JS`
/// from the `__buffr_media__:` console.log sentinel emitted by the poll script.
/// Expected shape: `{ "media": bool, "video": bool }`.
///
/// Stores the `video` field into the runtime-wide `video_active` atomic.
/// The `media` field is intentionally ignored here — audio state is already
/// tracked via the dedicated `buffrAudio` bridge (#132).
unsafe extern "C" fn on_media_probe_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let line = unsafe { CStr::from_ptr(raw_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { g_free(raw_ptr as *mut _) };

    if line.is_empty() {
        return;
    }

    // SAFETY: user_data is a Box<MediaProbeSignalCtx> owned until
    // drop_media_probe_signal_ctx fires on disconnect.
    let ctx = unsafe { &*(user_data as *const MediaProbeSignalCtx) };

    // H5: verify sentinel + nonce before believing anything. The old code
    // poked at `serde_json::Value["video"]` on whatever the shim handed over,
    // so any frame could pin the idle inhibitor on.
    let nonce = ctx.console_nonces.page(ctx.tab_id.0 as i32);
    let video: bool = match buffr_core::media_probe::parse(&line, &nonce) {
        Some(Ok(event)) => event.video,
        Some(Err(e)) => {
            tracing::warn!(
                error = %e,
                "webkit: buffrMediaProbe — authentic line but JSON parse failed"
            );
            return;
        }
        None => {
            tracing::debug!(
                "webkit: buffrMediaProbe message failed sentinel/nonce check (ignored)"
            );
            return;
        }
    };

    use std::sync::atomic::Ordering;
    ctx.video_active.store(video, Ordering::Relaxed);
    tracing::debug!(video, "webkit: buffrMediaProbe — video state updated");
}

// ── Edit bridge: UCM script-message signal (#134) ────────────────────────────

/// Per-tab heap context for the `buffrEdit` UCM signal handler.
pub(crate) struct EditSignalCtx {
    /// Shared with `WebKitEngine::edit_sink`. When the inner `Option` is
    /// `Some`, decoded `EditConsoleEvent`s are pushed to the `VecDeque`.
    pub edit_sink: Arc<Mutex<Option<buffr_core::edit::EditEventSink>>>,
    /// Tab this handler belongs to — the key into `console_nonces` (H5).
    pub tab_id: TabId,
    /// Nonce table used to authenticate inbound lines (H5).
    pub console_nonces: buffr_core::ConsoleNonces,
}

/// GLib `GClosureNotify` for `Box<EditSignalCtx>` leaked for the
/// `script-message-received::buffrEdit` connection.
unsafe extern "C" fn drop_edit_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Box::into_raw.
        drop(unsafe { Box::from_raw(user_data as *mut EditSignalCtx) });
    }
}

/// `script-message-received::buffrEdit` handler on the UCM.
///
/// Prototype: `(WebKitUserContentManager*, JSCValue*, user_data*)`.
/// `js_value` carries the full console line forwarded by `EDIT_CONSOLE_SHIM_JS`
/// from the `__buffr_edit__:` console.log sentinel emitted by `edit.js`.
///
/// Verifies sentinel + nonce with `buffr_core::edit::parse_console_event` and
/// pushes `Ok` events to the shared sink. Errors are logged; lines that fail
/// the nonce check are dropped quietly (H5).
unsafe extern "C" fn on_edit_script_message(
    _ucm: *mut std::os::raw::c_void,
    js_value: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if js_value.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: jsc_value_to_string allocates a gchar* that we must g_free.
    let raw_ptr = unsafe { jsc_value_to_string(js_value) };
    if raw_ptr.is_null() {
        return;
    }
    let line = unsafe { CStr::from_ptr(raw_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { g_free(raw_ptr as *mut _) };

    if line.is_empty() {
        return;
    }

    tracing::debug!(line, "webkit: buffrEdit script-message received");

    // SAFETY: user_data is a Box<EditSignalCtx> owned until
    // drop_edit_signal_ctx fires on disconnect.
    let ctx = unsafe { &*(user_data as *const EditSignalCtx) };

    // H5: the shim now forwards the whole line, nonce included, and the check
    // happens here on the trusted side rather than in page-controlled JS.
    let nonce = ctx.console_nonces.page(ctx.tab_id.0 as i32);
    match buffr_core::edit::parse_console_event(&line, &nonce) {
        Some(Ok(event)) => {
            if let Ok(guard) = ctx.edit_sink.lock()
                && let Some(sink) = guard.as_ref()
                && let Ok(mut q) = sink.lock()
            {
                q.push_back(event);
            }
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, "webkit: authentic edit line but decode failed");
        }
        None => {
            tracing::debug!("webkit: buffrEdit message failed sentinel/nonce check (ignored)");
        }
    }
}

// ── Clipboard paste: buffr-clipboard URI scheme ───────────────────────────────

/// URI scheme callback for `buffr-clipboard:read`.
///
/// Registered once per process in `WpeRuntime::new` via
/// `webkit_web_context_register_uri_scheme`. WebKit calls this on the
/// GLib main thread whenever the page fetches `buffr-clipboard:read`.
///
/// We spawn a worker thread so the GLib main thread is never blocked by
/// the hjkl-clipboard Wayland roundtrip (which can take a few ms).
/// The worker calls `webkit_uri_scheme_request_finish` (or `_finish_error`)
/// on the same thread via `g_idle_add` to ensure the call lands on the
/// GLib main loop as required.
///
/// `user_data` is NULL — the worker pulls the process-wide `Arc<Clipboard>`
/// from `super::clipboard::shared_clipboard()` (W10) rather than opening a
/// fresh Wayland connection per request.
unsafe extern "C" fn on_clipboard_paste_scheme_request(
    request: *mut super::ffi::WebKitURISchemeRequest,
    _user_data: *mut std::os::raw::c_void,
) {
    use super::ffi::{
        GInputStream, WebKitURISchemeRequest, webkit_uri_scheme_request_finish,
        webkit_uri_scheme_request_get_scheme, webkit_uri_scheme_request_get_web_view,
    };

    if request.is_null() {
        return;
    }

    // ── Origin gate (W2): only buffr's own internal pages may read the ──────
    // host clipboard via this scheme.
    //
    // `buffr://` pages share this handler and the scheme is registered
    // CORS-enabled + secure, so without this gate ANY page (cross-origin or
    // iframe) could `fetch('buffr-clipboard:read')` and read the system
    // clipboard. Deny with an empty body — same pattern as the `None` branch
    // below — so fetch() still resolves (resp.ok = true, text() = ''). We're
    // on the GLib main thread and finish synchronously, so the deny path
    // needs no g_object_ref/worker thread: WebKit still owns the request.
    let finish_empty = |req: *mut WebKitURISchemeRequest| {
        let empty: *mut GInputStream =
            unsafe { g_memory_input_stream_new_from_data(std::ptr::null(), 0, None) };
        let ct = std::ffi::CString::new("text/plain;charset=utf-8").unwrap();
        unsafe { webkit_uri_scheme_request_finish(req, empty, 0, ct.as_ptr()) };
        if !empty.is_null() {
            unsafe { g_object_unref(empty as *mut _) };
        }
    };

    // Only the `buffr-clipboard` scheme is served; a plain `buffr://` fetch
    // must not read the clipboard.
    let scheme_ptr = unsafe { webkit_uri_scheme_request_get_scheme(request) };
    let is_clipboard_scheme = if scheme_ptr.is_null() {
        false
    } else {
        unsafe { std::ffi::CStr::from_ptr(scheme_ptr) }.to_bytes() == b"buffr-clipboard"
    };
    if !is_clipboard_scheme {
        tracing::warn!(
            "webkit: buffr-clipboard read denied — request scheme is not buffr-clipboard"
        );
        finish_empty(request);
        return;
    }

    // Only buffr-internal pages (buffr://*) may read the clipboard. The
    // requesting page URL comes from the request's WebView.
    let page_uri = {
        let web_view = unsafe { webkit_uri_scheme_request_get_web_view(request) };
        if web_view.is_null() {
            None
        } else {
            let uri_ptr = unsafe { webkit_web_view_get_uri(web_view) };
            if uri_ptr.is_null() {
                None
            } else {
                // SAFETY: WebKit guarantees a null-terminated UTF-8 URI string.
                Some(
                    unsafe { std::ffi::CStr::from_ptr(uri_ptr) }
                        .to_string_lossy()
                        .into_owned(),
                )
            }
        }
    };
    if !page_uri.as_deref().is_some_and(|u| u.starts_with("buffr://")) {
        tracing::warn!(
            "webkit: buffr-clipboard read denied — requesting page {:?} is not a buffr:// internal page",
            page_uri
        );
        finish_empty(request);
        return;
    }

    // WebKit guarantees the request object is live for the duration of this
    // callback. We need to keep it alive until the finish call on the idle
    // thread — g_object_ref bumps the ref count.
    unsafe { g_object_ref(request as *mut _) };

    // Capture a raw pointer to pass to the worker thread. The g_object_ref
    // above ensures the request outlives the thread.
    let req_raw = request as usize; // usize is Send

    std::thread::spawn(move || {
        use hjkl_clipboard::{MimeType, Selection};

        // Read host clipboard text through the process-wide handle (W10).
        // The backend was probed once (Wayland bg thread → X11 → OSC52) and
        // is `Send + Sync`; the Wayland backend is safe to call from any OS
        // thread — it has its own dedicated Wayland socket and does not share
        // state with the main GLib/Wayland compositor thread.
        let text: Option<String> = match super::clipboard::shared_clipboard() {
            None => None,
            Some(cb) => match cb.get(Selection::Clipboard, MimeType::Text) {
                Ok(bytes) => String::from_utf8(bytes).ok().filter(|s| !s.is_empty()),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "webkit: buffr-clipboard — clipboard get failed, returning empty"
                    );
                    None
                }
            },
        };

        // Marshal back to the GLib main loop via g_idle_add.
        // SAFETY: g_idle_callback is a valid GSourceFunc; closure_ptr is a
        // Box<…> we release inside the callback.
        type Closure = Box<dyn FnOnce() + Send>;
        let closure: Closure = Box::new(move || {
            // SAFETY: req_raw was produced by `request as usize`; the
            // g_object_ref above keeps it alive until this closure runs.
            let req = req_raw as *mut WebKitURISchemeRequest;

            match text {
                Some(t) => {
                    // Allocate a C string from the heap. We pass ownership to
                    // g_memory_input_stream_new_from_data via g_free destroy.
                    let bytes = t.into_bytes();
                    let len = bytes.len() as isize;
                    // Box::into_raw — ownership transferred to GLib via g_free.
                    let data_ptr = {
                        let mut b = bytes.into_boxed_slice();
                        let p = b.as_mut_ptr() as *mut std::os::raw::c_void;
                        std::mem::forget(b);
                        p
                    };
                    let stream: *mut GInputStream = unsafe {
                        g_memory_input_stream_new_from_data(
                            data_ptr as *const _,
                            len,
                            // GLib calls g_free(data_ptr) when the stream is
                            // fully consumed — GLib's allocator matches Rust's
                            // global allocator on Linux (both use the system
                            // malloc), so this is safe. `g_free` already has
                            // the GDestroyNotify signature, so no transmute is
                            // needed (the old identity `transmute::<T, T>` was
                            // a clippy error).
                            Some(g_free),
                        )
                    };
                    if stream.is_null() {
                        // OOM — finish with an empty stream rather than crashing.
                        let empty: *mut GInputStream = unsafe {
                            g_memory_input_stream_new_from_data(std::ptr::null(), 0, None)
                        };
                        let ct = std::ffi::CString::new("text/plain;charset=utf-8").unwrap();
                        unsafe { webkit_uri_scheme_request_finish(req, empty, 0, ct.as_ptr()) };
                        if !empty.is_null() {
                            unsafe { g_object_unref(empty as *mut _) };
                        }
                    } else {
                        let ct = std::ffi::CString::new("text/plain;charset=utf-8").unwrap();
                        unsafe {
                            webkit_uri_scheme_request_finish(req, stream, len as i64, ct.as_ptr())
                        };
                        unsafe { g_object_unref(stream as *mut _) };
                    }
                }
                None => {
                    // Return empty body rather than an error so fetch() resolves
                    // cleanly (resp.ok = true, resp.text() = '').
                    let empty: *mut GInputStream =
                        unsafe { g_memory_input_stream_new_from_data(std::ptr::null(), 0, None) };
                    let ct = std::ffi::CString::new("text/plain;charset=utf-8").unwrap();
                    unsafe { webkit_uri_scheme_request_finish(req, empty, 0, ct.as_ptr()) };
                    if !empty.is_null() {
                        unsafe { g_object_unref(empty as *mut _) };
                    }
                }
            }

            // Release our extra ref — WebKit now owns the request again.
            unsafe { g_object_unref(req as *mut _) };
        });

        // Package the closure into a raw pointer for g_idle_add.
        let closure_ptr = Box::into_raw(Box::new(closure)) as *mut std::os::raw::c_void;

        unsafe extern "C" fn g_idle_callback(data: *mut std::os::raw::c_void) -> i32 {
            if !data.is_null() {
                // SAFETY: data was produced by Box::into_raw(Box<Box<dyn FnOnce()…>>).
                let closure = unsafe { Box::from_raw(data as *mut Closure) };
                closure();
            }
            0 // G_SOURCE_REMOVE — run once
        }

        // SAFETY: g_idle_add schedules g_idle_callback on the GLib default
        // main context. closure_ptr is a valid heap pointer.
        unsafe { g_idle_add(g_idle_callback, closure_ptr) };
    });
}

// ── Popup create signal ───────────────────────────────────────────────────────

/// `create (WebKitWebView*, WebKitNavigationAction*)` signal handler.
///
/// Fires when JS calls `window.open(url)` or a link has `target=_blank`.
/// We extract the requested URL, push it to `PopupQueue` for the apps layer to
/// open as a new tab, then return NULL to suppress the native popup WebView.
///
/// Signal prototype (GLib): returns `WebKitWebView*` (NULL = suppress).
/// `user_data` is `*const Mutex<VecDeque<String>>` (the PopupQueue inner Arc
/// produced by `Arc::into_raw`).
unsafe extern "C" fn on_create(
    _web_view: *mut WebKitWebView,
    nav_action: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) -> *mut WebKitWebView {
    if user_data.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: user_data is an Arc<Mutex<VecDeque<String>>> (PopupQueue inner)
    // produced by Arc::into_raw; we borrow without consuming.
    let queue_ptr = user_data as *const Mutex<std::collections::VecDeque<String>>;
    let queue = unsafe { &*queue_ptr };

    if !nav_action.is_null() {
        // Extract the request URI from the navigation action.
        // SAFETY: nav_action is a WebKitNavigationAction* for the duration of
        // this signal handler (WebKit guarantees it is valid inside the handler).
        let request = unsafe {
            webkit_navigation_action_get_request(nav_action as *mut WebKitNavigationAction)
        };
        if !request.is_null() {
            let uri_ptr = unsafe { webkit_uri_request_get_uri(request) };
            if !uri_ptr.is_null() {
                let url = unsafe { std::ffi::CStr::from_ptr(uri_ptr) }
                    .to_string_lossy()
                    .into_owned();
                if !url.is_empty() {
                    tracing::debug!(url, "webkit: create signal — pushing to popup_queue");
                    if let Ok(mut q) = queue.lock() {
                        q.push_back(url);
                    }
                }
            }
        }
    }
    // Return NULL: suppress the native popup WebView — the apps layer opens
    // the URL from popup_queue as a new tab.
    std::ptr::null_mut()
}

/// GLib `GClosureNotify` for the `PopupQueue` Arc pointer leaked in
/// `TabEntry::new` for the `create` signal connection.
unsafe extern "C" fn drop_popup_queue_arc(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Arc::into_raw on the PopupQueue's
        // inner Mutex. Reconstitute + drop to decrement the refcount.
        drop(unsafe {
            Arc::from_raw(user_data as *const Mutex<std::collections::VecDeque<String>>)
        });
    }
}

// ── TLS error signal (#120) ───────────────────────────────────────────────────

/// `load-failed-with-tls-errors` on `WebKitWebView`.
///
/// Signal prototype: `(WebKitWebView*, failing_uri: gchar*,
///   certificate: GTlsCertificate*, errors: GTlsCertificateFlags, user_data)`.
/// Returns gboolean: TRUE = we handled it (suppress WebKit's default error page),
/// FALSE = use WebKit's default (which shows an informative "your connection
/// is not private" page that the user can navigate away from).
///
/// Safe default: log the error + return FALSE. This keeps WebKit's built-in
/// error page in place. A previous version auto-allowed every TLS error with
/// just a `tracing::warn`, which is a silent security regression — any
/// self-signed-cert MITM would succeed without user awareness. See follow-up
/// issue for proper prompt integration (`buffr-permissions` style) where the
/// user can choose to bypass per-host. Until then, fail closed.
unsafe extern "C" fn on_load_failed_with_tls_errors(
    web_view: *mut WebKitWebView,
    failing_uri: *const std::os::raw::c_char,
    _certificate: *mut GTlsCertificate,
    errors: GTlsCertificateFlags,
    _user_data: *mut std::os::raw::c_void,
) -> gboolean {
    if web_view.is_null() || failing_uri.is_null() {
        return 0;
    }
    let uri_str = unsafe { CStr::from_ptr(failing_uri) }.to_string_lossy();
    let mut flag_parts: Vec<&str> = Vec::new();
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_UNKNOWN_CA != 0 {
        flag_parts.push("unknown-ca");
    }
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_BAD_IDENTITY != 0 {
        flag_parts.push("bad-identity");
    }
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_NOT_ACTIVATED != 0 {
        flag_parts.push("not-activated");
    }
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_EXPIRED != 0 {
        flag_parts.push("expired");
    }
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_REVOKED != 0 {
        flag_parts.push("revoked");
    }
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_INSECURE != 0 {
        flag_parts.push("insecure");
    }
    if errors & GTlsCertificateFlags_G_TLS_CERTIFICATE_GENERIC_ERROR != 0 {
        flag_parts.push("generic-error");
    }
    let flags_desc = if flag_parts.is_empty() {
        "none".to_owned()
    } else {
        flag_parts.join(", ")
    };
    tracing::warn!(
        uri = %uri_str,
        flags = %flags_desc,
        "webkit: TLS certificate error — letting WebKit show its error page (fail closed)"
    );
    0 // FALSE: use WebKit's default error page
}

/// Extract `scheme://host[:port]` from a URI for use as a permission origin.
/// Keeps the scheme (unlike `extract_host_from_uri`) so the apps layer can
/// distinguish `https://` from `http://` origins in the prompt strip.
fn extract_origin_from_uri(uri: &str) -> String {
    // Find the scheme boundary.
    let Some(scheme_end) = uri.find("://") else {
        // No scheme — return raw string so the caller still has something.
        return uri.to_owned();
    };
    let scheme = &uri[..scheme_end];
    let after_scheme = &uri[scheme_end + 3..];
    // Take up to the first path separator.
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    format!("{scheme}://{host_port}")
}

// ── Context-menu signal (#121) ────────────────────────────────────────────────

/// Heap context for the `context-menu` signal. Carries the context-menu sink
/// so the handler can push `ContextMenuRequest` entries without holding the
/// full `TabSignalCtx` (which owns engine_state and other unneeded fields).
pub(crate) struct ContextSignalCtx {
    pub context_menu_sink: WpeContextMenuSink,
}

/// GLib GClosureNotify for `Box<ContextSignalCtx>` leaked for the context-menu
/// signal connection.
unsafe extern "C" fn drop_context_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: produced by Box::into_raw; reconstitute and drop.
        drop(unsafe { Box::from_raw(user_data as *mut ContextSignalCtx) });
    }
}

/// `context-menu` on `WebKitWebView`.
///
/// Signal prototype: `(WebKitWebView*, WebKitContextMenu*,
///   WebKitHitTestResult*, user_data)` → gboolean.
/// Returns TRUE to suppress WebKit's native context menu. We push a neutral
/// `ContextMenuRequest` into the context-menu sink so the apps layer can
/// render buffr's `ContextMenuOverlay`.
unsafe extern "C" fn on_context_menu(
    web_view: *mut WebKitWebView,
    context_menu: *mut WebKitContextMenu,
    hit_test: *mut WebKitHitTestResult,
    user_data: *mut std::os::raw::c_void,
) -> gboolean {
    if web_view.is_null() || user_data.is_null() {
        return 0;
    }
    // SAFETY: user_data is a Box<ContextSignalCtx> produced by Box::into_raw.
    let ctx = unsafe { &*(user_data as *const ContextSignalCtx) };

    // Extract position from the context menu object.
    let (x, y) = if !context_menu.is_null() {
        let mut mx: gint = 0;
        let mut my: gint = 0;
        let ok = unsafe { webkit_context_menu_get_position(context_menu, &mut mx, &mut my) };
        if ok != 0 { (mx, my) } else { (0, 0) }
    } else {
        (0, 0)
    };

    // Extract hit-test result flags.
    let (is_link, is_image, is_media, is_editable, _is_selection) = if !hit_test.is_null() {
        unsafe {
            (
                webkit_hit_test_result_context_is_link(hit_test) != 0,
                webkit_hit_test_result_context_is_image(hit_test) != 0,
                webkit_hit_test_result_context_is_media(hit_test) != 0,
                webkit_hit_test_result_context_is_editable(hit_test) != 0,
                webkit_hit_test_result_context_is_selection(hit_test) != 0,
            )
        }
    } else {
        (false, false, false, false, false)
    };

    // Extract link / image / media URIs.
    let link_url: Option<String> = if is_link && !hit_test.is_null() {
        let ptr = unsafe { webkit_hit_test_result_get_link_uri(hit_test) };
        if ptr.is_null() {
            None
        } else {
            let s = unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned();
            if s.is_empty() { None } else { Some(s) }
        }
    } else {
        None
    };
    let image_url: Option<String> = if is_image && !hit_test.is_null() {
        let ptr = unsafe { webkit_hit_test_result_get_image_uri(hit_test) };
        if ptr.is_null() {
            None
        } else {
            let s = unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned();
            if s.is_empty() { None } else { Some(s) }
        }
    } else {
        None
    };
    let media_url: Option<String> = if is_media && !hit_test.is_null() {
        let ptr = unsafe { webkit_hit_test_result_get_media_uri(hit_test) };
        if ptr.is_null() {
            None
        } else {
            let s = unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned();
            if s.is_empty() { None } else { Some(s) }
        }
    } else {
        None
    };

    // Page URL from the WebView.
    let page_url: String = {
        let ptr = unsafe { webkit_web_view_get_uri(web_view) };
        if ptr.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    };

    // Determine the neutral MediaType for the ContextMenuRequest.
    let media_type_neutral = if is_image {
        MediaType::Image
    } else if is_media {
        MediaType::Video
    } else {
        MediaType::None
    };

    let req = ContextMenuRequest {
        browser_id: 0,
        x,
        y,
        page_url,
        frame_url: String::new(),
        link_url,
        image_url,
        media_url,
        selection_text: None,
        is_editable,
        has_image_contents: is_image,
        media_type: media_type_neutral,
    };

    tracing::debug!(
        x,
        y,
        is_link,
        is_image,
        is_media,
        is_editable,
        "webkit: context-menu — pushing ContextMenuRequest"
    );

    if let Ok(mut queue) = ctx.context_menu_sink.lock() {
        // Cap at 8 entries — same as buffr_core::context_menu::CONTEXT_MENU_REQUEST_QUEUE_CAP.
        if queue.len() >= 8 {
            queue.pop_front();
        }
        queue.push_back(req);
    }

    // Return TRUE: suppress WebKit's native context menu.
    1
}

// ── Policy signal handlers (#123) ────────────────────────────────────────────

/// Internal URI schemes that WebKit should load directly.
///
/// Any scheme not in this list is passed to `xdg-open` on a user-initiated
/// navigation and ignored by WebKit.
/// `ws` and `wss` are included because WebKit handles WebSocket connections
/// internally via the same network session as HTTP.
const INTERNAL_SCHEMES: &[&str] = &[
    "http",
    "https",
    "file",
    "buffr",
    "buffr-clipboard",
    "about",
    "data",
    "blob",
    "ws",
    "wss",
];

/// `decide-policy` on `WebKitWebView`.
///
/// Signal prototype:
///   `(WebKitWebView*, WebKitPolicyDecision*, WebKitPolicyDecisionType, gpointer)` → void.
///
/// For NAVIGATION_ACTION and NEW_WINDOW_ACTION decisions we inspect the
/// request URI scheme:
/// - Internal schemes (`http`, `https`, `file`, `buffr`, `about`, `data`,
///   `blob`, `ws`, `wss`): call `webkit_policy_decision_use`.
/// - External schemes (`mailto:`, `magnet:`, etc.): on a user-initiated
///   navigation (gesture check), spawn `xdg-open` and call
///   `webkit_policy_decision_ignore`; the child is reaped on a detached
///   thread. Without a user gesture the launch is skipped (still ignored).
///
/// RESPONSE decisions are always passed to `webkit_policy_decision_use`
/// (allow all MIME types; WebKit handles Content-Disposition downloads via
/// the `decide-policy` RESPONSE path automatically).
///
/// user_data is NULL — no context needed.
unsafe extern "C" fn on_decide_policy(
    _web_view: *mut WebKitWebView,
    decision: *mut WebKitPolicyDecision,
    decision_type: WebKitPolicyDecisionType,
    _user_data: *mut std::os::raw::c_void,
) {
    if decision.is_null() {
        return;
    }

    // WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION = 0
    // WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION = 1
    // WEBKIT_POLICY_DECISION_TYPE_RESPONSE          = 2
    let is_navigation = decision_type
        == WebKitPolicyDecisionType_WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION
        || decision_type == WebKitPolicyDecisionType_WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION;

    if is_navigation {
        // Cast to WebKitNavigationPolicyDecision to extract the URI.
        let nav_decision = decision as *mut WebKitNavigationPolicyDecision;
        let nav_action =
            unsafe { webkit_navigation_policy_decision_get_navigation_action(nav_decision) };

        let uri: String = if nav_action.is_null() {
            String::new()
        } else {
            let req = unsafe { webkit_navigation_action_get_request(nav_action) };
            if req.is_null() {
                String::new()
            } else {
                let uri_ptr = unsafe { webkit_uri_request_get_uri(req) };
                if uri_ptr.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(uri_ptr) }
                        .to_string_lossy()
                        .into_owned()
                }
            }
        };

        // Determine scheme (everything before the first ':').
        let scheme = uri.split(':').next().unwrap_or("").to_ascii_lowercase();

        if INTERNAL_SCHEMES.contains(&scheme.as_str()) {
            tracing::debug!(uri, "webkit: decide-policy → use (internal scheme)");
            unsafe { webkit_policy_decision_use(decision) };
        } else {
            // Only launch external handlers on a user-initiated navigation;
            // a scripted/redirected load must not pop a handler out of
            // nowhere. A null nav_action counts as no gesture.
            let user_gesture = !nav_action.is_null()
                && unsafe { webkit_navigation_action_is_user_gesture(nav_action) } != 0;

            if !uri.is_empty() && user_gesture {
                tracing::debug!(
                    uri,
                    "webkit: decide-policy → xdg-open + ignore (external scheme, user gesture)"
                );
                if let Ok(mut child) = std::process::Command::new("xdg-open").arg(&uri).spawn() {
                    // Reap the child so xdg-open's wrapper process doesn't linger as a
                    // zombie (W8). xdg-open may itself daemonize; waiting on the child
                    // we spawned is still required to reap it.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            } else {
                tracing::warn!(
                    uri,
                    "webkit: external scheme without user gesture — not launching"
                );
            }
            unsafe { webkit_policy_decision_ignore(decision) };
        }
    } else {
        // RESPONSE decision — let WebKit decide (handles Content-Disposition download).
        unsafe { webkit_policy_decision_use(decision) };
    }
}

/// `permission-request` on `WebKitWebView`.
///
/// Signal prototype: `(WebKitWebView*, WebKitPermissionRequest*, gpointer)` → gboolean.
/// Returns TRUE (handled). We g_object_ref the request to keep it alive past
/// signal return, classify the capability via GType introspection, push a
/// `PendingPermission` to the shared queue for the apps layer, and resolve
/// it later when `Command::ResolvePermission` arrives.
///
/// `user_data` is a `*const TabSignalCtx` (same lifecycle as the other signals).
unsafe extern "C" fn on_permission_request(
    web_view: *mut WebKitWebView,
    request: *mut WebKitPermissionRequest,
    user_data: *mut std::os::raw::c_void,
) -> gboolean {
    use super::ffi::{
        webkit_geolocation_permission_request_get_type,
        webkit_notification_permission_request_get_type,
        webkit_user_media_permission_is_for_audio_device,
        webkit_user_media_permission_is_for_video_device,
        webkit_user_media_permission_request_get_type,
        webkit_website_data_access_permission_request_get_type,
    };
    use buffr_permissions::Capability;

    if request.is_null() || user_data.is_null() {
        // No request or no context — auto-deny to be safe.
        if !request.is_null() {
            unsafe { webkit_permission_request_deny(request) };
        }
        return 1;
    }

    // SAFETY: user_data is an Arc<TabSignalCtx> produced by Arc::into_raw.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };

    // Keep request alive past signal return. We'll g_object_unref in resolve.
    unsafe { g_object_ref(request as *mut _) };

    // ── Classify capability by GType ─────────────────────────────────────────
    let mut capabilities: Vec<Capability> = Vec::new();
    let request_void = request as *mut std::os::raw::c_void;

    // User media (camera / microphone)?
    let user_media_type = unsafe { webkit_user_media_permission_request_get_type() };
    if unsafe { g_type_check_instance_is_a(request_void, user_media_type) } != 0 {
        let um = request as *mut super::ffi::WebKitUserMediaPermissionRequest;
        if unsafe { webkit_user_media_permission_is_for_audio_device(um) } != 0 {
            capabilities.push(Capability::Microphone);
        }
        if unsafe { webkit_user_media_permission_is_for_video_device(um) } != 0 {
            capabilities.push(Capability::Camera);
        }
        // If neither flag is set, treat as microphone (shouldn't happen but be safe).
        if capabilities.is_empty() {
            capabilities.push(Capability::Microphone);
        }
    }
    // Geolocation?
    else if unsafe {
        g_type_check_instance_is_a(
            request_void,
            webkit_geolocation_permission_request_get_type(),
        )
    } != 0
    {
        capabilities.push(Capability::Geolocation);
    }
    // Notifications?
    else if unsafe {
        g_type_check_instance_is_a(
            request_void,
            webkit_notification_permission_request_get_type(),
        )
    } != 0
    {
        capabilities.push(Capability::Notifications);
    }
    // Website data access (storage, cookies cross-site)?
    else if unsafe {
        g_type_check_instance_is_a(
            request_void,
            webkit_website_data_access_permission_request_get_type(),
        )
    } != 0
    {
        // WebsiteDataAccess is a cross-site storage / cookie-access prompt.
        // Map to Other(0) — no single `Capability` variant covers it yet.
        capabilities.push(Capability::Other(0));
    }
    // Unknown subclass.
    else {
        capabilities.push(Capability::Other(0));
    }

    // ── Extract origin from the WebView URI ───────────────────────────────────
    let origin = if !web_view.is_null() {
        let uri_ptr = unsafe { webkit_web_view_get_uri(web_view) };
        if uri_ptr.is_null() {
            String::new()
        } else {
            extract_origin_from_uri(
                unsafe { CStr::from_ptr(uri_ptr) }
                    .to_string_lossy()
                    .as_ref(),
            )
        }
    } else {
        String::new()
    };

    // ── Mint resolve_id ───────────────────────────────────────────────────────
    let resolve_id = ctx
        .permission_next_id
        .fetch_add(1, AtomicOrdering::Relaxed)
        .to_string();

    tracing::debug!(
        origin,
        resolve_id,
        ?capabilities,
        "webkit: permission-request — queued for apps layer"
    );

    // ── Store ptr in pending map ──────────────────────────────────────────────
    if let Ok(mut map) = ctx.pending_permissions.lock() {
        map.insert(resolve_id.clone(), WpePermissionRequestPtr(request));
    }

    // ── Push PendingPermission to shared queue ────────────────────────────────
    if let Ok(mut q) = ctx.permissions_queue.lock() {
        q.push_back(PendingPermission {
            origin,
            capabilities,
            resolve_id: Some(resolve_id),
        });
    }

    // Return TRUE: signal is handled; WebKit won't auto-allow.
    1
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
    /// Signal ID for `script-message-received::buffrClipboard` on the UCM.
    /// 0 when the clipboard init failed.
    clipboard_script_message_received_id: u64,
    /// Signal ID for `script-message-received::buffrFavicon` on the UCM.
    /// 0 when favicon init failed.
    favicon_script_message_received_id: u64,
    /// Signal ID for `create` on this WebView (popup interception).
    create_signal_id: u64,
    /// Signal ID for `load-failed-with-tls-errors` (#120).
    tls_error_id: u64,
    /// Signal ID for `context-menu` (#121).
    context_menu_id: u64,
    /// Signal ID for `decide-policy` (#123).
    decide_policy_id: u64,
    /// Signal ID for `permission-request` (#123).
    permission_request_id: u64,
    /// Signal ID for `script-message-received::buffrAudio` on the UCM (#132).
    /// 0 when audio bridge init failed.
    audio_script_message_received_id: u64,
    /// Shared audio-event queue — this tab's `buffrAudio` handler pushes here.
    /// Kept on `TabEntry` so `Drop` can push a final `active=false` event.
    audio_event_queue: WpeAudioEventQueue,
    /// This tab's browser_id (= id.0 as i32) used in the close-cleanup event.
    audio_browser_id: i32,
    /// Signal ID for `script-message-received::buffrCursor` on the UCM (#137).
    /// 0 when cursor bridge init failed.
    cursor_script_message_received_id: u64,
    /// Signal ID for `script-message-received::buffrMediaProbe` on the UCM (#135).
    /// 0 when media probe init failed.
    media_probe_script_message_received_id: u64,
    /// Signal ID for `script-message-received::buffrEdit` on the UCM (#134).
    /// 0 when edit bridge init failed.
    edit_script_message_received_id: u64,
    /// Runtime-wide video-active flag. Same Arc as `WpeRuntime::video_active`
    /// and `WebKitEngine::video_active`. Kept here so `Drop` is self-contained.
    video_active: Arc<std::sync::atomic::AtomicBool>,
    /// Shared per-tab flag wired into both `ViewCtx` (pixel write gate)
    /// and `TabSignalCtx` (is_loading_atomic write gate). Owned by
    /// `WpeRuntime` so it can flip the active tab via `select_tab`.
    pub is_active: Arc<std::sync::atomic::AtomicBool>,
    /// Owned `BuffrInputMethodContext` attached to this tab's WebView.
    ///
    /// Created by `buffr_input_method_context_new()` and wired to the WebView
    /// via `webkit_web_view_set_input_method_context`.  NULL if construction
    /// failed (defensive — should not happen in practice).  `Drop` releases
    /// the GObject ref via `g_object_unref`.
    pub ime_ctx: *mut WebKitInputMethodContext,
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
    /// Same rationale as `WpeRuntime::new` — a flat list of per-tab shared
    /// sinks, not independently meaningful groups.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: TabId,
        url: &str,
        display: *mut WPEDisplay,
        is_native: bool,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
        is_active: Arc<std::sync::atomic::AtomicBool>,
        hint_sink: HintEventSink,
        popup_queue: PopupQueue,
        context_menu_sink: WpeContextMenuSink,
        favicon_sink: buffr_core::favicon::FaviconSink,
        can_go_back: Arc<std::sync::atomic::AtomicBool>,
        can_go_forward: Arc<std::sync::atomic::AtomicBool>,
        permissions_queue: PermissionsQueue,
        pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>>,
        permission_next_id: Arc<AtomicU64>,
        audio_event_queue: WpeAudioEventQueue,
        cursor_state: SharedCursorState,
        video_active: Arc<std::sync::atomic::AtomicBool>,
        edit_sink: Arc<Mutex<Option<buffr_core::edit::EditEventSink>>>,
        console_nonces: buffr_core::ConsoleNonces,
    ) -> Option<Self> {
        if display.is_null() {
            tracing::error!("webkit: TabEntry::new called with NULL display");
            return None;
        }

        // ── H5: mint this tab's console-IPC nonces ──────────────────────────
        //
        // Rotation granularity differs from `buffr-cef` on purpose. CEF
        // injects `edit.js` / the media poll imperatively from `on_load_end`,
        // so it can call `rotate_page` on every main-frame load. WebKit's
        // `WebKitUserContentManager` scripts are *declarative*: they are added
        // once here and WebKit re-runs them itself on every document load,
        // with whatever nonce was baked into the source at add time. Rotating
        // per load would therefore need every UCM script torn down and
        // re-added mid-navigation (and `remove_all_scripts` would take the
        // clipboard / favicon / audio / cursor bridges with it, and race
        // document-start). So the *page* nonce here is per-tab, not per-load.
        //
        // The hint nonce is unaffected — `hint.js` is evaluated imperatively
        // per `enter_hint_mode`, so `WebKitEngine::enter_hint_mode` calls
        // `rotate_hint` each time and gets true per-session rotation.
        let page_nonce = console_nonces.rotate_page(id.0 as i32);

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

        // ── Recover the WPEView and wire up per-path state ────────────────────
        //
        // OSR path: BuffrDisplay's `create_view` vmethod stashes the newly
        // created WPEView in a thread-local; `buffr_display_take_last_view`
        // pops it. We then attach a `ViewCtx` so `buffr_rust_render_buffer`
        // can find the shared OsrFrame and wake callback.
        //
        // Wayland path: WPEDisplayWayland creates a `WPEViewWayland` internally.
        // We skip the OSR-only `buffr_display_take_last_view` / `attach_view_ctx`
        // machinery (ViewCtx is not needed — render_buffer is never called on
        // our side).
        let wpe_view = if is_native {
            // Wayland path: WPEDisplayWayland owns the view lifecycle.
            // `webkit_web_view_get_view` is not in our bindings; instead, the
            // display already called into WPE's view creation machinery when we
            // passed it as the `display` construct-property. We cannot get the
            // WPEViewWayland pointer synchronously here without a signal.
            // wpe_view is intentionally NULL on the native path.
            tracing::info!("webkit: native Wayland path — skipping OSR ViewCtx attach");
            std::ptr::null_mut::<super::ffi::WPEView>()
        } else {
            // OSR path: pop the stash left by BuffrDisplay's create_view vmethod.
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
            wpe_view
        };

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
            can_go_back,
            can_go_forward,
            web_view,
            permissions_queue,
            pending_permissions,
            permission_next_id,
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
                WebKitUserContentInjectedFrames_WEBKIT_USER_CONTENT_INJECT_TOP_FRAME as INJECT_TOP,
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

                // Inject console bridge at document-start on the top frame
                // only. `hint.js` itself is evaluated via
                // `webkit_web_view_evaluate_javascript` (main frame, default
                // world), so a subframe copy of the bridge would never see one
                // of our lines — and injecting there is exactly what would
                // leak the nonce to a frame we do not trust (H5).
                let source_c = CString::new(HINT_CONSOLE_BRIDGE_JS).unwrap();
                let script = webkit_user_script_new(
                    source_c.as_ptr(),
                    INJECT_TOP,
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
                let ctx_box = Box::new(HintSignalCtx {
                    tab_id: id,
                    sink: Arc::clone(&hint_sink),
                    console_nonces: console_nonces.clone(),
                });
                let sink_arc = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
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
                    Some(drop_hint_signal_ctx),
                    0,
                )
            }
        };

        // ── Clipboard bridge: UCM script-message handler ──────────────────────
        //
        // 1. Inject CLIPBOARD_BRIDGE_JS at document-start so `copy`/`cut` DOM
        //    events forward the selection to the native `buffrClipboard` handler.
        // 2. Register `buffrClipboard` and wire `script-message-received::buffrClipboard`
        //    to push the text to the system clipboard via `hjkl_clipboard`.
        //
        // Pattern mirrors the hint IPC above — same UCM, same signal mechanism.
        let clipboard_script_message_received_id: u64 = unsafe {
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
                0
            } else {
                // Take a handle on the process-wide Clipboard (W10) — this used
                // to probe a fresh Wayland connection per tab. Failure is
                // non-fatal; the probe already logged.
                match super::clipboard::shared_clipboard() {
                    None => {
                        tracing::warn!(
                            "webkit: no system clipboard — copy/cut will not reach system clipboard"
                        );
                        0
                    }
                    Some(cb) => {
                        // Register the native message handler name.
                        let handler_name = CString::new("buffrClipboard").unwrap();
                        let _ok = webkit_user_content_manager_register_script_message_handler(
                            ucm,
                            handler_name.as_ptr(),
                            std::ptr::null(),
                        );

                        // Inject clipboard bridge at document-start on all frames.
                        let source_c = CString::new(CLIPBOARD_BRIDGE_JS).unwrap();
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

                        // Connect `script-message-received::buffrClipboard`.
                        // user_data is a Box<Clipboard> leaked via Box::into_raw;
                        // drop_clipboard_box reconstitutes + drops it on disconnect.
                        let cb_ptr = Box::into_raw(Box::new(cb)) as *mut std::os::raw::c_void;
                        let signal_name =
                            CString::new("script-message-received::buffrClipboard").unwrap();
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
                            >(on_clipboard_script_message)),
                            cb_ptr,
                            Some(drop_clipboard_box),
                            0,
                        )
                    }
                }
            }
        };

        // ── Favicon bridge: UCM script-message handler ────────────────────────
        //
        // 1. Inject FAVICON_BRIDGE_JS at document-end on top-frame only so
        //    <link rel="icon"> elements are present when the script runs.
        // 2. Register `buffrFavicon` and wire `script-message-received::buffrFavicon`
        //    to spawn a background fetch + decode + push into the shared sink.
        //
        // Pattern mirrors hint / clipboard IPC above.
        let favicon_script_message_received_id: u64 = unsafe {
            use super::ffi::{
                WebKitUserContentInjectedFrames_WEBKIT_USER_CONTENT_INJECT_TOP_FRAME as INJECT_TOP,
                WebKitUserScriptInjectionTime_WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_END as INJECT_END,
                webkit_user_content_manager_add_script,
                webkit_user_content_manager_register_script_message_handler,
                webkit_user_script_new, webkit_user_script_unref,
                webkit_web_view_get_user_content_manager,
            };

            let ucm = webkit_web_view_get_user_content_manager(web_view);
            if ucm.is_null() {
                0
            } else {
                // Register the native message handler name.
                let handler_name = std::ffi::CString::new("buffrFavicon").unwrap();
                let _ok = webkit_user_content_manager_register_script_message_handler(
                    ucm,
                    handler_name.as_ptr(),
                    std::ptr::null(),
                );

                // Inject favicon bridge at document-end on top frame only.
                let source_c = std::ffi::CString::new(FAVICON_BRIDGE_JS).unwrap();
                let script = webkit_user_script_new(
                    source_c.as_ptr(),
                    INJECT_TOP,
                    INJECT_END,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if !script.is_null() {
                    webkit_user_content_manager_add_script(ucm, script);
                    webkit_user_script_unref(script);
                }

                // Connect `script-message-received::buffrFavicon`.
                // user_data is a Box<FaviconSignalCtx> leaked via Box::into_raw;
                // drop_favicon_signal_ctx reconstitutes + drops it on disconnect.
                let ctx_box = Box::new(FaviconSignalCtx {
                    tab_id: id,
                    favicon_sink: Arc::clone(&favicon_sink),
                });
                let ctx_raw = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
                let signal_name =
                    std::ffi::CString::new("script-message-received::buffrFavicon").unwrap();
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
                    >(on_favicon_script_message)),
                    ctx_raw,
                    Some(drop_favicon_signal_ctx),
                    0,
                )
            }
        };

        // ── Clipboard paste bridge: inject user-script (#128) ────────────────
        //
        // Inject CLIPBOARD_PASTE_BRIDGE_JS at document-start on all frames.
        // This overrides `navigator.clipboard.readText` and intercepts DOM
        // `paste` events, both delegating to `fetch('buffr-clipboard:read')`
        // which is served by the URI scheme registered in `WpeRuntime::new`.
        //
        // No UCM signal handler needed — the response goes through the URI
        // scheme callback (`on_clipboard_paste_scheme_request`), not postMessage.
        unsafe {
            use super::ffi::{
                WebKitUserContentInjectedFrames_WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES as INJECT_ALL,
                WebKitUserScriptInjectionTime_WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START as INJECT_START,
                webkit_user_content_manager_add_script, webkit_user_script_new,
                webkit_user_script_unref, webkit_web_view_get_user_content_manager,
            };
            let ucm = webkit_web_view_get_user_content_manager(web_view);
            if !ucm.is_null() {
                let source_c = CString::new(CLIPBOARD_PASTE_BRIDGE_JS).unwrap();
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
                    tracing::debug!("webkit: CLIPBOARD_PASTE_BRIDGE_JS injected for tab {id:?}");
                }
            }
        }

        // ── Audio bridge: UCM script-message handler (#132) ──────────────────
        //
        // 1. Register `buffrAudio` native handler on the UCM.
        // 2. Inject AUDIO_BRIDGE_JS at document-start on all frames so media
        //    events (play/pause/ended/emptied/abort) are forwarded whenever the
        //    aggregate "any playing" state changes.
        // 3. Connect `script-message-received::buffrAudio` → on_audio_script_message
        //    which pushes `AudioEvent { browser_id, active }` to the shared queue.
        //
        // UCM registration is per-WebView (each WebView gets its own UCM from
        // webkit_web_view_get_user_content_manager), so we register here just
        // like buffrHint / buffrClipboard / buffrFavicon above.
        let audio_script_message_received_id: u64 = unsafe {
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
                0
            } else {
                // Register the native message handler name.
                let handler_name = CString::new("buffrAudio").unwrap();
                let _ok = webkit_user_content_manager_register_script_message_handler(
                    ucm,
                    handler_name.as_ptr(),
                    std::ptr::null(),
                );

                // Inject audio bridge at document-start on all frames.
                let source_c = CString::new(AUDIO_BRIDGE_JS).unwrap();
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

                // Connect `script-message-received::buffrAudio`.
                // user_data is a Box<AudioSignalCtx> leaked via Box::into_raw;
                // drop_audio_signal_ctx reconstitutes + drops it on disconnect.
                let ctx_box = Box::new(AudioSignalCtx {
                    browser_id: id.0 as i32,
                    audio_event_queue: Arc::clone(&audio_event_queue),
                    last_active: std::sync::atomic::AtomicBool::new(false),
                });
                let ctx_raw = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
                let signal_name = CString::new("script-message-received::buffrAudio").unwrap();
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
                    >(on_audio_script_message)),
                    ctx_raw,
                    Some(drop_audio_signal_ctx),
                    0,
                )
            }
        };

        // ── Cursor bridge: UCM script-message handler (#137) ─────────────────
        //
        // 1. Register `buffrCursor` native handler on the UCM.
        // 2. Inject CURSOR_BRIDGE_JS at document-start on all frames so
        //    `mousemove` events report the computed CSS cursor keyword via
        //    `buffrCursor` postMessage whenever it changes.
        // 3. Connect `script-message-received::buffrCursor` → on_cursor_script_message
        //    which maps the keyword to a CEF discriminant and calls cursor_state.store.
        let cursor_script_message_received_id: u64 = unsafe {
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
                0
            } else {
                // Register the native message handler name.
                let handler_name = CString::new("buffrCursor").unwrap();
                let _ok = webkit_user_content_manager_register_script_message_handler(
                    ucm,
                    handler_name.as_ptr(),
                    std::ptr::null(),
                );

                // Inject cursor bridge at document-start on all frames.
                let source_c = CString::new(CURSOR_BRIDGE_JS).unwrap();
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

                // Connect `script-message-received::buffrCursor`.
                // user_data is a Box<CursorSignalCtx> leaked via Box::into_raw;
                // drop_cursor_signal_ctx reconstitutes + drops it on disconnect.
                let ctx_box = Box::new(CursorSignalCtx {
                    browser_id: id.0 as i32,
                    cursor_state: Arc::clone(&cursor_state),
                });
                let ctx_raw = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
                let signal_name = CString::new("script-message-received::buffrCursor").unwrap();
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
                    >(on_cursor_script_message)),
                    ctx_raw,
                    Some(drop_cursor_signal_ctx),
                    0,
                )
            }
        };

        // ── Media probe: UCM script-message handler (#135) ───────────────────
        //
        // 1. Register `buffrMediaProbe` native handler on the UCM.
        // 2. Inject MEDIA_PROBE_INIT_JS at document-start on top-frame only so
        //    constructor patching is installed once per main document.
        // 3. Inject MEDIA_PROBE_CONSOLE_SHIM_JS at document-start on top-frame
        //    to intercept the `__buffr_media__:` console.log sentinel and
        //    forward the JSON payload via the `buffrMediaProbe` UCM handler.
        // 4. Connect `script-message-received::buffrMediaProbe` →
        //    on_media_probe_script_message, which stores `video_active`.
        //
        // run_media_probe (evals the nonce-bearing poll script) is called by the apps
        // layer on its own polling cadence (~2 s); we just handle the response.
        let media_probe_script_message_received_id: u64 = unsafe {
            use super::ffi::{
                WebKitUserContentInjectedFrames_WEBKIT_USER_CONTENT_INJECT_TOP_FRAME as INJECT_TOP,
                WebKitUserScriptInjectionTime_WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START as INJECT_START,
                webkit_user_content_manager_add_script,
                webkit_user_content_manager_register_script_message_handler,
                webkit_user_script_new, webkit_user_script_unref,
                webkit_web_view_get_user_content_manager,
            };
            let ucm = webkit_web_view_get_user_content_manager(web_view);
            if ucm.is_null() {
                0
            } else {
                // Register the native message handler name.
                let handler_name = CString::new("buffrMediaProbe").unwrap();
                let _ok = webkit_user_content_manager_register_script_message_handler(
                    ucm,
                    handler_name.as_ptr(),
                    std::ptr::null(),
                );

                // Inject media probe init script at document-start on top frame.
                let init_src = CString::new(buffr_core::scripts::MEDIA_PROBE_INIT_JS).unwrap();
                let init_script = webkit_user_script_new(
                    init_src.as_ptr(),
                    INJECT_TOP,
                    INJECT_START,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if !init_script.is_null() {
                    webkit_user_content_manager_add_script(ucm, init_script);
                    webkit_user_script_unref(init_script);
                    tracing::debug!("webkit: MEDIA_PROBE_INIT_JS injected for tab {id:?}");
                }

                // Inject console.log shim at document-start on top frame.
                let shim_src = CString::new(MEDIA_PROBE_CONSOLE_SHIM_JS).unwrap();
                let shim_script = webkit_user_script_new(
                    shim_src.as_ptr(),
                    INJECT_TOP,
                    INJECT_START,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if !shim_script.is_null() {
                    webkit_user_content_manager_add_script(ucm, shim_script);
                    webkit_user_script_unref(shim_script);
                    tracing::debug!("webkit: MEDIA_PROBE_CONSOLE_SHIM_JS injected for tab {id:?}");
                }

                // Connect `script-message-received::buffrMediaProbe`.
                // user_data is a Box<MediaProbeSignalCtx> leaked via Box::into_raw;
                // drop_media_probe_signal_ctx reconstitutes + drops it on disconnect.
                let ctx_box = Box::new(MediaProbeSignalCtx {
                    video_active: Arc::clone(&video_active),
                    tab_id: id,
                    console_nonces: console_nonces.clone(),
                });
                let ctx_raw = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
                let signal_name = CString::new("script-message-received::buffrMediaProbe").unwrap();
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
                    >(on_media_probe_script_message)),
                    ctx_raw,
                    Some(drop_media_probe_signal_ctx),
                    0,
                )
            }
        };

        // ── Edit bridge: UCM script-message handler (#134) ───────────────────
        //
        // 1. Register `buffrEdit` native handler on the UCM.
        // 2. Inject EDIT_CONSOLE_SHIM_JS at document-start on the TOP FRAME so
        //    `console.log('__buffr_edit__:…')` calls from `edit.js` are
        //    forwarded to the native handler via postMessage.
        // 3. Inject the substituted `edit.js` at document-end on the TOP FRAME
        //    so the focus/blur/mutate listeners are installed once per load.
        // 4. Connect `script-message-received::buffrEdit` →
        //    on_edit_script_message, which verifies the nonce and pushes
        //    decoded EditConsoleEvents to the shared sink.
        //
        // H5 / top-frame restriction: `edit.js` carries the page nonce, so it
        // must only ever run somewhere we trust — handing a cross-origin ad
        // iframe the nonce would hand it the forgery it exists to prevent.
        // This matches `buffr-cef`, which injects from `on_load_end` for the
        // main frame only. The accepted cost is that edit-mode events no
        // longer fire for inputs *inside* iframes (framed rich-text editors,
        // iframed login forms); the top-level document is unaffected.
        let edit_script_message_received_id: u64 = unsafe {
            use super::ffi::{
                WebKitUserContentInjectedFrames_WEBKIT_USER_CONTENT_INJECT_TOP_FRAME as INJECT_TOP,
                WebKitUserScriptInjectionTime_WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_END as INJECT_END,
                WebKitUserScriptInjectionTime_WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START as INJECT_START,
                webkit_user_content_manager_add_script,
                webkit_user_content_manager_register_script_message_handler,
                webkit_user_script_new, webkit_user_script_unref,
                webkit_web_view_get_user_content_manager,
            };
            let ucm = webkit_web_view_get_user_content_manager(web_view);
            if ucm.is_null() {
                0
            } else {
                // Register the native message handler name.
                let handler_name = CString::new("buffrEdit").unwrap();
                let _ok = webkit_user_content_manager_register_script_message_handler(
                    ucm,
                    handler_name.as_ptr(),
                    std::ptr::null(),
                );

                // Inject console.log shim at document-start on the top frame
                // so the shim is in place before edit.js fires at document-end.
                let shim_src = CString::new(EDIT_CONSOLE_SHIM_JS).unwrap();
                let shim_script = webkit_user_script_new(
                    shim_src.as_ptr(),
                    INJECT_TOP,
                    INJECT_START,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if !shim_script.is_null() {
                    webkit_user_content_manager_add_script(ucm, shim_script);
                    webkit_user_script_unref(shim_script);
                    tracing::debug!("webkit: EDIT_CONSOLE_SHIM_JS injected for tab {id:?}");
                }

                // Inject the substituted edit.js at document-end on the top
                // frame, carrying this tab's page nonce (H5).
                let edit_js = buffr_core::edit::build_inject_script(&page_nonce);
                let edit_src = CString::new(edit_js.as_str()).unwrap();
                let edit_script = webkit_user_script_new(
                    edit_src.as_ptr(),
                    INJECT_TOP,
                    INJECT_END,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if !edit_script.is_null() {
                    webkit_user_content_manager_add_script(ucm, edit_script);
                    webkit_user_script_unref(edit_script);
                    tracing::debug!("webkit: edit.js injected for tab {id:?}");
                }

                // Connect `script-message-received::buffrEdit`.
                // user_data is a Box<EditSignalCtx> leaked via Box::into_raw;
                // drop_edit_signal_ctx reconstitutes + drops it on disconnect.
                let ctx_box = Box::new(EditSignalCtx {
                    edit_sink: Arc::clone(&edit_sink),
                    tab_id: id,
                    console_nonces: console_nonces.clone(),
                });
                let ctx_raw = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
                let signal_name = CString::new("script-message-received::buffrEdit").unwrap();
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
                    >(on_edit_script_message)),
                    ctx_raw,
                    Some(drop_edit_signal_ctx),
                    0,
                )
            }
        };

        // ── Popup queue: `create` signal ──────────────────────────────────────
        //
        // Fired when JS calls `window.open(url)` or a link has `target=_blank`.
        // We extract the URL, push to popup_queue, and return NULL to suppress
        // the native popup WebView. The apps layer drains popup_queue and opens
        // the URL as a new tab.
        //
        // Signal prototype (GLib): `WebKitWebView* create(WebKitWebView*,
        //   WebKitNavigationAction*, gpointer)` — returns `WebKitWebView*`.
        // We transmute to the generic `unsafe extern "C" fn()` shape expected
        // by g_signal_connect_data, same as all other signals above.
        let create_signal_id: u64 = {
            // Leak one Arc clone of the inner Mutex into user_data.
            // GLib calls drop_popup_queue_arc on disconnect (via GClosureNotify).
            // SAFETY: Arc::into_raw produces a valid pointer; drop_popup_queue_arc
            // will reconstitute + drop it exactly once on signal disconnect.
            let queue_raw = Arc::into_raw(Arc::clone(&popup_queue)) as *mut std::os::raw::c_void;
            let signal_name = CString::new("create").unwrap();
            unsafe {
                g_signal_connect_data(
                    web_view as *mut _,
                    signal_name.as_ptr(),
                    Some(std::mem::transmute::<
                        unsafe extern "C" fn(
                            *mut WebKitWebView,
                            *mut std::os::raw::c_void,
                            *mut std::os::raw::c_void,
                        ) -> *mut WebKitWebView,
                        unsafe extern "C" fn(),
                    >(on_create)),
                    queue_raw,
                    Some(drop_popup_queue_arc),
                    0,
                )
            }
        };

        // ── Context-menu signal (#121) ────────────────────────────────────────
        //
        // Fires on right-click. We suppress the native menu and push a neutral
        // ContextMenuRequest into the shared sink for the apps layer to render
        // as ContextMenuOverlay.
        // user_data is a Box<ContextSignalCtx> (contains the sink Arc).
        let context_menu_id: u64 = {
            let ctx_box = Box::new(ContextSignalCtx {
                context_menu_sink: Arc::clone(&context_menu_sink),
            });
            let ctx_raw = Box::into_raw(ctx_box) as *mut std::os::raw::c_void;
            let signal_c = CString::new("context-menu").unwrap();
            unsafe {
                g_signal_connect_data(
                    web_view as *mut _,
                    signal_c.as_ptr(),
                    Some(std::mem::transmute::<
                        unsafe extern "C" fn(
                            *mut WebKitWebView,
                            *mut WebKitContextMenu,
                            *mut WebKitHitTestResult,
                            *mut std::os::raw::c_void,
                        ) -> gboolean,
                        unsafe extern "C" fn(),
                    >(on_context_menu)),
                    ctx_raw,
                    Some(drop_context_signal_ctx),
                    0,
                )
            }
        };

        // ── TLS certificate error signal (#120) ───────────────────────────────
        //
        // Fires when a navigation hits a TLS error. We auto-allow + reload
        // in-session. user_data is a per-connection Arc<TabSignalCtx> clone
        // via the shared `connect` closure.
        let tls_error_id = connect("load-failed-with-tls-errors", unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    *const std::os::raw::c_char,
                    *mut GTlsCertificate,
                    GTlsCertificateFlags,
                    *mut std::os::raw::c_void,
                ) -> gboolean,
                unsafe extern "C" fn(),
            >(on_load_failed_with_tls_errors)
        });

        // ── Navigation + permission policy signals (#123) ─────────────────────
        //
        // decide-policy: external schemes → xdg-open + ignore; internal → use.
        // permission-request: auto-deny with tracing::warn.
        // Both use NULL user_data and no GClosureNotify (no heap allocation).
        let decide_policy_id: u64 = {
            let signal_c = CString::new("decide-policy").unwrap();
            unsafe {
                g_signal_connect_data(
                    web_view as *mut _,
                    signal_c.as_ptr(),
                    Some(std::mem::transmute::<
                        unsafe extern "C" fn(
                            *mut WebKitWebView,
                            *mut WebKitPolicyDecision,
                            WebKitPolicyDecisionType,
                            *mut std::os::raw::c_void,
                        ),
                        unsafe extern "C" fn(),
                    >(on_decide_policy)),
                    std::ptr::null_mut(),
                    None,
                    0,
                )
            }
        };

        let permission_request_id: u64 = {
            // Pass a per-connection Arc<TabSignalCtx> clone as user_data so
            // the signal handler can access the queue + pending map.
            // drop_tab_signal_ctx handles the Arc decrement on disconnect.
            let arc_clone = Arc::clone(&ctx);
            let user_data = Arc::into_raw(arc_clone) as *mut std::os::raw::c_void;
            let signal_c = CString::new("permission-request").unwrap();
            unsafe {
                g_signal_connect_data(
                    web_view as *mut _,
                    signal_c.as_ptr(),
                    Some(std::mem::transmute::<
                        unsafe extern "C" fn(
                            *mut WebKitWebView,
                            *mut WebKitPermissionRequest,
                            *mut std::os::raw::c_void,
                        ) -> gboolean,
                        unsafe extern "C" fn(),
                    >(on_permission_request)),
                    user_data,
                    Some(drop_tab_signal_ctx),
                    0,
                )
            }
        };

        // ── IME: create BuffrInputMethodContext and attach to WebView ─────────
        //
        // Each tab gets its own IME context so that Rust-side winit IME events
        // (preedit-started, preedit-changed, committed) are forwarded to the
        // focused editable in this tab's page.  The context is owned here
        // (ref=1 from g_object_new); Drop releases it via g_object_unref.
        let ime_ctx: *mut WebKitInputMethodContext = unsafe {
            let ctx = buffr_input_method_context_new();
            if !ctx.is_null() {
                webkit_web_view_set_input_method_context(web_view, ctx);
                tracing::debug!("webkit: IME context attached to WebView id={id:?}");
            } else {
                tracing::warn!(
                    "webkit: buffr_input_method_context_new returned NULL — IME unavailable for id={id:?}"
                );
            }
            ctx
        };

        let url_c = CString::new(url).unwrap_or_default();
        unsafe { webkit_web_view_load_uri(web_view, url_c.as_ptr()) };
        tracing::info!("webkit: created WebView id={id:?} url={url}");

        Some(TabEntry {
            id,
            web_view,
            wpe_view,
            ime_ctx,
            load_changed_id,
            notify_title_id,
            notify_uri_id,
            notify_estimated_load_progress_id,
            hint_script_message_received_id,
            clipboard_script_message_received_id,
            favicon_script_message_received_id,
            create_signal_id,
            tls_error_id,
            context_menu_id,
            decide_policy_id,
            permission_request_id,
            audio_script_message_received_id,
            audio_event_queue,
            audio_browser_id: id.0 as i32,
            cursor_script_message_received_id,
            media_probe_script_message_received_id,
            edit_script_message_received_id,
            video_active,
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
            tracing::warn!(target: "buffr::resize_path", "TabEntry::resize skipped: wpe_view null");
            return;
        }
        unsafe {
            let tl = wpe_view_get_toplevel(self.wpe_view);
            if tl.is_null() {
                tracing::warn!(target: "buffr::resize_path", "TabEntry::resize: toplevel null");
                return;
            }
            tracing::info!(
                target: "buffr::resize_path",
                width, height,
                "TabEntry::resize: wpe_toplevel_resize"
            );
            wpe_toplevel_resize(tl, width as i32, height as i32);
        }
    }

    /// Kick this tab's view to drop any cached paint and emit a fresh
    /// frame at `(width, height)`.  Same h-1 → h wiggle TabEntry::show
    /// uses on activation — WPE 2.52 no-ops a same-dim resize, so the
    /// 1-pixel intermediate forces the AcceleratedBackingStore to
    /// reflow + re-render and yields a fresh `render_buffer` callback
    /// on the way back to the original dim.
    pub(crate) fn force_repaint(&self, width: u32, height: u32) {
        if self.wpe_view.is_null() {
            return;
        }
        let wiggle_h = (height.saturating_sub(1)).max(1);
        // SAFETY: wpe_view is a live WPEView owned by the WebView;
        // calls are thread-bound to the GLib worker thread.
        unsafe {
            wpe_view_resized(self.wpe_view, width as i32, wiggle_h as i32);
            wpe_view_resized(self.wpe_view, width as i32, height as i32);
        }
        tracing::debug!(
            target: "buffr::resize_path",
            width,
            height,
            "TabEntry::force_repaint: resize-wiggle kick"
        );
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
            // Disconnect UCM signals (hint, clipboard) before unref so the
            // GClosureNotify (drop_hint_signal_ctx, drop_clipboard_box) fires while
            // the UCM is still alive, not after it's freed by g_object_unref.
            let ucm = super::ffi::webkit_web_view_get_user_content_manager(self.web_view);
            if !ucm.is_null() {
                if self.hint_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.hint_script_message_received_id,
                    );
                }
                if self.clipboard_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.clipboard_script_message_received_id,
                    );
                }
                if self.favicon_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.favicon_script_message_received_id,
                    );
                }
                if self.audio_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.audio_script_message_received_id,
                    );
                }
                if self.cursor_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.cursor_script_message_received_id,
                    );
                }
                if self.media_probe_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.media_probe_script_message_received_id,
                    );
                }
                if self.edit_script_message_received_id != 0 {
                    g_signal_handler_disconnect(
                        ucm as *mut _,
                        self.edit_script_message_received_id,
                    );
                }
            }
            // Clear video_active on tab close so the aggregate flag doesn't
            // stay stuck true after the tab that owned it is gone.
            self.video_active
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // Push a final active=false event so the apps layer clears any
            // audio indicator immediately on tab close rather than waiting for
            // the next JS-fired event (which will never come from a closed tab).
            if let Ok(mut q) = self.audio_event_queue.lock() {
                q.push_back(EngineAudioEvent {
                    browser_id: self.audio_browser_id,
                    active: false,
                });
            }
            // Disconnect `create` signal on the WebView before unref.
            if self.create_signal_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.create_signal_id);
            }
            // Disconnect TLS error signal (#120).
            if self.tls_error_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.tls_error_id);
            }
            // Disconnect context-menu signal (#121).
            if self.context_menu_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.context_menu_id);
            }
            // Disconnect policy signals (#123).
            if self.decide_policy_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.decide_policy_id);
            }
            if self.permission_request_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.permission_request_id);
            }
            // Release the IME context ref we own. webkit_web_view_set_input_method_context
            // held a ref on our behalf; dropping our own ref here lets GLib collect it
            // after the WebView's ref is released below.
            if !self.ime_ctx.is_null() {
                g_object_unref(self.ime_ctx as *mut _);
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
    /// EGL worker. Held for the runtime's lifetime so the EGLDisplay /
    /// EGLContext survive across tabs; not directly read after construction.
    #[allow(dead_code)]
    pub egl: EglWorker,
    /// Shared display. Lives for the runtime's lifetime; each WebView
    /// bumps its ref via the `display` construct property.
    display: WpeDisplayKind,
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
    /// Shared popup URL queue. Written by the `create` signal handler on
    /// each WebView when JS calls `window.open(url)` or a link has
    /// `target=_blank`. Drained by the apps layer via `BrowserEngine::popup_queue`.
    pub popup_queue: PopupQueue,
    /// Context-menu request sink. Written by the `context-menu` signal handler
    /// on each WebView; drained by the apps layer via `drain_context_menu_requests`.
    pub context_menu_sink: WpeContextMenuSink,
    /// Favicon decode sink. Written by the per-tab `buffrFavicon` UCM signal
    /// handler (background thread) after fetch + decode; drained by
    /// `WebKitEngine::drain_favicon_updates` on any thread.
    pub favicon_sink: buffr_core::favicon::FaviconSink,
    /// Runtime-wide nav-state atomics. Written by `on_load_changed` for the
    /// ACTIVE tab on COMMITTED / FINISHED; read lock-free from the UI thread
    /// by `WebKitEngine::can_go_back` / `can_go_forward`.
    pub can_go_back: Arc<std::sync::atomic::AtomicBool>,
    pub can_go_forward: Arc<std::sync::atomic::AtomicBool>,
    /// Shared permissions queue — pushed from `on_permission_request` on the
    /// GLib worker thread; drained by `WebKitEngine::permissions_queue` on any
    /// thread.
    pub permissions_queue: PermissionsQueue,
    /// Map resolve_id → g_object_ref'd WebKitPermissionRequest ptr. Written
    /// by the GLib worker's signal handler; consumed by the worker's
    /// `Command::ResolvePermission` handler.
    pub pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>>,
    /// Monotonic id counter for resolve_ids.
    pub permission_next_id: Arc<AtomicU64>,
    /// Shared audio-event queue. Written by per-tab `buffrAudio` UCM signal
    /// handlers; drained by `WebKitEngine::drain_audio_events`.
    pub audio_event_queue: WpeAudioEventQueue,
    /// Shared cursor state (#137). Written by per-tab `buffrCursor` UCM signal
    /// handlers; read by `WebKitEngine::take_cursor_change`.
    pub cursor_state: SharedCursorState,
    /// Runtime-wide video-active flag (#135). Written by per-tab
    /// `buffrMediaProbe` UCM signal handlers; read by
    /// `WebKitEngine::any_video_active`.
    pub video_active: Arc<std::sync::atomic::AtomicBool>,
    /// Edit-mode event sink (#134). Shared with `WebKitEngine::edit_sink` and
    /// per-tab `buffrEdit` UCM handlers. Inner `Option` is populated by
    /// `WebKitEngine::set_edit_sink` post-construction.
    pub edit_sink: Arc<Mutex<Option<buffr_core::edit::EditEventSink>>>,
    /// Per-tab console-IPC nonce table (H5). Same handle as
    /// `WorkerHandle::console_nonces`; cloned into every UCM signal ctx so
    /// inbound lines are verified against the nonce currently minted for the
    /// emitting tab.
    pub console_nonces: buffr_core::ConsoleNonces,
}

impl WpeRuntime {
    /// Every argument is a distinct shared sink/atomic wired straight through
    /// from `worker::spawn` (which carries the same allow). Bundling them into
    /// a params struct would just move the list one level out.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        egl: EglWorker,
        is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
        zoom_level: Arc<Mutex<f64>>,
        hint_sink: HintEventSink,
        popup_queue: PopupQueue,
        context_menu_sink: WpeContextMenuSink,
        favicon_sink: buffr_core::favicon::FaviconSink,
        can_go_back: Arc<std::sync::atomic::AtomicBool>,
        can_go_forward: Arc<std::sync::atomic::AtomicBool>,
        permissions_queue: PermissionsQueue,
        pending_permissions: Arc<Mutex<HashMap<String, WpePermissionRequestPtr>>>,
        permission_next_id: Arc<AtomicU64>,
        audio_event_queue: WpeAudioEventQueue,
        cursor_state: SharedCursorState,
        video_active: Arc<std::sync::atomic::AtomicBool>,
        edit_sink: Arc<Mutex<Option<buffr_core::edit::EditEventSink>>>,
        console_nonces: buffr_core::ConsoleNonces,
        prefer_native: bool,
        wayland_handles: Option<buffr_engine::WaylandNativeHandles>,
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

        // ── Display construction: OSR / BuffrWayland / stock Wayland (#144, #152)
        //
        // Priority when `prefer_native=true`:
        //
        //   1. BuffrDisplayWayland (#152) — preferred: reuses the host
        //      wl_display, solves the cross-client wl_subsurface problem.
        //      Requires all four Wayland handle pointers to be non-null.
        //      Skipped (→ 2) when `BUFFR_WEBKIT_STOCK_WAYLAND=1` or when
        //      any handle is null.
        //
        //   2. Stock WPEDisplayWayland (#144) — env-gated fallback:
        //      `BUFFR_WEBKIT_STOCK_WAYLAND=1`.  Opens its own wl_display.
        //      Rendered content is a sibling, not a child subsurface.
        //
        //   3. BuffrDisplay (OSR pixel-copy) — headless / CI fallback, or
        //      when `prefer_native=false`.
        //
        // The session-type env-var check was removed: buffr enforces Wayland
        // at startup on Linux, so the session is guaranteed Wayland here.
        let want_wayland = prefer_native;

        // Helper: construct + connect + set-primary an OSR BuffrDisplay.
        let make_osr = |egl_display: *mut std::ffi::c_void| -> Result<WpeDisplayKind, String> {
            let handle = BuffrDisplayHandle::new(egl_display, width, height, 1.0, hz)
                .ok_or_else(|| "BuffrDisplayHandle::new returned None".to_string())?;
            unsafe {
                let mut error: *mut GError = std::ptr::null_mut();
                let ok = wpe_display_connect(handle.raw, &mut error);
                if ok == 0 {
                    return Err("wpe_display_connect failed (OSR)".into());
                }
                wpe_display_set_primary(handle.raw);
            }
            tracing::info!("webkit: BuffrDisplay created + connected (OSR)");
            Ok(WpeDisplayKind::Osr(handle))
        };

        let display: WpeDisplayKind = if want_wayland {
            let use_stock = std::env::var("BUFFR_WEBKIT_STOCK_WAYLAND")
                .map(|v| v == "1")
                .unwrap_or(false);

            // ── Path 1: BuffrDisplayWayland (preferred, #152) ─────────────
            let handles_complete = wayland_handles.as_ref().is_some_and(|h| {
                !h.wl_display.is_null()
                    && !h.wl_compositor.is_null()
                    && !h.wl_subcompositor.is_null()
                    && !h.parent_wl_surface.is_null()
            });

            if handles_complete && !use_stock {
                let h = wayland_handles.unwrap();
                let raw = unsafe {
                    buffr_display_wayland_new(
                        h.wl_display,
                        h.wl_compositor,
                        h.wl_subcompositor,
                        h.parent_wl_surface,
                        width as i32,
                        height as i32,
                        1.0_f64,
                        hz as i32,
                    )
                };
                if !raw.is_null() {
                    let connect_ok = unsafe {
                        let mut error: *mut GError = std::ptr::null_mut();
                        // connect vmethod is a no-op — just updates WPEDisplay's
                        // internal "connected" flag so WebKit considers it ready.
                        let ok = wpe_display_connect(raw, &mut error);
                        if ok == 0 {
                            tracing::warn!(
                                "webkit: wpe_display_connect on BuffrDisplayWayland failed; \
                                 falling back to OSR"
                            );
                            g_object_unref(raw as *mut _);
                        } else {
                            wpe_display_set_primary(raw);
                        }
                        ok
                    };
                    if connect_ok != 0 {
                        tracing::info!(
                            "webkit: BuffrDisplayWayland constructed + set as primary (#152)"
                        );
                        WpeDisplayKind::BuffrWayland(BuffrDisplayWaylandHandle { raw })
                    } else {
                        make_osr(egl.raw_display())?
                    }
                } else {
                    tracing::warn!(
                        "webkit: buffr_display_wayland_new returned NULL \
                         (eglInitialize likely failed); falling back to OSR"
                    );
                    make_osr(egl.raw_display())?
                }
            } else {
                if use_stock {
                    tracing::info!(
                        "webkit: BUFFR_WEBKIT_STOCK_WAYLAND=1 → using stock WPEDisplayWayland"
                    );
                } else {
                    tracing::warn!(
                        "webkit: Wayland handles incomplete or missing; \
                         falling back to stock WPEDisplayWayland"
                    );
                }

                // ── Path 2: stock WPEDisplayWayland (env-gated / incomplete handles)
                // SAFETY: wpe_display_wayland_new returns a floating GObject ref.
                let d = unsafe { wpe_display_wayland_new() };
                if d.is_null() {
                    tracing::warn!(
                        "webkit: wpe_display_wayland_new returned NULL; \
                         falling back to BuffrDisplay (OSR)"
                    );
                    make_osr(egl.raw_display())?
                } else {
                    let mut error: *mut GError = std::ptr::null_mut();
                    let ok = unsafe {
                        wpe_display_wayland_connect(
                            d as *mut WPEDisplayWayland,
                            std::ptr::null(),
                            &mut error,
                        )
                    };
                    if ok == 0 {
                        if !error.is_null() {
                            unsafe { g_error_free(error) };
                        }
                        tracing::warn!(
                            "webkit: wpe_display_wayland_connect failed; \
                             falling back to BuffrDisplay (OSR)"
                        );
                        unsafe { g_object_unref(d as *mut _) };
                        make_osr(egl.raw_display())?
                    } else {
                        unsafe { wpe_display_set_primary(d) };
                        tracing::info!(
                            "webkit: stock WPEDisplayWayland connected + set as primary (#144)"
                        );
                        WpeDisplayKind::Wayland(d)
                    }
                }
            }
        } else {
            make_osr(egl.raw_display())?
        };

        // ── buffr-clipboard URI scheme (#128) ─────────────────────────────────
        //
        // Register `buffr-clipboard:read` once on the default WebContext so
        // buffr-internal pages can call `fetch('buffr-clipboard:read')` to
        // read the host clipboard. The callback
        // (`on_clipboard_paste_scheme_request`) spawns a worker thread to
        // avoid blocking the GLib main loop on the hjkl-clipboard Wayland
        // roundtrip, then delivers the result via g_idle_add.
        //
        // Security: the scheme must remain CORS-enabled + secure so buffr's
        // own internal pages can fetch it without preflight or mixed-content
        // blocking. The handler itself gates on the requesting page being a
        // buffr:// internal page (W2): any other origin's fetch is denied
        // with an empty body, so a cross-origin or iframe page cannot read
        // the host clipboard.
        unsafe {
            use super::ffi::{
                webkit_security_manager_register_uri_scheme_as_cors_enabled,
                webkit_security_manager_register_uri_scheme_as_secure,
                webkit_web_context_get_default, webkit_web_context_get_security_manager,
                webkit_web_context_register_uri_scheme,
            };
            let ctx = webkit_web_context_get_default();
            if !ctx.is_null() {
                let scheme_c = std::ffi::CString::new("buffr-clipboard").unwrap();
                webkit_web_context_register_uri_scheme(
                    ctx,
                    scheme_c.as_ptr(),
                    Some(on_clipboard_paste_scheme_request),
                    std::ptr::null_mut(),
                    None,
                );
                let sec = webkit_web_context_get_security_manager(ctx);
                if !sec.is_null() {
                    webkit_security_manager_register_uri_scheme_as_cors_enabled(
                        sec,
                        scheme_c.as_ptr(),
                    );
                    webkit_security_manager_register_uri_scheme_as_secure(sec, scheme_c.as_ptr());
                }
                tracing::info!(
                    "webkit: buffr-clipboard URI scheme registered (clipboard paste inbound)"
                );
            } else {
                tracing::warn!(
                    "webkit: webkit_web_context_get_default() returned NULL \
                     — buffr-clipboard paste scheme not registered"
                );
            }
        }

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
            popup_queue,
            favicon_sink,
            context_menu_sink,
            can_go_back,
            can_go_forward,
            permissions_queue,
            pending_permissions,
            permission_next_id,
            audio_event_queue,
            cursor_state,
            video_active,
            edit_sink,
            console_nonces,
        })
    }

    /// The currently active tab, if any.
    fn active_tab(&self) -> Option<&TabEntry> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    /// Whether the runtime's chosen display backend is a native
    /// compositing surface (`BuffrDisplayWayland` or stock
    /// `WPEDisplayWayland`) rather than the OSR readback path.
    /// Published to the apps layer via `BrowserEngine::is_using_native_compositing`.
    pub(crate) fn display_is_native(&self) -> bool {
        self.display.is_native()
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
        if !background && let Some(prev) = self.active_tab() {
            prev.is_active
                .store(false, std::sync::atomic::Ordering::SeqCst);
            prev.hide();
        }

        // TabInfo must exist before signal handlers fire — load-changed
        // can race the return from TabEntry::new because webkit_web_view_load_uri
        // emits LOAD_STARTED synchronously. Push it now; roll back on failure.
        let pushed_es_idx = {
            let mut st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex poison (push): {e}"))?;
            let info = TabInfo::new(id, url, st.private);
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
            self.display.raw_display(),
            self.display.is_native(),
            Arc::clone(&self.frame),
            Arc::clone(&self.view),
            Arc::clone(&self.engine_state),
            Arc::clone(&self.is_loading_atomic),
            Arc::clone(&is_active),
            Arc::clone(&self.hint_sink),
            Arc::clone(&self.popup_queue),
            Arc::clone(&self.context_menu_sink),
            Arc::clone(&self.favicon_sink),
            Arc::clone(&self.can_go_back),
            Arc::clone(&self.can_go_forward),
            Arc::clone(&self.permissions_queue),
            Arc::clone(&self.pending_permissions),
            Arc::clone(&self.permission_next_id),
            Arc::clone(&self.audio_event_queue),
            Arc::clone(&self.cursor_state),
            Arc::clone(&self.video_active),
            Arc::clone(&self.edit_sink),
            self.console_nonces.clone(),
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
                if !background
                    && let Some(idx) = prev_active_idx
                    && let Some(prev) = self.tabs.get(idx)
                {
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
        // Refresh can_go_back / can_go_forward atomics synchronously so the
        // UI thread reads nav state for the newly-active tab immediately.
        let new_tab = &self.tabs[new_idx];
        if !new_tab.web_view.is_null() {
            use std::sync::atomic::Ordering;
            // SAFETY: web_view is valid for the tab's lifetime; we're on the
            // GLib worker thread which is the only thread that can close tabs.
            let back = unsafe { webkit_web_view_can_go_back(new_tab.web_view) != 0 };
            let fwd = unsafe { webkit_web_view_can_go_forward(new_tab.web_view) != 0 };
            self.can_go_back.store(back, Ordering::Relaxed);
            self.can_go_forward.store(fwd, Ordering::Relaxed);
        }
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
        if let Some(ref mut active) = self.active_idx
            && idx < *active
        {
            *active -= 1;
        }
        // Mirror removal into engine_state.
        if let Ok(mut st) = self.engine_state.lock() {
            if idx < st.tabs.len() {
                st.tabs.remove(idx);
            }
            // Adjust engine_state.active_idx by the same rule.
            if let Some(ref mut a) = st.active_idx
                && idx < *a
            {
                *a -= 1;
            }
        }
        tracing::info!(?id, idx, "webkit: close_tab (background tab)");
        true
    }

    pub(crate) fn navigate(&mut self, url: &str) {
        // Capture dims before borrowing the tab — `force_repaint` needs
        // them and we can't hold a self-borrow across multiple paths.
        let (width, height) = if let Ok(st) = self.engine_state.lock() {
            (st.width, st.height)
        } else {
            (0, 0)
        };
        if let Some(tab) = self.active_tab() {
            tab.load_uri(url);
            // Same-tab navigations need the resize-wiggle + needs_fresh
            // trick that open_tab applies to foreground tab creations.
            // Two failure modes without this:
            //
            //   1. WebKit's render scheduler can decide the new
            //      navigation produces "no significant visual change
            //      yet" and skip emitting render buffers — e.g. when
            //      navigating off an error page (about:blank-ish) to
            //      a fresh URL whose first paint is also white.  The
            //      previous page's pixels stay in the shared OsrFrame
            //      forever, producing the symptom reported in
            //      production: chrome metadata (title, favicon, URL)
            //      updated correctly to the new page but the viewport
            //      showed the old page's content.
            //   2. The app-layer freshness gate is content-only
            //      (generation + dims, no per-URL tag), so without
            //      a needs_fresh flip the stale frame from the old
            //      page never gets rejected on read.
            //
            // The wiggle forces WebKit's AcceleratedBackingStore to
            // recomposite and emit a fresh buffer regardless of its
            // own emission heuristics.  Bounds check needed: width=0
            // means the runtime hasn't seen a resize yet (early
            // startup); skip the wiggle in that case — the natural
            // load-driven paint sequence covers it.
            if width > 0 && height > 0 {
                tab.force_repaint(width, height);
            }
        }
        if let Ok(mut frame) = self.frame.lock() {
            frame.needs_fresh = true;
        }
        if let Ok(mut st) = self.engine_state.lock()
            && let Some(idx) = st.active_idx
            && let Some(tab_info) = st.tabs.get_mut(idx)
        {
            tab_info.url = url.to_owned();
            tab_info.is_loading = true;
        }
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        tracing::info!(
            target: "buffr::resize_path",
            width, height,
            tabs = self.tabs.len(),
            "WpeRuntime::resize: dispatching wpe_toplevel_resize to all tabs"
        );
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

    /// Fire the media probe poll script on the active tab (#135).
    ///
    /// Evaluates the media-probe poll script, which recomputes
    /// `__buffr_media_active` / `__buffr_video_active` from five signal sources
    /// and, on any state transition, emits
    /// `console.log('__buffr_media__:' + nonce + ':' + JSON)`. The shim
    /// installed by `MEDIA_PROBE_CONSOLE_SHIM_JS` forwards the whole line to
    /// the `buffrMediaProbe` UCM handler, which verifies the nonce and updates
    /// the runtime-wide `video_active` atomic.
    ///
    /// H5: the script is built with the *active tab's* page nonce, the same one
    /// `TabEntry::new` minted, and `eval_js` runs it in the main frame only.
    pub(crate) fn run_media_probe(&self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let nonce = self.console_nonces.page(tab.id.0 as i32);
        self.eval_js(&buffr_core::media_probe::build_poll_script(&nonce));
    }

    /// Push a preedit update to the active tab's `BuffrInputMethodContext`.
    ///
    /// `cursor` is `(start_byte, end_byte)` within `text`, or `None` to
    /// collapse the cursor to the end of the text. Only the end byte is used
    /// (WebKit's get_preedit API exposes a single cursor_offset, not a range).
    pub(crate) fn ime_set_composition(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.ime_ctx.is_null() {
            return;
        }
        let cursor_byte = cursor
            .map(|(_, e)| e as i32)
            .unwrap_or_else(|| text.len() as i32);
        let text_c = match std::ffi::CString::new(text) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("webkit: ime_set_composition: text contains NUL byte");
                return;
            }
        };
        let ctx = tab.ime_ctx;
        // SAFETY: ctx is a live BuffrInputMethodContext owned by TabEntry;
        // this call is on the GLib worker thread.
        unsafe { buffr_input_method_context_set_preedit(ctx, text_c.as_ptr(), cursor_byte) };
    }

    /// Commit text to the focused editable in the active tab.
    pub(crate) fn ime_commit(&mut self, text: &str) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.ime_ctx.is_null() {
            return;
        }
        let text_c = match std::ffi::CString::new(text) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("webkit: ime_commit: text contains NUL byte");
                return;
            }
        };
        let ctx = tab.ime_ctx;
        // SAFETY: ctx is live; call is on the GLib worker thread.
        unsafe { buffr_input_method_context_commit(ctx, text_c.as_ptr()) };
    }

    /// Cancel the in-progress IME composition on the active tab.
    pub(crate) fn ime_cancel(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.ime_ctx.is_null() {
            return;
        }
        let ctx = tab.ime_ctx;
        // SAFETY: ctx is live; call is on the GLib worker thread.
        unsafe { buffr_input_method_context_cancel(ctx) };
    }

    /// Force the active tab's view to repaint at its current dims.
    /// Same h-1 → h wiggle TabEntry::show uses to kick a fresh frame
    /// after tab activation; applied here so the apps-layer
    /// resize-paint watchdog can retire its retry loop quickly when
    /// WPE coalesced the original resize.
    pub(crate) fn force_repaint_active(&self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let (width, height) = if let Ok(st) = self.engine_state.lock() {
            (st.width, st.height)
        } else {
            return;
        };
        tab.force_repaint(width, height);
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

    /// Execute a named editing command (`"Undo"`, `"Cut"`, `"Copy"`, etc.)
    /// on the active tab's focused frame.
    ///
    /// `webkit_web_view_execute_editing_command` is fire-and-forget: it
    /// delivers the command to the WebProcess and returns immediately.
    pub(crate) fn execute_editing_command(&self, name: &str) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.web_view.is_null() {
            return;
        }
        let Ok(cmd_c) = CString::new(name) else {
            tracing::warn!(name, "webkit: execute_editing_command: NUL in command name");
            return;
        };
        // SAFETY: web_view is valid for the tab's lifetime; cmd_c is
        // null-terminated and lives until the end of this call.
        unsafe {
            webkit_web_view_execute_editing_command(tab.web_view, cmd_c.as_ptr());
        }
        tracing::debug!(name, "webkit: execute_editing_command");
    }

    /// Trigger a download of `url` on the active WebView.
    ///
    /// Calls `webkit_web_view_download_uri`; the returned `*mut WebKitDownload`
    /// is discarded — the process-wide `download-started` signal on
    /// `WebKitNetworkSession` (wired in `worker::spawn`) picks it up and routes
    /// lifecycle events through the buffr-downloads pipeline.
    pub(crate) fn start_download(&self, url: &str) {
        let Some(tab) = self.active_tab() else {
            tracing::warn!(url, "webkit: start_download: no active tab");
            return;
        };
        if tab.web_view.is_null() {
            tracing::warn!(url, "webkit: start_download: active tab has no WebView");
            return;
        }
        let Ok(url_c) = CString::new(url) else {
            tracing::warn!(
                url,
                "webkit: start_download: URL contains NUL byte — dropping"
            );
            return;
        };
        // SAFETY: web_view is valid for the tab's lifetime; url_c is
        // null-terminated and lives until the end of this call.
        // The returned *mut WebKitDownload is a floating ref that WebKit
        // immediately sinks into its internal download list — we discard it.
        unsafe {
            webkit_web_view_download_uri(tab.web_view, url_c.as_ptr());
        }
        tracing::debug!(url, "webkit: start_download dispatched");
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

    /// Resolve a pending permission request identified by `resolve_id`.
    ///
    /// Called from the GLib worker's command handler when
    /// `Command::ResolvePermission` arrives. Looks up the stored
    /// `WebKitPermissionRequest*`, fires allow or deny, and drops our
    /// g_object_ref via g_object_unref.
    ///
    /// `remember` flag: persistence is out of scope for this PR — both
    /// `Allow { remember: true }` and `Allow { remember: false }` call
    /// `webkit_permission_request_allow` (allow once). Both `Deny` variants
    /// and `Defer` call deny. A follow-up issue should add per-origin storage.
    pub(crate) fn resolve_permission(&self, resolve_id: &str, outcome: PromptOutcome) {
        use super::ffi::{webkit_permission_request_allow, webkit_permission_request_deny};

        let ptr = match self.pending_permissions.lock() {
            Ok(mut map) => map.remove(resolve_id),
            Err(_) => {
                tracing::warn!(
                    resolve_id,
                    "webkit: resolve_permission — pending_permissions mutex poisoned"
                );
                return;
            }
        };

        let Some(WpePermissionRequestPtr(raw)) = ptr else {
            tracing::debug!(
                resolve_id,
                "webkit: resolve_permission — resolve_id not found (already resolved?)"
            );
            return;
        };

        if raw.is_null() {
            return;
        }

        // SAFETY: raw was g_object_ref'd in on_permission_request and kept
        // alive in pending_permissions. We are on the GLib worker thread.
        // After allow/deny WebKit has completed its internal handling; we then
        // release our ref via g_object_unref.
        unsafe {
            match outcome {
                PromptOutcome::Allow { .. } => {
                    tracing::debug!(resolve_id, "webkit: permission allowed");
                    webkit_permission_request_allow(raw);
                }
                PromptOutcome::Deny { .. } | PromptOutcome::Defer => {
                    tracing::debug!(resolve_id, ?outcome, "webkit: permission denied/deferred");
                    webkit_permission_request_deny(raw);
                }
            }
            g_object_unref(raw as *mut _);
        }
    }
}

impl Drop for WpeRuntime {
    fn drop(&mut self) {
        // Drain any unresolved permission requests — deny them all and release
        // the g_object_ref we took in on_permission_request. Without this,
        // WebKit's network/media process holds the request pending forever.
        use super::ffi::webkit_permission_request_deny;
        let pending: Vec<(String, WpePermissionRequestPtr)> = self
            .pending_permissions
            .lock()
            .map(|mut map| map.drain().collect())
            .unwrap_or_default();
        for (id, WpePermissionRequestPtr(raw)) in pending {
            if !raw.is_null() {
                tracing::debug!(
                    id,
                    "webkit: WpeRuntime drop — deny + unref pending permission"
                );
                // SAFETY: raw is still valid; we hold the only remaining ref.
                unsafe {
                    webkit_permission_request_deny(raw);
                    g_object_unref(raw as *mut _);
                }
            }
        }
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{EDIT_CONSOLE_SHIM_JS, HINT_CONSOLE_BRIDGE_JS, MEDIA_PROBE_CONSOLE_SHIM_JS};
    use buffr_core::console_nonce::CONSOLE_NONCE_SEPARATOR;
    use buffr_core::edit::EDIT_CONSOLE_SENTINEL;
    use buffr_core::hint::HINT_CONSOLE_SENTINEL;
    use buffr_core::media_probe::MEDIA_PROBE_SENTINEL;
    use buffr_core::{CONSOLE_NONCE_LEN, ConsoleNonces, new_console_nonce};

    /// Build the wire line an injected script emits: `<sentinel><nonce>:<json>`.
    fn wire(sentinel: &str, nonce: &str, json: &str) -> String {
        format!("{sentinel}{nonce}{CONSOLE_NONCE_SEPARATOR}{json}")
    }

    // ── JS bridge ↔ buffr-core sentinel drift guards (H5) ────────────────────

    /// Each console bridge hardcodes its sentinel as a JS string literal. If
    /// `buffr-core` ever renames one, the bridge silently stops matching and
    /// hint / edit / media-probe die with no error anywhere. Pin them.
    #[test]
    fn bridges_hardcode_the_current_sentinels() {
        for (name, src, sentinel) in [
            ("buffrHint", HINT_CONSOLE_BRIDGE_JS, HINT_CONSOLE_SENTINEL),
            ("buffrEdit", EDIT_CONSOLE_SHIM_JS, EDIT_CONSOLE_SENTINEL),
            (
                "buffrMediaProbe",
                MEDIA_PROBE_CONSOLE_SHIM_JS,
                MEDIA_PROBE_SENTINEL,
            ),
        ] {
            assert!(
                src.contains(&format!("'{sentinel}'")),
                "{name} bridge JS must match on the live sentinel {sentinel:?}"
            );
        }
    }

    /// The bridges must match on the BARE sentinel, never on a
    /// sentinel+nonce prefix. They are injected once per tab and re-run by
    /// WebKit on every document load; baking a nonce into them would mean
    /// re-injecting the bridge on every rotation, and would move the
    /// authentication check into page-controlled JS. See the comment on
    /// `HINT_CONSOLE_BRIDGE_JS`.
    #[test]
    fn bridges_do_not_bake_in_a_nonce() {
        for (name, src) in [
            ("buffrHint", HINT_CONSOLE_BRIDGE_JS),
            ("buffrEdit", EDIT_CONSOLE_SHIM_JS),
            ("buffrMediaProbe", MEDIA_PROBE_CONSOLE_SHIM_JS),
        ] {
            assert!(
                !src.contains("%%SENTINEL%%") && !src.contains("{nonce}"),
                "{name} bridge JS must not carry a nonce placeholder"
            );
        }
    }

    /// The bridges must forward the WHOLE line. Stripping the prefix in JS
    /// (as the edit / media-probe shims used to) throws the nonce away before
    /// Rust ever sees it, leaving `parse_payload` with nothing to verify.
    #[test]
    fn bridges_forward_the_whole_line() {
        assert!(
            HINT_CONSOLE_BRIDGE_JS.contains("postMessage(msg)"),
            "hint bridge must forward the unmodified line"
        );
        for (name, src, handler) in [
            ("buffrEdit", EDIT_CONSOLE_SHIM_JS, "buffrEdit"),
            (
                "buffrMediaProbe",
                MEDIA_PROBE_CONSOLE_SHIM_JS,
                "buffrMediaProbe",
            ),
        ] {
            assert!(
                src.contains(&format!("messageHandlers.{handler}.postMessage(args[0])")),
                "{name} shim must forward args[0] unmodified"
            );
            assert!(
                !src.contains(".substring("),
                "{name} shim must not strip the prefix — that discards the nonce"
            );
        }
    }

    /// A line the bridge would forward must still be anchored-parseable on
    /// the Rust side. Guards the exact concatenation order.
    #[test]
    fn bridge_prefix_check_agrees_with_the_rust_parser() {
        let nonce = new_console_nonce();
        let line = wire(
            HINT_CONSOLE_SENTINEL,
            &nonce,
            r#"{"kind":"error","message":"x"}"#,
        );
        // What the JS `indexOf(sentinel) === 0` check sees:
        assert!(line.starts_with(HINT_CONSOLE_SENTINEL));
        // What Rust sees:
        assert!(buffr_core::hint::parse_console_event(&line, &nonce).is_some());
    }

    // ── End-to-end nonce discrimination over the UCM transport ───────────────

    #[test]
    fn nonce_is_128_bits_of_hex() {
        let n = new_console_nonce();
        assert_eq!(n.len(), CONSOLE_NONCE_LEN);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(n, new_console_nonce(), "nonces must not repeat");
    }

    /// The core property: a frame that never learned the nonce cannot drive
    /// the media probe, even though `window.webkit.messageHandlers
    /// .buffrMediaProbe` is reachable from any frame in the web view.
    #[test]
    fn media_probe_rejects_a_forged_line() {
        let real = new_console_nonce();
        let forged = new_console_nonce();
        let json = r#"{"media":true,"video":true}"#;

        let authentic = wire(MEDIA_PROBE_SENTINEL, &real, json);
        let event = buffr_core::media_probe::parse(&authentic, &real)
            .expect("authentic line must verify")
            .expect("payload must decode");
        assert!(event.video);

        let attack = wire(MEDIA_PROBE_SENTINEL, &forged, json);
        assert!(
            buffr_core::media_probe::parse(&attack, &real).is_none(),
            "a line bearing an unknown nonce must be rejected"
        );
        // And the pre-H5 shape (bare sentinel, no nonce) must not verify either.
        let legacy = format!("{MEDIA_PROBE_SENTINEL}{json}");
        assert!(buffr_core::media_probe::parse(&legacy, &real).is_none());
    }

    #[test]
    fn edit_rejects_a_forged_line() {
        let real = new_console_nonce();
        let forged = new_console_nonce();
        let json = r#"{"type":"blur","field_id":"f3"}"#;

        assert!(
            buffr_core::edit::parse_console_event(&wire(EDIT_CONSOLE_SENTINEL, &real, json), &real)
                .is_some_and(|r| r.is_ok()),
            "authentic edit line must verify and decode"
        );
        assert!(
            buffr_core::edit::parse_console_event(
                &wire(EDIT_CONSOLE_SENTINEL, &forged, json),
                &real
            )
            .is_none(),
            "forged edit line must be rejected"
        );
    }

    /// The handlers look the nonce up by `TabId.0 as i32`, matching
    /// `TabSummary::browser_id`. Two tabs must not share a nonce, and a tab
    /// that was never injected into must not verify anything.
    #[test]
    fn nonces_are_keyed_per_tab() {
        let nonces = ConsoleNonces::new();
        let a = nonces.rotate_page(1);
        let b = nonces.rotate_page(2);
        assert_ne!(a, b, "distinct tabs must get distinct page nonces");
        assert_eq!(nonces.page(1), a, "page nonce must be stable per tab");
        assert_eq!(nonces.page(2), b);

        // An unknown tab mints a fresh entry that matches nothing already emitted.
        let unknown = nonces.page(99);
        assert_ne!(unknown, a);
        assert_ne!(unknown, b);
    }

    /// `enter_hint_mode` rotates the hint nonce on every entry while leaving
    /// the page nonce (baked into the tab's UCM scripts) alone — rotating it
    /// would silently kill edit mode, since WebKit re-runs the already-added
    /// `edit.js` with the old nonce.
    #[test]
    fn rotating_hint_leaves_the_page_nonce_intact() {
        let nonces = ConsoleNonces::new();
        let page = nonces.rotate_page(1);
        let hint1 = nonces.rotate_hint(1);
        let hint2 = nonces.rotate_hint(1);

        assert_ne!(hint1, hint2, "each hint session must get a fresh nonce");
        assert_eq!(
            nonces.page(1),
            page,
            "rotating the hint nonce must not disturb the page nonce"
        );
        assert_eq!(nonces.hint(1), hint2);
    }
}
