//! Per-tab WPE WebKit state — lives on the worker thread.
//!
//! `TabEntry` owns a `WebKitWebView*` (opaque C pointer) and the associated
//! exportable FDO backend that delivers SHM frames via callback.
//!
//! # Thread safety
//!
//! `TabEntry` is `!Send` because raw WebKit pointers are thread-confined to
//! the GLib worker thread. All instances are owned by `WpeRuntime` which runs
//! exclusively on the dedicated worker thread.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId};

use super::ffi::*;
use super::worker::EngineState;

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

// ── ExportableContext (callback userdata) ─────────────────────────────────────

/// Userdata pointer passed through the C FDO callback back into Rust.
///
/// Kept alive for the lifetime of the exportable backend via `Box::into_raw`;
/// reclaimed when the TabEntry is dropped.
pub(crate) struct ExportableCtx {
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub exportable: *mut wpe_view_backend_exportable_fdo,
    /// Unused in Phase 2 — placeholder for Phase 3 audio tracking.
    pub _audio_active: Arc<AtomicBool>,
}

// SAFETY: ExportableCtx is only accessed from the single GLib worker thread
// (the SHM callback fires on that thread). Marking Send here so we can store
// it in a Box that is moved into the worker thread closure.
unsafe impl Send for ExportableCtx {}

// ── FDO SHM export callback ───────────────────────────────────────────────────

/// Called by wpebackend-fdo on every composited frame via the SHM path.
///
/// Reads pixel data from the `wl_shm_buffer`, copies rows into the shared
/// `OsrFrame`, and fires the wake callback so the UI thread redraws.
///
/// # Safety
///
/// Called from C; `data` must be a valid `*mut ExportableCtx` for the duration
/// of the callback, and `buf` must be a valid non-null SHM buffer pointer.
pub(crate) unsafe extern "C" fn on_export_shm_buffer(
    data: *mut std::os::raw::c_void,
    buf: *mut wpe_fdo_shm_exported_buffer,
) {
    // SAFETY: `data` was set as `Box::into_raw(ctx)` in TabEntry::new.
    let ctx = unsafe { &*(data as *const ExportableCtx) };

    // SAFETY: buf is non-null per the FDO callback contract.
    let shm = unsafe { wpe_fdo_shm_exported_buffer_get_shm_buffer(buf) };
    if shm.is_null() {
        // Release the buffer so the renderer isn't stalled.
        // SAFETY: exportable is valid for the lifetime of the tab.
        unsafe {
            wpe_view_backend_exportable_fdo_dispatch_frame_complete(ctx.exportable);
            wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(
                ctx.exportable,
                buf,
            );
        }
        return;
    }

    // SAFETY: wl_shm_buffer accessors are safe when called between
    // begin_access / end_access.
    unsafe { wl_shm_buffer_begin_access(shm) };

    let (width, height, stride, data_ptr) = unsafe {
        let w = wl_shm_buffer_get_width(shm);
        let h = wl_shm_buffer_get_height(shm);
        let s = wl_shm_buffer_get_stride(shm);
        let p = wl_shm_buffer_get_data(shm) as *const u8;
        (w, h, s, p)
    };

    if width > 0 && height > 0 && !data_ptr.is_null() {
        let src_len = (stride * height) as usize;
        // SAFETY: data_ptr..data_ptr+src_len is within the shm buffer mapping
        // while we hold the begin_access lock.
        let src = unsafe { std::slice::from_raw_parts(data_ptr, src_len) };

        let w = width as u32;
        let h = height as u32;
        let frame_len = (w * h * 4) as usize;

        if let Ok(mut frame) = ctx.frame.lock() {
            if frame.width != w || frame.height != h {
                frame.width = w;
                frame.height = h;
                frame.pixels.resize(frame_len, 0);
                frame.needs_fresh = false;
            }

            // WPE SHM format is WL_SHM_FORMAT_ARGB8888 (0) or XRGB8888 (1).
            // On little-endian both are stored as [B,G,R,A/X] in memory =
            // BGRA, which is what OsrFrame expects. Direct row copy is correct.
            let dst = &mut frame.pixels;
            for row in 0..(h as i32) {
                let src_off = (row * stride) as usize;
                let dst_off = (row as usize) * (w as usize) * 4;
                let row_len = (w as usize) * 4;
                if src_off + row_len <= src.len() && dst_off + row_len <= dst.len() {
                    dst[dst_off..dst_off + row_len]
                        .copy_from_slice(&src[src_off..src_off + row_len]);
                }
            }

            frame.generation = frame.generation.wrapping_add(1);
        }

        // Notify the paint wake callback so the UI thread schedules a redraw.
        if let Some(wake) = ctx.view.wake.get() {
            wake();
        }
    }

    unsafe { wl_shm_buffer_end_access(shm) };

    // Dispatch frame_complete so WPE schedules the next frame.
    // SAFETY: exportable is valid for the lifetime of the tab.
    unsafe {
        wpe_view_backend_exportable_fdo_dispatch_frame_complete(ctx.exportable);
        wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(ctx.exportable, buf);
    }
}

