//! Per-tab WebView2 lifecycle — lives on the STA worker thread.
//!
//! `TabEntry` owns an `ICoreWebView2Controller` (HWND path) and an
//! `ICoreWebView2` and wires navigation event delegates back into the shared
//! `Arc<Mutex<EngineState>>` so the engine thread can read URL / title / load
//! state without blocking.
//!
//! # Controller path: HWND
//!
//! Phase B-2 uses `CreateCoreWebView2Controller` (HWND path) rather than the
//! composition path (`CreateCoreWebView2CompositionController`). The HWND path:
//!
//! - Cross-compiles cleanly to `x86_64-pc-windows-gnu`.
//! - Supports all navigation events and `ICoreWebView2::Navigate`.
//! - Avoids WinRT Visual-layer interop (`DesktopWindowTarget`, etc.) which
//!   adds D3D11 / DXGI setup and composition-specific APIs not needed for
//!   Phase B.
//!
//! A hidden 1×1 top-level HWND is created for each tab and passed to
//! `CreateCoreWebView2Controller`. `ICoreWebView2Controller::SetIsVisible(false)`
//! ensures it never appears on screen.
//!
//! TODO(phase-c): Switch to composition controller + `CapturePreview` or D3D11
//! staging readback for the full OSR pixel pipeline.
//!
//! # Thread safety
//!
//! `TabEntry` is `!Send` because `ICoreWebView2Controller` and `ICoreWebView2`
//! are COM apartment-threaded (STA). All `TabEntry` instances are owned by
//! `StaRuntime` which itself runs exclusively on the dedicated STA thread.
//!
//! # Event wiring
//!
//! Five navigation events are wired per tab:
//! - `add_NavigationStarting` — marks the tab loading, clears progress.
//! - `add_NavigationCompleted` — marks load done.
//! - `add_SourceChanged` — reads `ICoreWebView2::Source` into shared state.
//! - `add_DocumentTitleChanged` — reads `ICoreWebView2::DocumentTitle`.
//! - `add_HistoryChanged` — reads `ICoreWebView2::CanGoBack/CanGoForward`.
//! - `add_FaviconChanged` — available on `ICoreWebView2_15`, no-op state update.
//!
//! `add_FaviconChanged` lives on `ICoreWebView2_15` (cast from the base
//! `ICoreWebView2`). If the cast fails (older runtime) the event is skipped
//! without error.
//!
//! # Safety discipline
//!
//! All COM calls in this file are in `unsafe {}` blocks on the STA thread.
//! Event handler closures created inside a top-level `unsafe {}` block inherit
//! that unsafe context (Rust 2024 edition), so the closure bodies that call
//! COM methods do not need a redundant inner `unsafe {}`.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};

#[cfg(target_os = "windows")]
use buffr_core::hint::{HINT_CONSOLE_SENTINEL, HintEventSink, parse_console_event};
use buffr_engine::TabId;
#[cfg(target_os = "windows")]
use buffr_engine::popup::PopupQueue;

use super::worker::{Command, EngineState, TabInfo};

#[cfg(target_os = "windows")]
use super::error::WebView2Error;

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab on the STA thread.
pub(crate) struct TabEntry {
    pub id: TabId,
    /// Current URL, updated by SourceChanged.
    pub url: String,
    /// Current title, updated by DocumentTitleChanged.
    pub title: String,
    /// Whether the tab is actively loading.
    pub is_loading: bool,
    /// Back-stack presence (updated by HistoryChanged).
    pub can_go_back: bool,
    /// Forward-stack presence.
    pub can_go_forward: bool,
    /// Load progress 0.0–1.0.
    pub progress: f64,
    /// COM controller (HWND path). Created once in `new`; kept for the tab
    /// lifetime so WebView2 stays alive.
    #[cfg(target_os = "windows")]
    pub(crate) controller: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
    /// Raw ICoreWebView2 interface. Obtained from `controller.CoreWebView2()`.
    #[cfg(target_os = "windows")]
    webview: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2,
    /// Hidden HWND used as controller parent. Kept alive for the tab lifetime.
    #[cfg(target_os = "windows")]
    _hwnd: windows::Win32::Foundation::HWND,
    /// Event registration tokens — stored so we could remove them on Drop if
    /// needed. In Phase B-2 we rely on controller `Close()` to clean them up.
    #[cfg(target_os = "windows")]
    _tokens: EventTokens,
}

/// Event registration tokens returned by `add_*` COM methods.
///
/// Stored for completeness (allows explicit `remove_*` calls in future).
/// Currently the tokens are freed implicitly when `ICoreWebView2Controller::Close`
/// is called in `TabEntry::drop`.
#[cfg(target_os = "windows")]
#[allow(dead_code)]
struct EventTokens {
    nav_starting: i64,
    nav_completed: i64,
    source_changed: i64,
    title_changed: i64,
    history_changed: i64,
}

