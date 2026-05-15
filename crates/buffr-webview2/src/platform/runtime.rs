//! Per-tab WebView2 lifecycle — lives on the STA worker thread.
//!
//! `TabEntry` owns an `ICoreWebView2` composition controller and wires its
//! event delegates back into the shared `Arc<Mutex<EngineState>>` so the
//! engine thread can read URL / title / load state without blocking.
//!
//! # Thread safety
//!
//! `TabEntry` is `!Send` because `ICoreWebView2` is COM apartment-threaded
//! (STA). All `TabEntry` instances are owned by `StaRuntime` which itself
//! runs exclusively on the dedicated STA thread.
//!
//! # Phase B status
//!
//! Phase B stubs the WebView2 COM objects and wires the `EngineState` mirror
//! through in-memory data. Real COM construction (environment, controller,
//! event handlers) is deferred to Phase C, where the STA thread will call
//! `CreateCoreWebView2EnvironmentWithOptions` and then
//! `CreateCoreWebView2CompositionController` for each tab.
//!
//! # TODO markers
//!
//! - TODO(wv2-env): Call `CreateCoreWebView2EnvironmentWithOptions` once on
//!   STA thread startup. Store the resulting `ICoreWebView2Environment` in
//!   `StaRuntime`.
//! - TODO(wv2-tab): Replace the placeholder `TabEntry` with real
//!   `ICoreWebView2CompositionController` + `ICoreWebView2` construction.
//! - TODO(wv2-events): Wire `add_NavigationCompleted`, `add_SourceChanged`,
//!   `add_DocumentTitleChanged`, `add_HistoryChanged` event delegates to
//!   update `EngineState` from the STA thread.
//! - TODO(wv2-probe): On STA startup, call
//!   `CoreWebView2Environment.get_AvailableBrowserVersionString` to confirm
//!   the WebView2 Runtime is installed; return `WebView2Error::InitFailed`
//!   if the runtime is missing.

use std::sync::{Arc, Mutex};

use buffr_engine::TabId;

use super::worker::{EngineState, TabInfo};

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab on the STA thread.
///
/// Phase B: holds metadata only (no live COM objects).
/// Phase C will add `ICoreWebView2CompositionController` + `ICoreWebView2`.
pub(crate) struct TabEntry {
    pub id: TabId,
    /// Current URL (updated by event handlers once wired in Phase C).
    pub url: String,
    /// Current title.
    pub title: String,
    /// Whether the tab is actively loading.
    pub is_loading: bool,
    /// Whether the back-stack is non-empty (wired in Phase C via CanGoBack).
    pub can_go_back: bool,
    /// Whether the forward-stack is non-empty.
    pub can_go_forward: bool,
    /// Load progress 0.0–1.0.
    pub progress: f64,
    // Phase C will add:
    //   pub controller: ICoreWebView2CompositionController,
    //   pub webview: ICoreWebView2,
}

impl TabEntry {
    /// Create a new tab entry and mirror its initial state into `engine_state`.
    ///
    /// Phase C will add COM controller construction here.
    pub(crate) fn new(id: TabId, url: &str, engine_state: &Arc<Mutex<EngineState>>) -> Self {
        // Mirror initial tab info into shared state.
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

        tracing::info!("webview2 runtime: opened tab {id:?} → {url}");
        // TODO(wv2-tab): call CreateCoreWebView2CompositionController here.
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
    //
    // Phase C will delegate to the real ICoreWebView2 COM methods.

    /// Navigate to `url`.
    ///
    /// TODO(wv2-nav): call `self.webview.Navigate(url)` on the STA thread.
    pub(crate) fn load_uri(&mut self, url: &str, engine_state: &Arc<Mutex<EngineState>>) {
        self.url = url.to_owned();
        self.is_loading = true;
        self.sync_to_state(engine_state);
        tracing::debug!(
            "webview2 runtime: navigate tab {:?} → {url} (Phase B no-op)",
            self.id
        );
        // TODO(wv2-nav): self.webview.Navigate(url)?;
    }

    /// Go back one step.
    ///
    /// TODO(wv2-nav): call `self.webview.GoBack()`.
    pub(crate) fn go_back(&self) {
        tracing::debug!(
            "webview2 runtime: go_back tab {:?} (Phase B no-op)",
            self.id
        );
        // TODO(wv2-nav): self.webview.GoBack()?;
    }

    /// Go forward one step.
    ///
    /// TODO(wv2-nav): call `self.webview.GoForward()`.
    pub(crate) fn go_forward(&self) {
        tracing::debug!(
            "webview2 runtime: go_forward tab {:?} (Phase B no-op)",
            self.id
        );
        // TODO(wv2-nav): self.webview.GoForward()?;
    }

    /// Reload the current page.
    ///
    /// TODO(wv2-nav): call `self.webview.Reload()`.
    pub(crate) fn reload(&self) {
        tracing::debug!("webview2 runtime: reload tab {:?} (Phase B no-op)", self.id);
        // TODO(wv2-nav): self.webview.Reload()?;
    }

    /// Stop loading.
    ///
    /// TODO(wv2-nav): call `self.webview.Stop()`.
    pub(crate) fn stop(&self) {
        tracing::debug!("webview2 runtime: stop tab {:?} (Phase B no-op)", self.id);
        // TODO(wv2-nav): self.webview.Stop()?;
    }

    /// Whether the back-stack is non-empty.
    ///
    /// Phase B reads from the cached state. Phase C will query CanGoBack.
    pub(crate) fn can_go_back(&self) -> bool {
        self.can_go_back
    }

    /// Whether the forward-stack is non-empty.
    pub(crate) fn can_go_forward(&self) -> bool {
        self.can_go_forward
    }

    /// Mirror current entry state into the shared `EngineState`.
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