// ── GLib signal helpers (raw FFI without gtk4 dep) ────────────────────────────

// SAFETY: These symbols are provided by GLib (linked via wpe-webkit-2.0).
unsafe extern "C" {
    fn g_signal_connect_data(
        instance: *mut std::os::raw::c_void,
        detailed_signal: *const std::os::raw::c_char,
        c_handler: Option<unsafe extern "C" fn()>,
        data: *mut std::os::raw::c_void,
        destroy_data: Option<
            unsafe extern "C" fn(*mut std::os::raw::c_void, *mut std::os::raw::c_void),
        >,
        connect_flags: u32,
    ) -> u64;

    fn g_signal_handler_disconnect(instance: *mut std::os::raw::c_void, handler_id: u64);

    fn g_object_unref(object: *mut std::os::raw::c_void);
}

// ── Signal handler: load-changed ──────────────────────────────────────────────

/// Fires when the WebView's load state changes.
///
/// # Safety
///
/// Called from GLib's signal dispatch. `data` must be a valid
/// `Arc<Mutex<EngineState>>` raw pointer (kept alive by `arc_destroy`).
unsafe extern "C" fn on_load_changed(
    web_view: *mut WebKitWebView,
    load_event: std::os::raw::c_int,
    data: *mut std::os::raw::c_void,
) {
    // Reconstruct the Arc without consuming it.
    let es = unsafe { Arc::from_raw(data as *const Mutex<EngineState>) };
    let es_clone = Arc::clone(&es);
    std::mem::forget(es); // don't drop the refcount

    let is_loading = load_event != 3; // WEBKIT_LOAD_FINISHED = 3
    let (progress, uri) = unsafe {
        let p = webkit_web_view_get_estimated_load_progress(web_view);
        let uri_ptr = webkit_web_view_get_uri(web_view);
        let uri = if uri_ptr.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(uri_ptr)
                .to_string_lossy()
                .into_owned()
        };
        (p, uri)
    };

    if let Ok(mut st) = es_clone.lock() {
        if let Some(idx) = st.active_idx {
            if let Some(tab) = st.tabs.get_mut(idx) {
                tab.is_loading = is_loading;
                tab.progress = progress;
                if !uri.is_empty() {
                    tab.url = uri;
                }
            }
        }
        st.address_changed = true;
    }
}

// ── Signal handler: notify (title / uri) ─────────────────────────────────────

/// Fires on `notify::title` and `notify::uri`.
///
/// # Safety
///
/// Called from GLib's signal dispatch. `data` must be a valid
/// `Arc<Mutex<EngineState>>` raw pointer.
unsafe extern "C" fn on_notify_prop(
    web_view: *mut WebKitWebView,
    _pspec: *mut std::os::raw::c_void,
    data: *mut std::os::raw::c_void,
) {
    let es = unsafe { Arc::from_raw(data as *const Mutex<EngineState>) };
    let es_clone = Arc::clone(&es);
    std::mem::forget(es);

    let (title, uri, is_loading, progress) = unsafe {
        let t_ptr = webkit_web_view_get_title(web_view);
        let u_ptr = webkit_web_view_get_uri(web_view);
        let loading = webkit_web_view_is_loading(web_view) != 0;
        let prog = webkit_web_view_get_estimated_load_progress(web_view);

        let t = if t_ptr.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr(t_ptr)
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        let u = if u_ptr.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr(u_ptr)
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        (t, u, loading, prog)
    };

    if let Ok(mut st) = es_clone.lock() {
        if let Some(idx) = st.active_idx {
            if let Some(tab) = st.tabs.get_mut(idx) {
                if let Some(t) = title {
                    if !t.is_empty() {
                        tab.title = t;
                    }
                }
                if let Some(u) = uri {
                    if !u.is_empty() {
                        tab.url = u;
                    }
                }
                tab.is_loading = is_loading;
                tab.progress = progress;
            }
        }
        st.address_changed = true;
    }
}