impl TabEntry {
    /// Create a new tab entry: allocate a hidden HWND, call
    /// `CreateCoreWebView2Controller` (async, pumped via `wait_with_pump`),
    /// obtain the `ICoreWebView2`, wire all events, then trigger the initial
    /// navigation.
    #[cfg(target_os = "windows")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: TabId,
        url: &str,
        engine_state: &Arc<Mutex<EngineState>>,
        environment: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
        cmd_tx: &mpsc::SyncSender<Command>,
        hint_sink: HintEventSink,
        video_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cursor_change: std::sync::Arc<Mutex<Option<(i32, u32)>>>,
        history: Option<std::sync::Arc<buffr_history::History>>,
        downloads: Option<std::sync::Arc<buffr_downloads::Downloads>>,
        notice_queue: Option<buffr_core::download_notice::DownloadNoticeQueue>,
        popup_queue: PopupQueue,
    ) -> Result<Self, WebView2Error> {
        use webview2_com::{
            AddScriptToExecuteOnDocumentCreatedCompletedHandler,
            CreateCoreWebView2ControllerCompletedHandler, DocumentTitleChangedEventHandler,
            HistoryChangedEventHandler, NavigationCompletedEventHandler,
            NavigationStartingEventHandler, SourceChangedEventHandler,
            WebMessageReceivedEventHandler,
        };
        use windows::Win32::Foundation::E_POINTER;

        // Insert initial tab info into shared state before COM is ready, so
        // the engine thread always sees a tab entry.
        if let Ok(mut guard) = engine_state.lock() {
            guard.tabs.push(TabInfo {
                id,
                url: url.to_owned(),
                title: String::new(),
                is_loading: true,
                can_go_back: false,
                can_go_forward: false,
                progress: 0.0,
                zoom: 1.0,
            });
        }

        // Create hidden HWND for the controller.
        let hwnd = super::worker::create_hidden_hwnd()?;

        // ── Controller construction (async via wait_with_pump) ────────────────
        //
        // `wait_for_async_operation` calls the provided closure (which fires
        // `CreateCoreWebView2Controller`), then runs a GetMessage/DispatchMessage
        // loop pumping the STA apartment until the completion handler fires.
        let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<
            Result<
                webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
                windows::core::Error,
            >,
        >();

        let env_clone = environment.clone();

        CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| {
                // SAFETY: CreateCoreWebView2Controller is an STA COM method.
                // `hwnd` is a valid HWND we just created. `handler` is a valid
                // COM interface pointer that lives for the duration of this call.
                unsafe {
                    env_clone
                        .CreateCoreWebView2Controller(hwnd, &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }
            }),
            Box::new(move |error_code, ctrl| {
                error_code?;
                ctrl_tx
                    .send(ctrl.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .expect("send controller over mpsc");
                Ok(())
            }),
        )
        .map_err(|e| WebView2Error::InitFailed(format!("CreateCoreWebView2Controller: {e}")))?;

        let controller = ctrl_rx
            .recv()
            .map_err(|_| WebView2Error::InitFailed("controller channel closed".into()))?
            .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;

        // SAFETY: SetIsVisible + CoreWebView2 are STA COM methods on a valid
        // controller pointer that we own on this thread.
        unsafe {
            controller
                .SetIsVisible(false)
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        let webview = unsafe {
            controller
                .CoreWebView2()
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?
        };

        // ── Wire navigation events ────────────────────────────────────────────
        //
        // Each `add_*` call is inside its own `unsafe {}` block. The closures
        // passed as handlers are defined inside those blocks and, in Rust 2024,
        // inherit the unsafe context — so COM calls inside closure bodies do not
        // need a redundant inner `unsafe {}`.

        let state_nav_starting = Arc::clone(engine_state);
        let state_nav_completed = Arc::clone(engine_state);
        let state_source = Arc::clone(engine_state);
        let state_title = Arc::clone(engine_state);
        let state_history = Arc::clone(engine_state);
        // Clone for NavigationCompleted → TriggerCapture post.
        let cmd_tx_nav_completed = cmd_tx.clone();
        // Clone for NavigationCompleted → history.record_visit.
        // ICoreWebView2 is COM STA — valid to hold on this thread.
        let wv_nav_completed = webview.clone();

        let mut nav_starting_token: i64 = 0;
        let mut nav_completed_token: i64 = 0;
        let mut source_token: i64 = 0;
        let mut title_token: i64 = 0;
        let mut history_token: i64 = 0;

        // ── NavigationStarting ────────────────────────────────────────────────
        // SAFETY: add_NavigationStarting is an STA COM method on `webview`.
        // The handler closure captures only Arc<Mutex<EngineState>> which is
        // Send; the closure itself is called on the STA thread by WebView2.
        unsafe {
            webview
                .add_NavigationStarting(
                    &NavigationStartingEventHandler::create(Box::new(move |_sender, _args| {
                        if let Ok(mut guard) = state_nav_starting.lock() {
                            if let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id) {
                                tab.is_loading = true;
                                tab.progress = 0.0;
                            }
                            // Keep loading_active in sync for the active tab.
                            guard.sync_loading_active();
                        }
                        Ok(())
                    })),
                    &mut nav_starting_token,
                )
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        // ── NavigationCompleted ───────────────────────────────────────────────
        // SAFETY: same invariants as NavigationStarting above.
        // The closure also captures `wv_nav_completed` (ICoreWebView2 — COM STA
        // pointer valid on this thread) and `history` (Arc<History> — Send).
        // Reading Source() and DocumentTitle() from inside the NavigationCompleted
        // handler is safe: the page has finished navigating, so Source returns the
        // committed URL and DocumentTitle returns the final page title.
        unsafe {
            webview
                .add_NavigationCompleted(
                    &NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
                        if let Ok(mut guard) = state_nav_completed.lock() {
                            if let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id) {
                                tab.is_loading = false;
                                tab.progress = 1.0;
                            }
                            // Keep loading_active in sync for the active tab.
                            guard.sync_loading_active();
                        }

                        // ── History record ────────────────────────────────────
                        //
                        // Read the committed URL and document title from the
                        // webview on the STA thread, then call
                        // `History::record_visit`. Both Source() and
                        // DocumentTitle() return CoTask-allocated PWSTRs that we
                        // own and must free.
                        if let Some(ref hist) = history {
                            let mut uri_pwstr = windows::core::PWSTR::null();
                            let url_str = if wv_nav_completed.Source(&mut uri_pwstr).is_ok()
                                && !uri_pwstr.is_null()
                            {
                                let s = uri_pwstr.to_string().unwrap_or_default();
                                windows::Win32::System::Com::CoTaskMemFree(Some(
                                    uri_pwstr.0.cast(),
                                ));
                                s
                            } else {
                                String::new()
                            };
                            if !url_str.is_empty() {
                                let mut title_pwstr = windows::core::PWSTR::null();
                                let title_str =
                                    if wv_nav_completed.DocumentTitle(&mut title_pwstr).is_ok()
                                        && !title_pwstr.is_null()
                                    {
                                        let s = title_pwstr.to_string().unwrap_or_default();
                                        windows::Win32::System::Com::CoTaskMemFree(Some(
                                            title_pwstr.0.cast(),
                                        ));
                                        s
                                    } else {
                                        String::new()
                                    };
                                let title_opt = if title_str.is_empty() {
                                    None
                                } else {
                                    Some(title_str.as_str())
                                };
                                if let Err(e) = hist.record_visit(
                                    &url_str,
                                    title_opt,
                                    buffr_history::Transition::Link,
                                ) {
                                    tracing::debug!(
                                        url = %url_str,
                                        error = %e,
                                        "webview2 runtime: history.record_visit failed (non-fatal)"
                                    );
                                } else {
                                    tracing::debug!(
                                        url = %url_str,
                                        "webview2 runtime: history.record_visit ok"
                                    );
                                }
                            }
                        }

                        // Post a TriggerCapture so the worker fires CapturePreview
                        // as soon as the page finishes loading. Fire-and-forget:
                        // if the channel is full the capture will happen on the
                        // next 250 ms OSR timer tick instead.
                        let _ = cmd_tx_nav_completed.try_send(Command::TriggerCapture);
                        Ok(())
                    })),
                    &mut nav_completed_token,
                )
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        // ── SourceChanged ─────────────────────────────────────────────────────
        // SAFETY: add_SourceChanged is an STA COM method. The closure captures a
        // clone of `webview` (ICoreWebView2 is COM STA — safe to hold on this
        // thread) and calls ICoreWebView2::Source which writes a CoTask PWSTR.
        // The PWSTR is freed with CoTaskMemFree after conversion to String.
        let wv_source = webview.clone();
        unsafe {
            webview
                .add_SourceChanged(
                    &SourceChangedEventHandler::create(Box::new(move |_sender, _args| {
                        let mut uri_pwstr = windows::core::PWSTR::null();
                        // ICoreWebView2::Source: CoTask-allocated PWSTR out-param.
                        if wv_source.Source(&mut uri_pwstr).is_ok() {
                            let uri = if uri_pwstr.is_null() {
                                String::new()
                            } else {
                                // uri_pwstr is valid; convert then free.
                                let s = uri_pwstr.to_string().unwrap_or_default();
                                // CoTaskMemFree the PWSTR allocated by Source.
                                windows::Win32::System::Com::CoTaskMemFree(Some(
                                    uri_pwstr.0.cast(),
                                ));
                                s
                            };
                            if let Ok(mut guard) = state_source.lock()
                                && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                            {
                                tab.url = uri;
                            }
                        }
                        Ok(())
                    })),
                    &mut source_token,
                )
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        // ── DocumentTitleChanged ──────────────────────────────────────────────
        // SAFETY: same invariants as SourceChanged. ICoreWebView2::DocumentTitle
        // writes a CoTask-allocated PWSTR that we own and must free.
        let wv_title = webview.clone();
        unsafe {
            webview
                .add_DocumentTitleChanged(
                    &DocumentTitleChangedEventHandler::create(Box::new(move |_sender, _args| {
                        let mut title_pwstr = windows::core::PWSTR::null();
                        if wv_title.DocumentTitle(&mut title_pwstr).is_ok() {
                            let title = if title_pwstr.is_null() {
                                String::new()
                            } else {
                                // title_pwstr is valid; convert then free.
                                let s = title_pwstr.to_string().unwrap_or_default();
                                // CoTaskMemFree the PWSTR allocated by DocumentTitle.
                                windows::Win32::System::Com::CoTaskMemFree(Some(
                                    title_pwstr.0.cast(),
                                ));
                                s
                            };
                            if let Ok(mut guard) = state_title.lock()
                                && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                            {
                                tab.title = title;
                            }
                        }
                        Ok(())
                    })),
                    &mut title_token,
                )
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        // ── HistoryChanged ────────────────────────────────────────────────────
        // SAFETY: CanGoBack / CanGoForward write to stack-allocated BOOL
        // out-parameters. The webview clone is valid on this STA thread.
        let wv_history = webview.clone();
        unsafe {
            webview
                .add_HistoryChanged(
                    &HistoryChangedEventHandler::create(Box::new(move |_sender, _args| {
                        let mut can_back = windows::core::BOOL::default();
                        let mut can_fwd = windows::core::BOOL::default();
                        let back_ok = wv_history.CanGoBack(&mut can_back).is_ok();
                        let fwd_ok = wv_history.CanGoForward(&mut can_fwd).is_ok();
                        if let Ok(mut guard) = state_history.lock()
                            && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                        {
                            if back_ok {
                                tab.can_go_back = can_back.as_bool();
                            }
                            if fwd_ok {
                                tab.can_go_forward = can_fwd.as_bool();
                            }
                        }
                        Ok(())
                    })),
                    &mut history_token,
                )
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        // ── FaviconChanged (ICoreWebView2_15, optional) ───────────────────────
        //
        // `add_FaviconChanged` lives on ICoreWebView2_15. We QI-cast and skip
        // gracefully if the runtime predates SDK 1.0.1369 (runtime ~107).
        //
        // On each event we call `ICoreWebView2_15::FaviconUri` to read the URL,
        // then spawn a background thread that fetches the image with `ureq`,
        // decodes it with the `image` crate, and pushes a `FaviconUpdate` with
        // decoded RGBA pixels into the shared `favicon_updates` queue.
        //
        // The spawned thread is fire-and-forget; failures are debug-logged.
        // This mirrors the CEF `BuffrDownloadImageCallback` pattern without
        // the CEF-specific `download_image` round-trip.
        if let Ok(wv15) = windows::core::Interface::cast::<
            webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_15,
        >(&webview)
        {
            use webview2_com::FaviconChangedEventHandler;
            // Clone the favicon_updates Arc directly so the spawned thread
            // can push without holding the full EngineState lock.
            let favicon_updates = engine_state
                .lock()
                .map(|g| Arc::clone(&g.favicon_updates))
                .unwrap_or_else(|_| Arc::new(Mutex::new(Vec::new())));
            let wv15_fav = wv15.clone();
            let mut favicon_token: i64 = 0;
            // SAFETY: add_FaviconChanged is an STA COM method on `wv15`.
            // The handler closure captures:
            //   - `Arc<Mutex<Vec<FaviconUpdate>>>` which is Send,
            //   - `ICoreWebView2_15` (COM STA pointer, valid on this thread),
            //   - `id` (Copy).
            // The closure is called by WebView2 on the STA thread.
            if let Err(e) = unsafe {
                wv15.add_FaviconChanged(
                    &FaviconChangedEventHandler::create(Box::new(move |_sender, _args| {
                        // Read the current favicon URI from the webview.
                        let mut uri_pwstr = windows::core::PWSTR::null();
                        let uri_str = if wv15_fav.FaviconUri(&mut uri_pwstr).is_ok()
                            && !uri_pwstr.is_null()
                        {
                            // SAFETY: uri_pwstr is a valid CoTask-allocated PWSTR.
                            // Convert to String then free with CoTaskMemFree.
                            let s = uri_pwstr.to_string().unwrap_or_default();
                            windows::Win32::System::Com::CoTaskMemFree(Some(uri_pwstr.0.cast()));
                            s
                        } else {
                            String::new()
                        };

                        tracing::debug!(
                            tab_id = id.0,
                            favicon_uri = %uri_str,
                            "webview2 runtime: FaviconChanged"
                        );

                        if uri_str.is_empty() {
                            return Ok(());
                        }

                        // Spawn a background thread to fetch and decode the favicon.
                        // The STA thread must not block on network I/O.
                        let browser_id = id.0 as i32;
                        let favicon_sink = Arc::clone(&favicon_updates);
                        std::thread::Builder::new()
                            .name(format!("buffr-wv2-favicon-{browser_id}"))
                            .spawn(move || {
                                fetch_and_push_favicon(uri_str, browser_id, favicon_sink);
                            })
                            .ok();
                        Ok(())
                    })),
                    &mut favicon_token,
                )
            } {
                tracing::debug!(
                    "webview2 runtime: add_FaviconChanged skipped (older runtime?): {e}"
                );
            }
        }

        // ── Download wiring (ICoreWebView2_4::add_DownloadStarting) ─────────────
        //
        // Subscribe to the download-starting event to record downloads in the
        // shared `buffr_downloads::Downloads` store and push a `DownloadNotice`
        // onto the UI-layer queue.
        //
        // `add_DownloadStarting` lives on `ICoreWebView2_4`. We QI-cast from the
        // base `ICoreWebView2` and skip gracefully if the cast fails (older
        // runtime). This matches the `FaviconChanged` / `ICoreWebView2_15` pattern.
        //
        // The handler:
        //   1. Reads `DownloadOperation` from the event args.
        //   2. Reads `Uri` + `MimeType` + `TotalBytesToReceive` from the operation.
        //   3. Calls `downloads.record_started` (idempotent on cef_id=0 — we use
        //      the tab id as a surrogate since WebView2 doesn't expose a numeric
        //      download id at the Starting phase).
        //   4. Pushes a `DownloadNotice::Started` onto the notice queue.
        //   5. Subscribes `add_StateChanged` on the operation to fire
        //      `Completed` / `Failed` notices.
        //
        // SAFETY: add_DownloadStarting is an STA COM method on `wv4`.
        // The closure captures only Send types (Arc<Downloads>, DownloadNoticeQueue)
        // and is invoked on the STA thread by WebView2.
        if let (Some(ref dl_store), Some(ref dl_queue)) = (downloads, notice_queue)
            && let Ok(wv4) = windows::core::Interface::cast::<
                webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_4,
            >(&webview)
        {
            use webview2_com::DownloadStartingEventHandler;
            use webview2_com::Microsoft::Web::WebView2::Win32::{
                COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED, COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED,
            };

            let dl_store_dl = dl_store.clone();
            let dl_queue_dl = dl_queue.clone();
            let mut dl_token: i64 = 0;

            if let Err(e) = unsafe {
                wv4.add_DownloadStarting(
                        &DownloadStartingEventHandler::create(Box::new(
                            move |_sender, args| {
                                let Some(args) = args else { return Ok(()) };

                                // Obtain the download operation.
                                let op = match args.DownloadOperation() {
                                    Ok(o) => o,
                                    Err(_) => return Ok(()),
                                };

                                // Read URI.
                                let mut uri_pwstr = windows::core::PWSTR::null();
                                let url_str = if op.Uri(&mut uri_pwstr).is_ok()
                                    && !uri_pwstr.is_null()
                                {
                                    let s = uri_pwstr.to_string().unwrap_or_default();
                                    windows::Win32::System::Com::CoTaskMemFree(Some(
                                        uri_pwstr.0.cast(),
                                    ));
                                    s
                                } else {
                                    String::new()
                                };

                                // Read MIME type (optional).
                                let mut mime_pwstr = windows::core::PWSTR::null();
                                let mime_str = if op.MimeType(&mut mime_pwstr).is_ok()
                                    && !mime_pwstr.is_null()
                                {
                                    let s = mime_pwstr.to_string().unwrap_or_default();
                                    windows::Win32::System::Com::CoTaskMemFree(Some(
                                        mime_pwstr.0.cast(),
                                    ));
                                    s
                                } else {
                                    String::new()
                                };

                                // Read total bytes (optional; -1 = unknown).
                                let mut total: i64 = -1;
                                let _ = op.TotalBytesToReceive(&mut total);
                                let total_bytes =
                                    if total > 0 { Some(total as u64) } else { None };

                                // Derive a suggested filename from the URL path.
                                let suggested_name = url_str
                                    .rsplit('/')
                                    .next()
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("download")
                                    .to_owned();

                                tracing::debug!(
                                    url = %url_str,
                                    mime = %mime_str,
                                    ?total_bytes,
                                    "webview2 runtime: DownloadStarting"
                                );

                                // Record start — cef_id=0 (no numeric id
                                // exposed at Starting phase); the store dedupes
                                // by cef_id so repeated calls for the same
                                // download are idempotent.
                                let mime_opt =
                                    if mime_str.is_empty() { None } else { Some(mime_str.as_str()) };
                                let dl_id = dl_store_dl.record_started(
                                    0,
                                    &url_str,
                                    &suggested_name,
                                    mime_opt,
                                    total_bytes,
                                );
                                tracing::debug!(
                                    download_id = ?dl_id,
                                    "webview2 runtime: download recorded"
                                );

                                // Push Started notice.
                                buffr_core::download_notice::push(
                                    &dl_queue_dl,
                                    buffr_core::DownloadNotice {
                                        kind: buffr_core::DownloadNoticeKind::Started,
                                        filename: suggested_name.clone(),
                                        path: String::new(),
                                        created_at: std::time::Instant::now(),
                                    },
                                );

                                // Subscribe StateChanged to push Completed/Failed.
                                let op_state = op.clone();
                                let queue_state = dl_queue_dl.clone();
                                let name_state = suggested_name.clone();
                                let mut state_token: i64 = 0;
                                let _ = op.add_StateChanged(
                                    &webview2_com::StateChangedEventHandler::create(Box::new(
                                        move |_sender, _args| {
                                            let mut state =
                                                webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS;
                                            if op_state.State(&mut state).is_err() {
                                                return Ok(());
                                            }
                                            let kind = if state
                                                == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED
                                            {
                                                // Read the result file path.
                                                let mut path_pwstr =
                                                    windows::core::PWSTR::null();
                                                let path_str = if op_state
                                                    .ResultFilePath(&mut path_pwstr)
                                                    .is_ok()
                                                    && !path_pwstr.is_null()
                                                {
                                                    let s = path_pwstr
                                                        .to_string()
                                                        .unwrap_or_default();
                                                    windows::Win32::System::Com::CoTaskMemFree(
                                                        Some(path_pwstr.0.cast()),
                                                    );
                                                    s
                                                } else {
                                                    String::new()
                                                };
                                                tracing::debug!(
                                                    path = %path_str,
                                                    "webview2 runtime: download completed"
                                                );
                                                buffr_core::download_notice::push(
                                                    &queue_state,
                                                    buffr_core::DownloadNotice {
                                                        kind: buffr_core::DownloadNoticeKind::Completed,
                                                        filename: name_state.clone(),
                                                        path: path_str,
                                                        created_at: std::time::Instant::now(),
                                                    },
                                                );
                                                return Ok(());
                                            } else if state
                                                == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED
                                            {
                                                buffr_core::DownloadNoticeKind::Failed
                                            } else {
                                                return Ok(());
                                            };
                                            tracing::debug!(?kind, "webview2 runtime: download state changed");
                                            buffr_core::download_notice::push(
                                                &queue_state,
                                                buffr_core::DownloadNotice {
                                                    kind,
                                                    filename: name_state.clone(),
                                                    path: String::new(),
                                                    created_at: std::time::Instant::now(),
                                                },
                                            );
                                            Ok(())
                                        },
                                    )),
                                    &mut state_token,
                                );

                                Ok(())
                            },
                        )),
                        &mut dl_token,
                    )
            } {
                tracing::debug!(
                    "webview2 runtime: add_DownloadStarting skipped (older runtime?): {e}"
                );
            }
        }

        // ── AddScriptToExecuteOnDocumentCreated: hint + cursor intercept ─────────
        //
        // Inject two scripts on every document load:
        //
        // 1. Hint-mode console.log wrapper: forwards `__buffr_hint__:…` lines
        //    from the hint JS to Rust via `window.chrome.webview.postMessage`.
        //
        // 2. Cursor-change mousemove listener: on every mousemove where the CSS
        //    cursor changes, emits `__buffr_cursor__:<css-value>` via postMessage.
        //    The WebMessageReceived handler translates the CSS name to a raw CEF
        //    kind integer and stores it in `cursor_change`.
        //
        // Both scripts are registered before the initial navigation so they fire
        // even on the first page.  We ignore the returned IDs (never removed).
        //
        // SAFETY: AddScriptToExecuteOnDocumentCreated is an STA COM method on a
        // valid ICoreWebView2 pointer. The completion handler closure is called on
        // the STA thread by WebView2.
        {
            use windows::core::HSTRING;
            // Script 1: hint console.log forwarder.
            const HINT_INTERCEPT_JS: &str = concat!(
                "(function(){",
                "var _orig=console.log;",
                "console.log=function(msg){",
                "_orig.call(this,msg);",
                "if(typeof msg==='string'&&msg.startsWith('__buffr_hint__:')){",
                "window.chrome.webview.postMessage(msg);",
                "}",
                "};",
                "})();"
            );
            // Script 2: cursor-change mousemove listener.
            const CURSOR_JS: &str = concat!(
                "(function(){",
                "var __buffr_last_cursor=null;",
                "document.addEventListener('mousemove',function(e){",
                "var c=getComputedStyle(e.target).cursor;",
                "if(c!==__buffr_last_cursor){",
                "__buffr_last_cursor=c;",
                "window.chrome.webview.postMessage('__buffr_cursor__:'+c);",
                "}",
                "},true);",
                "})();"
            );
            for (js_src, label) in [
                (HINT_INTERCEPT_JS, "hint console-log interceptor"),
                (CURSOR_JS, "cursor mousemove listener"),
            ] {
                let js_wide = HSTRING::from(js_src);
                let handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(
                    Box::new(|_hr, _id| Ok(())),
                );
                // SAFETY: STA COM method; handler and js_wide are valid for the
                // duration of this call.
                if let Err(e) =
                    unsafe { webview.AddScriptToExecuteOnDocumentCreated(&js_wide, &handler) }
                {
                    tracing::warn!(
                        "webview2 runtime: AddScriptToExecuteOnDocumentCreated ({label}) failed: {e}"
                    );
                } else {
                    tracing::debug!("webview2 runtime: {label} registered");
                }
            }
        }

        // ── WebMessageReceived: hint + cursor + media ─────────────────────────
        //
        // Subscribe to web messages on this tab's webview.  The injected scripts
        // call `window.chrome.webview.postMessage(msg)` for three sentinel types:
        //
        // - `__buffr_hint__:…`    → parse hint event, write to hint_sink.
        // - `__buffr_cursor__:<css>` → translate CSS cursor name to raw kind,
        //                            write (browser_id, kind) to cursor_change.
        // - `__buffr_media__:true/false` → flip video_active atomic.
        //
        // All handlers run on the STA thread; writes to Arc<Mutex<…>> are cheap.
        {
            let mut web_msg_token: i64 = 0;
            // SAFETY: add_WebMessageReceived is an STA COM method.
            // The handler closure captures Send types (Arc<Mutex<…>>, Arc<AtomicBool>)
            // and is invoked on the STA thread by WebView2.
            if let Err(e) = unsafe {
                webview.add_WebMessageReceived(
                    &WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                        let Some(args) = args else { return Ok(()) };
                        // TryGetWebMessageAsString returns a CoTask-allocated PWSTR.
                        let mut msg_pwstr = windows::core::PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut msg_pwstr).is_err()
                            || msg_pwstr.is_null()
                        {
                            return Ok(());
                        }
                        // SAFETY: msg_pwstr is a valid CoTask-allocated PWSTR; convert
                        // then free.
                        let line = msg_pwstr.to_string().unwrap_or_default();
                        windows::Win32::System::Com::CoTaskMemFree(Some(msg_pwstr.0.cast()));

                        if line.starts_with(HINT_CONSOLE_SENTINEL) {
                            match parse_console_event(&line) {
                                Some(Ok(event)) => {
                                    if let Ok(mut guard) = hint_sink.lock() {
                                        *guard = Some(event);
                                    }
                                }
                                Some(Err(e)) => {
                                    tracing::warn!(
                                        "webview2 runtime: malformed hint event JSON: {e}"
                                    );
                                }
                                None => {}
                            }
                        } else if let Some(css) = line.strip_prefix("__buffr_cursor__:") {
                            let raw = super::engine::css_cursor_to_cef_raw(css);
                            if let Ok(mut guard) = cursor_change.lock() {
                                *guard = Some((id.0 as i32, raw));
                            }
                        } else if let Some(val) = line.strip_prefix("__buffr_media__:") {
                            let active = val.trim() == "true";
                            video_active.store(active, Ordering::Relaxed);
                        }
                        Ok(())
                    })),
                    &mut web_msg_token,
                )
            } {
                tracing::warn!("webview2 runtime: add_WebMessageReceived failed: {e}");
            } else {
                tracing::debug!("webview2 runtime: WebMessageReceived handler wired");
            }
        }

        // ── NewWindowRequested (window.open intercept) ────────────────────────
        //
        // When web content calls `window.open(url)` WebView2 fires this event
        // before creating a native popup.  We intercept it, mark it handled
        // (suppressing the popup), and push the URL onto the shared `popup_queue`
        // so the apps layer can open it as a new tab via `drain_popup_urls`.
        //
        // Note: WebView2 handles popup routing as a new-tab re-route here via
        // NewWindowRequested; no OSR sinks are needed for popup frames.
        //
        // SAFETY: add_NewWindowRequested is an STA COM method on `webview`.
        // The handler closure captures `popup_queue` (Arc<Mutex<…>> — Send)
        // and is invoked on the STA thread by WebView2.
        {
            use webview2_com::NewWindowRequestedEventHandler;
            let popup_queue_handler = Arc::clone(&popup_queue);
            let mut new_window_token: i64 = 0;
            if let Err(e) = unsafe {
                webview.add_NewWindowRequested(
                    &NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
                        let Some(args) = args else { return Ok(()) };

                        // Extract the requested URI (CoTask-allocated PWSTR).
                        let mut uri_pwstr = windows::core::PWSTR::null();
                        let url_str = if args.Uri(&mut uri_pwstr).is_ok() && !uri_pwstr.is_null() {
                            // SAFETY: uri_pwstr is a valid CoTask-allocated PWSTR.
                            let s = uri_pwstr.to_string().unwrap_or_default();
                            windows::Win32::System::Com::CoTaskMemFree(Some(uri_pwstr.0.cast()));
                            s
                        } else {
                            String::new()
                        };

                        // Suppress the native popup window.
                        // SAFETY: SetHandled is an STA COM method on a valid args pointer.
                        let _ = args.SetHandled(true);

                        if !url_str.is_empty() {
                            tracing::debug!(
                                tab_id = id.0,
                                url = %url_str,
                                "webview2 runtime: NewWindowRequested → popup_queue"
                            );
                            if let Ok(mut guard) = popup_queue_handler.lock() {
                                guard.push_back(url_str);
                            }
                        }

                        Ok(())
                    })),
                    &mut new_window_token,
                )
            } {
                tracing::warn!("webview2 runtime: add_NewWindowRequested failed: {e}");
            } else {
                tracing::debug!("webview2 runtime: NewWindowRequested handler wired");
            }
        }

        tracing::info!("webview2 runtime: tab {id:?} controller ready, navigating → {url}");

        // ── Initial navigation ────────────────────────────────────────────────
        //
        // SAFETY: Navigate is an STA COM method. `uri_wide` is a valid
        // NUL-terminated wide string; its lifetime covers the Navigate call.
        {
            let uri_wide = url_to_wide(url);
            let pcwstr = windows::core::PCWSTR(uri_wide.as_ptr());
            if let Err(e) = unsafe { webview.Navigate(pcwstr) } {
                tracing::warn!(
                    "webview2 runtime: initial Navigate failed for {url}: HRESULT {:#010x}",
                    e.code().0 as u32
                );
            }
        }

        Ok(TabEntry {
            id,
            url: url.to_owned(),
            title: String::new(),
            is_loading: true,
            can_go_back: false,
            can_go_forward: false,
            progress: 0.0,
            controller,
            webview,
            _hwnd: hwnd,
            _tokens: EventTokens {
                nav_starting: nav_starting_token,
                nav_completed: nav_completed_token,
                source_changed: source_token,
                title_changed: title_token,
                history_changed: history_token,
            },
        })
    }

    /// Non-Windows stub constructor (unreachable — spawn exits before calling this).
    #[cfg(not(target_os = "windows"))]
    pub(crate) fn new(id: TabId, url: &str, engine_state: &Arc<Mutex<EngineState>>) -> Self {
        if let Ok(mut guard) = engine_state.lock() {
            guard.tabs.push(TabInfo {
                id,
                url: url.to_owned(),
                title: String::new(),
                is_loading: true,
                can_go_back: false,
                can_go_forward: false,
                progress: 0.0,
                zoom: 1.0,
            });
        }
        TabEntry {
            id,
            url: url.to_owned(),
            title: String::new(),
            is_loading: true,
            can_go_back: false,
            can_go_forward: false,
            progress: 0.0,
        }
    }

    // ── Navigation helpers ────────────────────────────────────────────────────

    /// Navigate to `url` via `ICoreWebView2::Navigate`.
    ///
    /// The SourceChanged / NavigationStarting / NavigationCompleted events will
    /// update `EngineState` asynchronously once the navigation proceeds.
    pub(crate) fn load_uri(
        &mut self,
        url: &str,
        engine_state: &Arc<Mutex<EngineState>>,
    ) -> Result<(), super::error::WebView2Error> {
        self.url = url.to_owned();
        self.is_loading = true;
        self.sync_to_state(engine_state);

        #[cfg(target_os = "windows")]
        {
            // SAFETY: Navigate is an STA COM method. `uri_wide` lives for the
            // duration of this call.
            let uri_wide = url_to_wide(url);
            let pcwstr = windows::core::PCWSTR(uri_wide.as_ptr());
            unsafe {
                self.webview
                    .Navigate(pcwstr)
                    .map_err(|e| super::error::WebView2Error::ComError(e.code().0 as u32))?;
            }
            tracing::debug!("webview2 runtime: Navigate tab {:?} → {url}", self.id);
        }

        #[cfg(not(target_os = "windows"))]
        tracing::debug!(
            "webview2 runtime: navigate tab {:?} → {url} (stub path)",
            self.id
        );

        Ok(())
    }

    /// Go back via `ICoreWebView2::GoBack`.
    pub(crate) fn go_back(&self) {
        #[cfg(target_os = "windows")]
        // SAFETY: GoBack is an STA COM method on a valid ICoreWebView2.
        if let Err(e) = unsafe { self.webview.GoBack() } {
            tracing::debug!("webview2 runtime: GoBack tab {:?}: {e}", self.id);
        }
        #[cfg(not(target_os = "windows"))]
        tracing::debug!("webview2 runtime: go_back tab {:?} (stub)", self.id);
    }

    /// Go forward via `ICoreWebView2::GoForward`.
    pub(crate) fn go_forward(&self) {
        #[cfg(target_os = "windows")]
        // SAFETY: GoForward is an STA COM method on a valid ICoreWebView2.
        if let Err(e) = unsafe { self.webview.GoForward() } {
            tracing::debug!("webview2 runtime: GoForward tab {:?}: {e}", self.id);
        }
        #[cfg(not(target_os = "windows"))]
        tracing::debug!("webview2 runtime: go_forward tab {:?} (stub)", self.id);
    }

    /// Reload via `ICoreWebView2::Reload`.
    pub(crate) fn reload(&self) {
        #[cfg(target_os = "windows")]
        // SAFETY: Reload is an STA COM method on a valid ICoreWebView2.
        if let Err(e) = unsafe { self.webview.Reload() } {
            tracing::debug!("webview2 runtime: Reload tab {:?}: {e}", self.id);
        }
        #[cfg(not(target_os = "windows"))]
        tracing::debug!("webview2 runtime: reload tab {:?} (stub)", self.id);
    }

    /// Stop loading via `ICoreWebView2::Stop`.
    pub(crate) fn stop(&self) {
        #[cfg(target_os = "windows")]
        // SAFETY: Stop is an STA COM method on a valid ICoreWebView2.
        if let Err(e) = unsafe { self.webview.Stop() } {
            tracing::debug!("webview2 runtime: Stop tab {:?}: {e}", self.id);
        }
        #[cfg(not(target_os = "windows"))]
        tracing::debug!("webview2 runtime: stop tab {:?} (stub)", self.id);
    }

    /// Notify the controller of a viewport resize via
    /// `ICoreWebView2Controller::SetBounds`.
    ///
    /// This ensures the hidden HWND controller tracks the target render
    /// dimensions for future `CapturePreview` (Phase C).
    pub(crate) fn on_resize(&mut self, width: u32, height: u32) {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::Foundation::RECT;
            let bounds = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            // SAFETY: SetBounds is an STA COM method. `bounds` is a valid RECT.
            if let Err(e) = unsafe { self.controller.SetBounds(bounds) } {
                tracing::debug!("webview2 runtime: SetBounds tab {:?}: {e}", self.id);
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (width, height);
        }
    }

    /// Borrow the raw `ICoreWebView2` interface for OSR capture.
    ///
    /// Used by `StaRuntime::paint` to pass the webview pointer to
    /// `osr::request_capture` without transferring ownership.
    #[cfg(target_os = "windows")]
    pub(crate) fn webview(&self) -> &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2 {
        &self.webview
    }

    /// Return the hidden HWND used as the controller parent for this tab.
    ///
    /// Used by the STA worker to `PostMessageW` Win32 input messages (Option A
    /// input dispatch). The HWND is valid for the lifetime of this `TabEntry`.
    #[cfg(target_os = "windows")]
    pub(crate) fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self._hwnd
    }

    /// Back-stack presence from cached state (mirrored by HistoryChanged).
    pub(crate) fn can_go_back(&self) -> bool {
        self.can_go_back
    }

    /// Forward-stack presence from cached state.
    pub(crate) fn can_go_forward(&self) -> bool {
        self.can_go_forward
    }

    /// Mirror current cached state into the shared `EngineState`.
    pub(crate) fn sync_to_state(&self, engine_state: &Arc<Mutex<EngineState>>) {
        if let Ok(mut guard) = engine_state.lock()
            && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == self.id)
        {
            tab.url = self.url.clone();
            tab.title = self.title.clone();
            tab.is_loading = self.is_loading;
            tab.can_go_back = self.can_go_back;
            tab.can_go_forward = self.can_go_forward;
            tab.progress = self.progress;
        }
    }
}

