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

use std::sync::{Arc, Mutex};

use buffr_engine::TabId;

use super::worker::{EngineState, TabInfo};

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
    controller: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
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
    pub(crate) fn new(
        id: TabId,
        url: &str,
        engine_state: &Arc<Mutex<EngineState>>,
        environment: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
    ) -> Result<Self, WebView2Error> {
        use webview2_com::{
            CreateCoreWebView2ControllerCompletedHandler, DocumentTitleChangedEventHandler,
            HistoryChangedEventHandler, NavigationCompletedEventHandler,
            NavigationStartingEventHandler, SourceChangedEventHandler,
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
                        if let Ok(mut guard) = state_nav_starting.lock()
                            && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                        {
                            tab.is_loading = true;
                            tab.progress = 0.0;
                        }
                        Ok(())
                    })),
                    &mut nav_starting_token,
                )
                .map_err(|e| WebView2Error::ComError(e.code().0 as u32))?;
        }

        // ── NavigationCompleted ───────────────────────────────────────────────
        // SAFETY: same invariants as NavigationStarting above.
        unsafe {
            webview
                .add_NavigationCompleted(
                    &NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
                        if let Ok(mut guard) = state_nav_completed.lock()
                            && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                        {
                            tab.is_loading = false;
                            tab.progress = 1.0;
                        }
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
        // `Interface::cast` is a safe method (QueryInterface via the vtable).
        // Failure means the runtime is older and we proceed without favicon events.
        if let Ok(wv15) = windows::core::Interface::cast::<
            webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_15,
        >(&webview)
        {
            use webview2_com::FaviconChangedEventHandler;
            let state_favicon = Arc::clone(engine_state);
            let mut favicon_token: i64 = 0;
            // SAFETY: add_FaviconChanged is an STA COM method on `wv15`.
            // The handler closure captures only Arc<Mutex<EngineState>>.
            if let Err(e) = unsafe {
                wv15.add_FaviconChanged(
                    &FaviconChangedEventHandler::create(Box::new(move |_sender, _args| {
                        // Phase B-2: acknowledge the event to keep EngineState dirty flag.
                        // TODO(phase-c): read FaviconUri from ICoreWebView2_15 and store it.
                        if let Ok(_guard) = state_favicon.lock() {
                            // No-op: favicon byte retrieval is Phase C.
                        }
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