/// GLib destroy-data callback: drops the `Arc<Mutex<EngineState>>` refcount.
///
/// # Safety
///
/// `data` must be a valid `Arc<Mutex<EngineState>>` raw pointer.
unsafe extern "C" fn arc_destroy(data: *mut std::os::raw::c_void, _obj: *mut std::os::raw::c_void) {
    // Reconstruct and immediately drop the Arc.
    let _ = unsafe { Arc::from_raw(data as *const Mutex<EngineState>) };
}

fn g_signal_connect_load_changed(
    web_view: *mut WebKitWebView,
    es_raw: *mut std::os::raw::c_void,
) -> u64 {
    unsafe {
        g_signal_connect_data(
            web_view as *mut _,
            b"load-changed\0".as_ptr() as *const _,
            // SAFETY: function pointer cast from matching signature.
            Some(std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    std::os::raw::c_int,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_load_changed)),
            es_raw,
            Some(arc_destroy),
            0,
        )
    }
}

fn g_signal_connect_notify(
    web_view: *mut WebKitWebView,
    prop: &[u8], // e.g. b"title\0"
    es_raw: *mut std::os::raw::c_void,
) -> u64 {
    // Build "notify::prop" signal name (prop already includes \0).
    let mut sig = b"notify::".to_vec();
    sig.extend_from_slice(prop);
    unsafe {
        g_signal_connect_data(
            web_view as *mut _,
            sig.as_ptr() as *const _,
            // SAFETY: function pointer cast from matching signature.
            Some(std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    *mut std::os::raw::c_void,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_notify_prop)),
            es_raw,
            Some(arc_destroy),
            0,
        )
    }
}

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab. Owns the raw WPE WebKit pointers for its lifetime.
///
/// Drop order: disconnect signals → unref WebView → destroy FDO exportable →
/// free ExportableCtx.
pub(crate) struct TabEntry {
    pub id: TabId,
    /// Raw `WebKitWebView*` — owned, `g_object_unref`'d on drop.
    pub web_view: *mut WebKitWebView,
    /// Raw `WebKitWebViewBackend*` — owned via the WebView.
    pub wk_backend: *mut WebKitWebViewBackend,
    /// FDO exportable backend — destroyed after the WebView is unref'd.
    pub exportable: *mut wpe_view_backend_exportable_fdo,
    /// Heap-allocated callback context — freed on drop.
    ctx_ptr: *mut ExportableCtx,
    /// GLib signal handler IDs for disconnect on drop.
    load_changed_id: u64,
    notify_title_id: u64,
    notify_uri_id: u64,
}

// SAFETY: TabEntry owns C pointers that are used only on the worker thread.
unsafe impl Send for TabEntry {}