// ── Drop: close the controller ────────────────────────────────────────────────

#[cfg(target_os = "windows")]
impl Drop for TabEntry {
    fn drop(&mut self) {
        // SAFETY: ICoreWebView2Controller::Close is the required cleanup that
        // releases the WebView2 browser process connection. All event tokens are
        // implicitly removed when the controller closes.
        if let Err(e) = unsafe { self.controller.Close() } {
            tracing::debug!("webview2 runtime: Close tab {:?} during Drop: {e}", self.id);
        }
        tracing::debug!("webview2 runtime: tab {:?} controller closed", self.id);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Encode a UTF-8 URL as a NUL-terminated UTF-16 wide string for `PCWSTR`.
#[cfg(target_os = "windows")]
fn url_to_wide(url: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    let mut v: Vec<u16> = OsStr::new(url).encode_wide().collect();
    v.push(0);
    v
}

/// Fetch a favicon URL, decode the image, and push a `FaviconUpdate` with RGBA
/// pixels into `sink`.
///
/// Called on a dedicated background thread spawned by the `FaviconChanged` event
/// handler so the STA thread is never blocked on network I/O.  Mirrors the CEF
/// `BuffrDownloadImageCallback::on_download_image_finished` pixel-decode path.
///
/// Pixels are packed as `0xAA_RR_GG_BB` u32 — the same layout used by CEF and
/// expected by the chrome-strip favicon blitter.
#[cfg(target_os = "windows")]
fn fetch_and_push_favicon(
    url: String,
    browser_id: i32,
    sink: std::sync::Arc<std::sync::Mutex<Vec<buffr_engine::FaviconUpdate>>>,
) {
    use image::GenericImageView;

    tracing::debug!(browser_id, url = %url, "webview2 favicon: fetching");

    let bytes = match ureq::get(&url).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            if let Err(e) = std::io::Read::read_to_end(&mut resp.into_body().as_reader(), &mut buf)
            {
                tracing::debug!(browser_id, error = %e, "webview2 favicon: read failed");
                return;
            }
            buf
        }
        Err(e) => {
            tracing::debug!(browser_id, error = %e, "webview2 favicon: fetch failed");
            return;
        }
    };

    let img = match image::load_from_memory(&bytes) {
        Ok(img) => img,
        Err(e) => {
            tracing::debug!(browser_id, error = %e, "webview2 favicon: decode failed");
            return;
        }
    };

    // Scale down to 32×32 (nearest-neighbour) if larger to keep memory bounded.
    let img = if img.width() > 64 || img.height() > 64 {
        img.resize(32, 32, image::imageops::FilterType::Nearest)
    } else {
        img
    };

    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        return;
    }

    // Convert to RGBA8 and pack as 0xAA_RR_GG_BB u32.
    let rgba = img.to_rgba8();
    let pixels: Vec<u32> = rgba
        .pixels()
        .map(|p| {
            let [r, g, b, a] = p.0;
            ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        })
        .collect();

    tracing::debug!(browser_id, width, height, "webview2 favicon: decoded");

    if let Ok(mut guard) = sink.lock() {
        guard.push(buffr_engine::FaviconUpdate {
            browser_id,
            width,
            height,
            pixels,
        });
    }
}