impl TabEntry {
    /// Create a WPE WebKit WebView with a SHM exportable FDO backend.
    ///
    /// Returns `None` if any WPE/WebKit allocation fails.
    pub(crate) fn new(
        id: TabId,
        url: &str,
        width: u32,
        height: u32,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Option<Self> {
        // ── Allocate ExportableCtx (userdata for the C callback) ───────────────
        let ctx = Box::new(ExportableCtx {
            frame: Arc::clone(&frame),
            view: Arc::clone(&view),
            exportable: std::ptr::null_mut(),
            _audio_active: Arc::new(AtomicBool::new(false)),
        });
        let ctx_ptr = Box::into_raw(ctx);

        let (exportable, wpe_backend, wk_backend, web_view) = unsafe {
            // ── FDO SHM exportable client vtable ──────────────────────────────
            let fdo_client = wpe_view_backend_exportable_fdo_client {
                export_buffer_resource: None,
                export_dmabuf_resource: None,
                export_shm_buffer: Some(on_export_shm_buffer),
                _wpe_reserved0: None,
                _wpe_reserved1: None,
            };

            // ── Create FDO exportable backend ──────────────────────────────────
            let exportable = wpe_view_backend_exportable_fdo_create(
                &fdo_client as *const _,
                ctx_ptr as *mut std::os::raw::c_void,
                width,
                height,
            );
            if exportable.is_null() {
                drop(Box::from_raw(ctx_ptr));
                tracing::error!("webkit: wpe_view_backend_exportable_fdo_create returned null");
                return None;
            }

            // Patch ctx with the now-known exportable pointer.
            (*ctx_ptr).exportable = exportable;

            // ── Get raw wpe_view_backend ───────────────────────────────────────
            let wpe_backend = wpe_view_backend_exportable_fdo_get_view_backend(exportable);
            if wpe_backend.is_null() {
                wpe_view_backend_exportable_fdo_destroy(exportable);
                drop(Box::from_raw(ctx_ptr));
                tracing::error!("webkit: get_view_backend returned null");
                return None;
            }

            // ── Wrap in WebKitWebViewBackend ───────────────────────────────────
            let wk_backend = webkit_web_view_backend_new(wpe_backend, None, std::ptr::null_mut());
            if wk_backend.is_null() {
                wpe_view_backend_exportable_fdo_destroy(exportable);
                drop(Box::from_raw(ctx_ptr));
                tracing::error!("webkit: webkit_web_view_backend_new returned null");
                return None;
            }

            // ── Set activity state: visible + focused + in_window ─────────────
            use super::ffi::{
                wpe_view_activity_state_wpe_view_activity_state_focused as FOCUSED,
                wpe_view_activity_state_wpe_view_activity_state_in_window as IN_WINDOW,
                wpe_view_activity_state_wpe_view_activity_state_visible as VISIBLE,
            };
            wpe_view_backend_add_activity_state(wpe_backend, VISIBLE | FOCUSED | IN_WINDOW);
            wpe_view_backend_dispatch_set_size(wpe_backend, width, height);

            // ── Create WebKitWebView ───────────────────────────────────────────
            let web_view = webkit_web_view_new(wk_backend);
            if web_view.is_null() {
                wpe_view_backend_exportable_fdo_destroy(exportable);
                drop(Box::from_raw(ctx_ptr));
                tracing::error!("webkit: webkit_web_view_new returned null");
                return None;
            }

            (exportable, wpe_backend, wk_backend, web_view)
        };

        let _ = wpe_backend; // suppress unused warning (activity state already set)

        // ── Connect GLib signals ───────────────────────────────────────────────

        // Each signal gets its own Arc clone. `arc_destroy` frees the refcount.
        let es_lc = Arc::clone(&engine_state);
        let load_changed_id =
            g_signal_connect_load_changed(web_view, Arc::into_raw(es_lc) as *mut _);

        let es_title = Arc::clone(&engine_state);
        let notify_title_id =
            g_signal_connect_notify(web_view, b"title\0", Arc::into_raw(es_title) as *mut _);

        let es_uri = Arc::clone(&engine_state);
        let notify_uri_id =
            g_signal_connect_notify(web_view, b"uri\0", Arc::into_raw(es_uri) as *mut _);

        // ── Load initial URL ───────────────────────────────────────────────────
        let url_cstr = std::ffi::CString::new(url).unwrap_or_default();
        // SAFETY: web_view is valid; url_cstr is null-terminated.
        unsafe { webkit_web_view_load_uri(web_view, url_cstr.as_ptr()) };

        tracing::info!("webkit: created WebView id={id:?} url={url}");

        Some(TabEntry {
            id,
            web_view,
            wk_backend,
            exportable,
            ctx_ptr,
            load_changed_id,
            notify_title_id,
            notify_uri_id,
        })
    }

    /// Navigate to a new URL.
    pub(crate) fn load_uri(&self, url: &str) {
        let url_cstr = std::ffi::CString::new(url).unwrap_or_default();
        // SAFETY: web_view is valid for the lifetime of TabEntry.
        unsafe { webkit_web_view_load_uri(self.web_view, url_cstr.as_ptr()) };
    }

    /// Resize the underlying WPE view backend.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        // SAFETY: wk_backend is valid.
        let wpe = unsafe { webkit_web_view_backend_get_wpe_backend(self.wk_backend) };
        if !wpe.is_null() {
            // SAFETY: wpe is non-null.
            unsafe { wpe_view_backend_dispatch_set_size(wpe, width, height) };
        }
    }

    /// Current URL from the WebView (may lag by one GLib tick).
    pub(crate) fn current_url(&self) -> String {
        // SAFETY: web_view is valid.
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

    /// Current title from the WebView.
    pub(crate) fn current_title(&self) -> String {
        // SAFETY: web_view is valid.
        let ptr = unsafe { webkit_web_view_get_title(self.web_view) };
        if ptr.is_null() {
            String::new()
        } else {
            // SAFETY: ptr is a valid null-terminated UTF-8 string.
            unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Raw `wpe_view_backend*` for input dispatch.
    pub(crate) fn wpe_backend(&self) -> *mut wpe_view_backend {
        // SAFETY: wk_backend is valid.
        unsafe { webkit_web_view_backend_get_wpe_backend(self.wk_backend) }
    }

    /// Whether the tab is currently playing audio.
    pub(crate) fn is_playing_audio(&self) -> bool {
        // SAFETY: web_view is valid.
        unsafe { webkit_web_view_is_playing_audio(self.web_view) != 0 }
    }
}

impl Drop for TabEntry {
    fn drop(&mut self) {
        // SAFETY: pointers are valid. GLib owns the signal lifetime; disconnect
        // before unref to prevent callbacks firing on a freed object.
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

            g_object_unref(self.web_view as *mut _);
            wpe_view_backend_exportable_fdo_destroy(self.exportable);
            drop(Box::from_raw(self.ctx_ptr));
        }
    }
}

// ── WpeRuntime ────────────────────────────────────────────────────────────────

/// Top-level runtime that lives on the GLib worker thread.
///
/// Owns the single open tab (Phase 2: single tab). Multi-tab lifecycle is
/// Phase 3.
pub(crate) struct WpeRuntime {
    pub tab: Option<TabEntry>,
    pub engine_state: Arc<Mutex<EngineState>>,
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
}

impl WpeRuntime {
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Self {
        Self {
            tab: None,
            engine_state,
            frame,
            view,
        }
    }

    /// Open a tab (single-tab Phase 2: replaces any existing tab).
    pub(crate) fn open_tab(&mut self, url: &str) -> Result<TabId, String> {
        let id = {
            let mut st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex poison: {e}"))?;
            let id = TabId(st.next_id);
            st.next_id += 1;
            id
        };

        let (width, height) = {
            let st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex: {e}"))?;
            (st.width, st.height)
        };

        self.tab = None; // drop existing tab

        let entry = TabEntry::new(
            id,
            url,
            width,
            height,
            Arc::clone(&self.frame),
            Arc::clone(&self.view),
            Arc::clone(&self.engine_state),
        )
        .ok_or_else(|| "TabEntry::new returned None".to_string())?;

        let info = TabInfo::new(id, url);
        if let Ok(mut st) = self.engine_state.lock() {
            st.tabs.clear();
            st.tabs.push(info);
            st.active_idx = Some(0);
        }

        self.tab = Some(entry);
        Ok(id)
    }

    /// Navigate the active tab to `url`.
    pub(crate) fn navigate(&mut self, url: &str) {
        if let Some(tab) = &self.tab {
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

    /// Resize the viewport and the OsrFrame.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if let Ok(mut st) = self.engine_state.lock() {
            st.width = width;
            st.height = height;
        }
        if let Some(tab) = &self.tab {
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

    /// Dispatch a keyboard event via the WPE view backend.
    pub(crate) fn dispatch_keyboard(&self, ev: wpe_input_keyboard_event) {
        if let Some(tab) = &self.tab {
            let backend = tab.wpe_backend();
            if !backend.is_null() {
                let mut ev = ev;
                // SAFETY: backend is valid; ev is stack-allocated.
                unsafe { wpe_view_backend_dispatch_keyboard_event(backend, &mut ev as *mut _) };
            }
        }
    }

    /// Dispatch a pointer (mouse) event.
    pub(crate) fn dispatch_pointer(&self, ev: wpe_input_pointer_event) {
        if let Some(tab) = &self.tab {
            let backend = tab.wpe_backend();
            if !backend.is_null() {
                let mut ev = ev;
                // SAFETY: backend is valid.
                unsafe { wpe_view_backend_dispatch_pointer_event(backend, &mut ev as *mut _) };
            }
        }
    }

    /// Dispatch an axis (wheel/scroll) event.
    pub(crate) fn dispatch_axis(&self, ev: wpe_input_axis_event) {
        if let Some(tab) = &self.tab {
            let backend = tab.wpe_backend();
            if !backend.is_null() {
                let mut ev = ev;
                // SAFETY: backend is valid.
                unsafe { wpe_view_backend_dispatch_axis_event(backend, &mut ev as *mut _) };
            }
        }
    }

    /// Check audio activity.
    pub(crate) fn any_audio_active(&self) -> bool {
        self.tab
            .as_ref()
            .map(|t| t.is_playing_audio())
            .unwrap_or(false)
    }
}
