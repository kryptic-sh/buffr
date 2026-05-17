//! [`BrowserEngine`] — engine-agnostic browser trait.
//!
//! # PHASE-1 SCOPE:
//!
//! This trait covers the core surface that `apps/buffr-app` calls through
//! the abstraction — lifecycle, tabs, navigation, viewport, neutral input,
//! OSR state, find/zoom, and audio/video activity.
//!
//! **Not in Phase 1** (stays as inherent methods on `BrowserHost`):
//!   - `dispatch(action: &buffr_modal::PageAction)` — modal action routing
//!   - favicon accessors (`favicon_sink`, `favicon_enabled`, etc.)
//!   - permissions accessors (`permissions`, `permissions_queue`)
//!   - context-menu drain (`drain_context_menu_requests`)
//!   - audio event queue drain (`drain_audio_events`)
//!   - edit / clipboard helpers
//!   - media probe helpers
//!
//! Phase 6a (#95): popup_* family added to the trait so `apps/buffr-app`
//! no longer needs the `cef_engines` parallel map for popup calls.
//!
//! Phase 6b (#95): hint_* family added so hint-mode key routing goes
//! through `Arc<dyn BrowserEngine>` instead of the CEF-specific
//! `active_host()` reach-through.
//!
//! Phase 6c (#95): frame editing, media, JS execution, run_edit_*,
//! run_media_probe, image_rotate, start_download, and show_dev_tools_at
//! added so `apps/buffr-app` no longer needs `active_host()` reach-
//! through for these families. Methods that are CEF-specific or not yet
//! implemented by CDP have default impls that return
//! `EngineError::Unimplemented` (Result-returning) or emit a debug log
//! and no-op (void). This avoids forcing every backend to write stubs.
//!
//! Phase 2 will widen the trait as the apps layer is further decoupled.

use std::sync::Arc;

use crate::{
    AudioEvent, ClipboardReader, ContextMenuRequest, EngineError, FaviconUpdate, HintAction,
    HintStatus, MouseButton, NativeRect, NeutralKeyEvent, PermissionsQueue, PromptOutcome,
    SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
    popup::{PopupCloseSink, PopupCreateSink, PopupQueue},
};

/// Engine-agnostic browser abstraction.
///
/// `buffr-cef` implements this via `impl BrowserEngine for BrowserHost`.
/// Future backends (e.g. `buffr-servo`) provide their own impl.
///
/// All methods are `&self` — the concrete type uses interior mutability
/// (`Arc<Mutex<…>>`) for any mutable state, matching the existing
/// `BrowserHost` design.
pub trait BrowserEngine: Send + Sync {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    /// Close every live browser. Call before `cef::shutdown()`.
    fn close_all_browsers(&self);

    // ── Tabs ─────────────────────────────────────────────────────────────────

    /// Open a new foreground tab loading `url`.
    fn open_tab(&self, url: &str) -> Result<TabId, EngineError>;

    /// Open a new background tab loading `url`. The active tab does not change.
    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError>;

    /// Open a new tab at `insert_idx` in the strip. The new tab becomes active.
    fn open_tab_at(&self, url: &str, insert_idx: usize) -> Result<TabId, EngineError>;

    /// Close the tab with `id`. Returns `true` when more tabs remain.
    fn close_tab(&self, id: TabId) -> Result<bool, EngineError>;

    /// Close the active tab. Returns `true` when more tabs remain.
    fn close_active(&self) -> Result<bool, EngineError>;

    /// Switch to the tab with `id`. No-op when not found.
    fn select_tab(&self, id: TabId);

    /// Cycle to the next tab (wraps).
    fn next_tab(&self);

    /// Cycle to the previous tab (wraps).
    fn prev_tab(&self);

    /// Move the tab at `from` to position `to`.
    fn move_tab(&self, from: usize, to: usize);

    /// Duplicate the active tab. Returns the new tab's [`TabId`].
    fn duplicate_active(&self) -> Result<TabId, EngineError>;

    /// Toggle the pinned bit on the active tab.
    fn toggle_pin_active(&self);

    /// Set the pinned bit on the tab with `id`.
    fn set_pinned(&self, id: TabId, pinned: bool);

    /// Pop the most recently closed tab off the undo stack.
    /// Returns `Ok(None)` when the stack is empty.
    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError>;

    /// Number of entries on the closed-tab undo stack.
    fn closed_stack_len(&self) -> usize;

    /// Snapshot of the active tab, if any.
    fn active_tab(&self) -> Option<TabSummary>;

    /// Snapshot of every tab in strip order.
    fn tabs_summary(&self) -> Vec<TabSummary>;

    /// Number of open tabs.
    fn tab_count(&self) -> usize;

    /// Number of pinned tabs.
    fn pinned_count(&self) -> usize;

    /// Index of the active tab in [`Self::tabs_summary`] ordering.
    fn active_index(&self) -> Option<usize>;

    // ── Navigation / address ─────────────────────────────────────────────────

    /// Navigate the active tab to `url`.
    fn navigate(&self, url: &str) -> Result<(), EngineError>;

    /// Cached main-frame URL of the active tab. Empty when no active tab.
    fn active_tab_live_url(&self) -> String;

    /// Drain queued `on_address_change` events. Returns `true` when at
    /// least one URL changed.
    fn pump_address_changes(&self) -> bool;

    // ── Viewport ─────────────────────────────────────────────────────────────

    /// Notify the engine that every browser's viewport changed to `(width, height)`.
    fn resize(&self, width: u32, height: u32);

    /// Update the device scale factor and notify CEF to re-query screen info.
    fn set_device_scale(&self, scale: f32);

    /// Set the target frame rate in Hz for all browsers.
    fn set_frame_rate(&self, hz: u32);

    /// Notify CEF that the screen / monitor info changed.
    fn notify_screen_info_changed(&self);

    /// Notify CEF the OSR viewport resized to `(width, height)`.
    fn osr_resize(&self, width: u32, height: u32);

    // ── Input — neutral types only ────────────────────────────────────────────

    /// Forward a keyboard event to the active tab.
    fn osr_key_event(&self, event: NeutralKeyEvent);

    /// Forward a mouse-move to the active tab.
    fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32);

    /// Forward a mouse-click to the active tab.
    fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    );

    /// Notify CEF the mouse left the window.
    fn osr_mouse_leave(&self, modifiers: u32);

    /// Forward a mouse-wheel event to the active tab.
    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32);

    /// Notify CEF of focus changes.
    fn osr_focus(&self, focused: bool);

    // ── OSR state ────────────────────────────────────────────────────────────

    /// Clone the shared OSR frame buffer handle.
    fn osr_frame(&self) -> SharedOsrFrame;

    /// Clone the shared OSR viewport state handle.
    fn osr_view(&self) -> SharedOsrViewState;

    /// Aggressive force-repaint of the active tab.
    fn force_repaint_active(&self);

    /// Put the active tab to sleep or wake it (`was_hidden`).
    fn osr_sleep(&self, sleep: bool);

    /// Nudge CEF to deliver a fresh paint after waking.
    fn osr_invalidate_view(&self);

    /// Install a wake callback fired on every `on_paint`. First setter wins.
    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>);

    // ── Find / zoom ──────────────────────────────────────────────────────────

    /// Begin a find session on the active tab.
    fn start_find(&self, query: &str, forward: bool);

    /// Cancel the active tab's find session.
    fn stop_find(&self);

    /// Active tab's zoom level. Returns `1.0` (100 %) when no active tab or
    /// zoom has not been changed from the default.
    fn active_zoom_level(&self) -> f64;

    /// Increase zoom by one step on the active tab.
    ///
    /// Default no-op — backends that support zoom override this.
    fn zoom_in(&self) {}

    /// Decrease zoom by one step on the active tab.
    ///
    /// Default no-op — backends that support zoom override this.
    fn zoom_out(&self) {}

    /// Reset zoom to 100 % on the active tab.
    ///
    /// Default no-op — backends that support zoom override this.
    fn zoom_reset(&self) {}

    // ── DevTools ─────────────────────────────────────────────────────────────

    /// Open the developer tools panel/window for the given tab.
    ///
    /// Default no-op: backends that don't support it ignore the call.
    /// Returns `Ok(())` even when stubbed so the apps layer doesn't
    /// need to special-case missing capability.
    fn open_devtools(&self, _tab: TabId) -> Result<(), EngineError> {
        Ok(())
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    /// `true` when at least one browser has an active audio stream.
    fn any_audio_active(&self) -> bool;

    /// `true` when the last JS media probe reported a video signal active.
    fn any_video_active(&self) -> bool;

    // ── Popup sinks ──────────────────────────────────────────────────────────
    //
    // Phase 6a (#95): popup_* family promoted to the trait so the apps layer
    // can reach popup operations through `Arc<dyn BrowserEngine>` without a
    // CEF-specific reach-through map.
    //
    // Backends that do not support popups (e.g. `buffr-blink-cdp`) return
    // empty sinks / Vec or no-op, and log a debug note where appropriate.
    // CDP popup support: future work, see #95.

    /// Clone the popup URL queue (new-tab reroutes from `on_before_popup`).
    fn popup_queue(&self) -> PopupQueue;

    /// Clone the popup-create sink so the apps layer can drain it.
    fn popup_create_sink(&self) -> PopupCreateSink;

    /// Clone the popup-close sink so the apps layer can drain it.
    fn popup_close_sink(&self) -> PopupCloseSink;

    /// Notify the engine that a popup browser has been resized.
    fn popup_resize(&self, browser_id: i32, width: u32, height: u32);

    /// Request the engine to close a popup browser (async; teardown arrives
    /// via `popup_close_sink`).
    fn popup_close(&self, browser_id: i32);

    /// Drain address-change events for popup browsers.
    fn popup_drain_address_changes(&self) -> Vec<(i32, String)>;

    /// Drain title-change events for popup browsers.
    fn popup_drain_title_changes(&self) -> Vec<(i32, String)>;

    /// Navigate a popup browser back in its own history.
    fn popup_history_back(&self, browser_id: i32);

    /// Navigate a popup browser forward in its own history.
    fn popup_history_forward(&self, browser_id: i32);

    /// Set focus on a popup browser.
    fn popup_osr_focus(&self, browser_id: i32, focused: bool);

    /// Forward a keyboard event to a popup browser.
    fn popup_osr_key_event(&self, browser_id: i32, event: NeutralKeyEvent);

    /// Forward a mouse-click to a popup browser.
    #[allow(clippy::too_many_arguments)]
    fn popup_osr_mouse_click(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        button: MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    );

    /// Forward a mouse-move to a popup browser.
    fn popup_osr_mouse_move(&self, browser_id: i32, x: i32, y: i32, modifiers: u32);

    /// Forward a mouse-wheel event to a popup browser.
    fn popup_osr_mouse_wheel(
        &self,
        browser_id: i32,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    );

    // ── Hint mode (Phase 6b, #95) ─────────────────────────────────────────────
    //
    // Backends that do not support hint mode (e.g. `buffr-blink-cdp`) return
    // no-op / idle stubs. CDP hint mode: future work, see #95.

    /// Whether the active tab has a live hint session.
    fn is_hint_mode(&self) -> bool;

    /// Status snapshot of the active tab's hint session.
    ///
    /// Returns `None` when no session is active or no active tab exists.
    fn hint_status(&self) -> Option<HintStatus>;

    /// Drain renderer-side hint events and finalise the active tab's session.
    ///
    /// Returns `true` if the session state changed (Ready / Error received).
    fn pump_hint_events(&self) -> bool;

    /// Feed a printable character to the active tab's hint session.
    ///
    /// Returns `None` when no session is active.
    fn feed_hint_key(&self, c: char) -> Option<HintAction>;

    /// Backspace the typed buffer in the active tab's hint session.
    ///
    /// Returns `None` when no session is active.
    fn backspace_hint(&self) -> Option<HintAction>;

    /// Cancel the active tab's hint session and remove all overlays.
    fn cancel_hint(&self);

    // ── Frame editing (Phase 6c, #95) ────────────────────────────────────────
    //
    // These methods forward CEF frame-level edit commands (undo, redo, cut,
    // copy, paste, paste-as-plain-text, select-all) to the focused frame in
    // the active tab.  They are void (no return value) because CEF does not
    // surface per-command errors.
    //
    // Default impls emit a `tracing::debug!` and no-op so backends that don't
    // support frame-level editing (e.g. blink-cdp in the initial phases) don't
    // need to write 7 explicit stubs.

    /// Send Undo to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op (backends that don't support it override).
    fn frame_undo(&self) {
        tracing::debug!("BrowserEngine::frame_undo: not implemented by this backend");
    }

    /// Send Redo to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op.
    fn frame_redo(&self) {
        tracing::debug!("BrowserEngine::frame_redo: not implemented by this backend");
    }

    /// Send Cut to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op.
    fn frame_cut(&self) {
        tracing::debug!("BrowserEngine::frame_cut: not implemented by this backend");
    }

    /// Send Copy to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op.
    fn frame_copy(&self) {
        tracing::debug!("BrowserEngine::frame_copy: not implemented by this backend");
    }

    /// Send Paste to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op.
    fn frame_paste(&self) {
        tracing::debug!("BrowserEngine::frame_paste: not implemented by this backend");
    }

    /// Send Paste-as-plain-text to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op.
    fn frame_paste_plain(&self) {
        tracing::debug!("BrowserEngine::frame_paste_plain: not implemented by this backend");
    }

    /// Send Select-All to the active tab's focused frame.
    ///
    /// Default: debug-log and no-op.
    fn frame_select_all(&self) {
        tracing::debug!("BrowserEngine::frame_select_all: not implemented by this backend");
    }

    // ── Media (Phase 6c, #95) ────────────────────────────────────────────────
    //
    // These helpers inject JS into the active tab's main frame to manipulate
    // the media element at `(x, y)` (browser-local CSS pixels, from the
    // context-menu request). They are CEF-specific (context-menu params supply
    // the hit coordinates) so the default impls no-op.

    /// Toggle play/pause on the media element under `(x, y)`.
    ///
    /// Default: debug-log and no-op.
    fn media_play_pause(&self, _x: i32, _y: i32) {
        tracing::debug!("BrowserEngine::media_play_pause: not implemented by this backend");
    }

    /// Request Picture-in-Picture for the media element under `(x, y)`.
    ///
    /// Default: debug-log and no-op.
    fn media_picture_in_picture(&self, _x: i32, _y: i32) {
        tracing::debug!("BrowserEngine::media_picture_in_picture: not implemented by this backend");
    }

    /// Toggle `.controls` on the media element under `(x, y)`.
    ///
    /// Default: debug-log and no-op.
    fn media_toggle_controls(&self, _x: i32, _y: i32) {
        tracing::debug!("BrowserEngine::media_toggle_controls: not implemented by this backend");
    }

    /// Toggle `.loop` on the media element under `(x, y)`.
    ///
    /// Default: debug-log and no-op.
    fn media_toggle_loop(&self, _x: i32, _y: i32) {
        tracing::debug!("BrowserEngine::media_toggle_loop: not implemented by this backend");
    }

    /// Toggle `.muted` on the media element under `(x, y)`.
    ///
    /// Default: debug-log and no-op.
    fn media_toggle_mute(&self, _x: i32, _y: i32) {
        tracing::debug!("BrowserEngine::media_toggle_mute: not implemented by this backend");
    }

    // ── Image (Phase 6c, #95) ────────────────────────────────────────────────

    /// Rotate the `<img>` element under `(x, y)` by `delta_deg` degrees.
    ///
    /// Positive `delta_deg` = clockwise, negative = counter-clockwise.
    /// Accumulates into `el.dataset.buffrRotate` so successive rotations compose.
    ///
    /// Default: debug-log and no-op.
    fn image_rotate(&self, _x: i32, _y: i32, _delta_deg: i32) {
        tracing::debug!("BrowserEngine::image_rotate: not implemented by this backend");
    }

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────
    //
    // Fire-and-forget JS execution. No synchronous return value is surfaced
    // (CEF exposes no `StringVisitor` for `execute_java_script`; see host.rs
    // doc comment for rationale).
    //
    // Default impls return `Err(Unimplemented)` so the apps layer can log
    // and continue rather than panicking on unsupported backends.

    /// Execute `code` on the active tab's main frame.
    ///
    /// Default: returns `Err(EngineError::Unimplemented)`.
    fn run_js(&self, _code: &str) -> Result<(), EngineError> {
        Err(EngineError::Unimplemented { method: "run_js" })
    }

    /// Execute `code` on the active tab's main frame, attributed to `url`.
    ///
    /// The `url` is used as the script origin in DevTools / error stacks.
    ///
    /// Default: returns `Err(EngineError::Unimplemented)`.
    fn run_main_frame_js(&self, _code: &str, _url: &str) -> Result<(), EngineError> {
        Err(EngineError::Unimplemented {
            method: "run_main_frame_js",
        })
    }

    // ── Edit IPC helpers (Phase 6c, #95) ─────────────────────────────────────
    //
    // Sentinel-driven IPC with the JS edit layer (`edit.js`). The JS side
    // exposes `window.__buffrEditAttach`, `__buffrEditDetach`, etc. which
    // are called via `run_main_frame_js` under the hood.
    //
    // Default impls no-op + debug log. CEF implements these via its inherent
    // counterparts; CDP will implement them in a future phase.

    /// Add the edit-active CSS class to `field_id` via `__buffrEditAttach`.
    ///
    /// Default: debug-log and no-op.
    fn run_edit_attach(&self, _field_id: &str) {
        tracing::debug!("BrowserEngine::run_edit_attach: not implemented by this backend");
    }

    /// Move focus to the next (`forward=true`) or previous visible input.
    ///
    /// Default: debug-log and no-op.
    fn run_edit_cycle(&self, _forward: bool) {
        tracing::debug!("BrowserEngine::run_edit_cycle: not implemented by this backend");
    }

    /// Remove the edit-active CSS class from `field_id` via `__buffrEditDetach`.
    ///
    /// Default: debug-log and no-op.
    fn run_edit_detach(&self, _field_id: &str) {
        tracing::debug!("BrowserEngine::run_edit_detach: not implemented by this backend");
    }

    /// Re-focus `field_id` by its buffr-assigned ID via `__buffrEditFocus`.
    ///
    /// Default: debug-log and no-op.
    fn run_edit_focus(&self, _field_id: &str) {
        tracing::debug!("BrowserEngine::run_edit_focus: not implemented by this backend");
    }

    /// Fire the JS media-activity poll against the active tab's main frame.
    ///
    /// The poll writes a boolean to `window.__buffr_media_active` which a
    /// follow-up read can consume. See [`BrowserHost::run_media_probe`] for
    /// the full rationale.
    ///
    /// Default: debug-log and no-op.
    fn run_media_probe(&self) {
        tracing::debug!("BrowserEngine::run_media_probe: not implemented by this backend");
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    /// Trigger a file-as-download for `url` via the engine's download manager.
    ///
    /// Default: debug-log and no-op (CEF implements via
    /// `CefBrowserHost::StartDownload`; CDP download support is future work).
    fn start_download(&self, _url: &str) {
        tracing::debug!("BrowserEngine::start_download: not implemented by this backend");
    }

    // ── DevTools at point (Phase 6c, #95) ────────────────────────────────────

    /// Open DevTools for the active tab, focused on the element at `(x, y)`.
    ///
    /// Backends that support inspect-element use the coordinates to highlight
    /// the element in the Elements panel. Backends that don't support
    /// inspect-element (e.g. blink-cdp, which always opens the full inspector)
    /// ignore `x`/`y` and delegate to their existing `open_devtools` impl.
    ///
    /// Default: debug-log and no-op. The CEF backend overrides with
    /// `show_dev_tools_at(Some(x), Some(y))`. The blink-cdp backend opens
    /// the full inspector (ignoring coordinates).
    fn show_dev_tools_at(&self, _x: i32, _y: i32) {
        tracing::debug!("BrowserEngine::show_dev_tools_at: not implemented by this backend");
    }

    // ── Clipboard (Phase 6d, #95) ─────────────────────────────────────────────
    //
    // Neutral clipboard surface. CEF wraps `hjkl_clipboard::Clipboard`;
    // CDP and other backends return no-op defaults until implemented.
    //
    // `clipboard_handle` hands out a `ClipboardReader` (`Arc<dyn ClipboardRead>`)
    // so the apps layer can do blocking reads on a worker thread without pulling
    // in the backend clipboard crate directly, and without deadlocking the CEF
    // UI thread (Wayland `wl_data_source.send` runs on the main thread).

    /// Clone the clipboard reader handle for off-thread reads.
    ///
    /// Returns `None` when the backend has no system clipboard (e.g. CDP
    /// in Phase 6d, or when clipboard initialisation failed at startup).
    ///
    /// Default: `None`.
    fn clipboard_handle(&self) -> Option<ClipboardReader> {
        None
    }

    /// Read the current system clipboard contents as UTF-8 text.
    ///
    /// Convenience wrapper — prefer [`Self::clipboard_handle`] when reads
    /// must happen off-thread. Returns `None` when the clipboard is empty,
    /// non-text, or unavailable.
    ///
    /// Default: `None`.
    fn clipboard_text(&self) -> Option<String> {
        None
    }

    /// Write `text` to the system clipboard. Returns `true` on success.
    ///
    /// Default: debug-log and return `false` (CEF backend overrides).
    fn clipboard_set_text(&self, _text: &str) -> bool {
        tracing::debug!("BrowserEngine::clipboard_set_text: not implemented by this backend");
        false
    }

    /// Spawn an off-thread worker that fetches `url`, decodes the image,
    /// re-encodes it as PNG, and writes it to the system clipboard.
    ///
    /// Used by the right-click "Copy Image" menu item. Returns immediately;
    /// the outcome surfaces via `tracing`.
    ///
    /// Default: debug-log and no-op (CEF backend overrides via
    /// `buffr_core::image_copy::copy_image_to_clipboard`).
    fn copy_image_url_to_clipboard(&self, _url: &str) {
        tracing::debug!(
            "BrowserEngine::copy_image_url_to_clipboard: not implemented by this backend"
        );
    }

    // ── Audio events (Phase 6d, #95) ─────────────────────────────────────────
    //
    // Drain the per-engine audio edge-event queue. The apps layer calls this
    // each tick across all engines and recomputes `media_active`.

    /// Drain all pending [`AudioEvent`]s that the backend has queued since
    /// the last call. Returns an empty `Vec` when no audio events are pending
    /// or the backend has no audio tracking (e.g. blink-cdp).
    ///
    /// Default: `Vec::new()`.
    fn drain_audio_events(&self) -> Vec<AudioEvent> {
        Vec::new()
    }

    // ── Context menu (Phase 6d, #95) ─────────────────────────────────────────

    /// Drain all pending [`ContextMenuRequest`]s queued by the backend since
    /// the last call. The apps layer shows the most-recent one as an overlay.
    ///
    /// Default: `Vec::new()`.
    fn drain_context_menu_requests(&self) -> Vec<ContextMenuRequest> {
        Vec::new()
    }

    // ── Cursor (Phase 6d, #95) ────────────────────────────────────────────────
    //
    // Returns a pending cursor change as `(browser_id, raw_kind)` and clears
    // the dirty flag. The `raw_kind` is the platform-raw discriminant from
    // CEF's `CursorType` enum (a `u32`); the apps layer maps it to
    // `winit::CursorIcon` via `cef_cursor_type_to_winit`.
    //
    // Returning the raw pair rather than a `SharedCursorState` handle keeps
    // `buffr-engine` free of any `buffr-core` dependency (which would be
    // circular, since `buffr-core` depends on `buffr-engine`).

    /// Take a pending cursor change, if any, and clear the dirty flag.
    ///
    /// Returns `Some((browser_id, raw_kind))` when a new cursor has arrived
    /// since the last call, `None` otherwise. Last writer wins — CEF
    /// coalesces multiple changes per frame into one slot.
    ///
    /// Default: `None` (CDP and other backends without cursor tracking).
    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        None
    }

    // ── IME composition (Phase 8d, #86) ─────────────────────────────────────────
    //
    // Three-call lifecycle matching both CEF's BrowserHost IME API and the
    // Chrome DevTools Protocol `Input.imeSetComposition` / `Input.insertText`
    // commands:
    //
    //  1. `ime_set_composition` — called repeatedly while the user is composing
    //     (winit `Ime::Preedit`).
    //  2. `ime_commit` — called once when the user accepts the composition
    //     (winit `Ime::Commit`).
    //  3. `ime_cancel` — called when the IME is disabled or the window loses
    //     focus (winit `Ime::Disabled` or `Ime::Enabled` without a pending
    //     commit).
    //
    // All three have default no-op bodies so backends that do not yet support
    // IME (e.g. future backends) compile without stubs.

    /// Update IME composition state.  Called during pre-edit.
    ///
    /// `cursor` is a `(start, end)` byte range within `text` for the
    /// composition selection, or `None` to indicate no active selection.
    ///
    /// Default: no-op (backends override for their platform IME API).
    fn ime_set_composition(&self, _text: &str, _cursor: Option<(usize, usize)>) {}

    /// Commit text to the focused editable.  Called when composition is accepted.
    ///
    /// Default: no-op.
    fn ime_commit(&self, _text: &str) {}

    /// Cancel the current composition.  Called when IME is disabled or the
    /// window blurs while a composition is in progress.
    ///
    /// Default: no-op.
    fn ime_cancel(&self) {}

    // ── Permissions (Phase 8a, #88) ───────────────────────────────────────────
    //
    // Both the CEF backend and blink-cdp push neutral `PendingPermission`
    // entries onto the shared queue.  When the user answers, apps calls
    // `resolve_permission` so the backend can fire the appropriate callback
    // (CEF C++ callback or CDP Runtime.evaluate promise resolution).
    //
    // CEF: wraps existing callback registry; blink-cdp: sends
    // `__buffrPermissionResolve(id, outcome)` via Runtime.evaluate.
    // Default no-op: backends that don't implement permissions return an
    // empty queue and ignore resolve calls.

    /// Clone the shared permissions queue for this engine.
    ///
    /// The apps layer drains one entry per tick and shows the prompt strip.
    ///
    /// Default: returns a fresh empty queue (backends that don't surface
    /// permission requests return no-op here). CEF and blink-cdp override
    /// with their respective shared queues.
    fn permissions_queue(&self) -> PermissionsQueue {
        crate::permissions::new_queue()
    }

    /// Resolve the pending permission identified by `resolve_id` with
    /// `outcome`.  The backend fires the appropriate C++ callback (CEF) or
    /// resolves the JS promise (blink-cdp).
    ///
    /// `resolve_id` is taken from [`PendingPermission::resolve_id`].
    /// `None` means the entry has no async backend state to clean up
    /// (future synchronous backends).
    ///
    /// Default: no-op (backends that don't implement permissions ignore this).
    fn resolve_permission(&self, _resolve_id: Option<&str>, _outcome: PromptOutcome) {}

    // ── Favicon (Phase 6d, #95) ───────────────────────────────────────────────

    /// Drain all pending [`FaviconUpdate`]s delivered by the backend since
    /// the last call. The apps layer caches the bitmaps by browser id for
    /// the tab strip.
    ///
    /// Default: `Vec::new()` (CDP and other backends without favicon tracking).
    fn drain_favicon_updates(&self) -> Vec<FaviconUpdate> {
        Vec::new()
    }

    // ── Loading state / navigation capability (Phase 6f, #95) ───────────────

    /// `true` when the active tab is currently loading a document.
    ///
    /// CEF overrides this with an atomic set by `on_load_start` / `on_paint`
    /// for tighter accuracy. The default impl delegates to `active_tab()` so
    /// non-CEF backends get a reasonable answer without overriding.
    fn is_loading(&self) -> bool {
        self.active_tab().is_some_and(|t| t.is_loading)
    }

    /// `true` when the active tab can navigate backwards.
    ///
    /// Used to drive the enabled state of back-navigation UI (e.g.
    /// context-menu `HistoryBack { enabled }`).
    ///
    /// Default: `false` — backends that track history accurately override this.
    /// CEF overrides via `CefBrowser::CanGoBack`; blink-cdp uses its nav_count
    /// heuristic. Defaulting to `true` was wrong: a newly-opened tab has no
    /// history, so the back-button would incorrectly appear enabled.
    fn can_go_back(&self) -> bool {
        false
    }

    /// `true` when the active tab can navigate forwards.
    ///
    /// Default: `false` (same safe default as [`Self::can_go_back`]).
    fn can_go_forward(&self) -> bool {
        false
    }

    // ── Action dispatch (Phase 6f, #95) ──────────────────────────────────────
    //
    // Forward a [`buffr_modal::PageAction`] to the engine.
    //
    // CEF's `BrowserHost::dispatch` handles the full action surface.
    // Non-CEF backends override only the arms they support; the default
    // no-op means unhandled actions are silently swallowed (matching the
    // prior behaviour when the CEF-only path was gated by a downcast).

    /// Dispatch `action` to the engine.
    ///
    /// Default: debug-log and no-op (CEF backend overrides with the full
    /// `BrowserHost::dispatch` implementation).
    fn dispatch(&self, _action: &buffr_modal::PageAction) {
        tracing::debug!("BrowserEngine::dispatch: not implemented by this backend");
    }

    // ── Native compositing (Phase 3, #109) ───────────────────────────────────
    //
    // Three-method API surface for backends that can render directly into a
    // host-provided native surface (Wayland wl_surface today; X11 / Cocoa
    // later). Default is "OSR-only": all three are no-ops, so existing
    // backends compile + behave unchanged.

    /// `true` when the backend supports native compositing into a host-
    /// provided surface. When true, the apps layer should:
    ///   1. Call [`Self::set_native_parent`] with a `RawWindowHandle` to the
    ///      host's surface (e.g. winit's `wl_surface`) and the browser rect
    ///      within it.
    ///   2. Skip the OSR drain in its render loop — engine pixels arrive
    ///      directly via the platform-native compositor.
    ///   3. Render its chrome with alpha (transparent over the browser
    ///      region) so the engine's native surface shows through.
    ///   4. Call [`Self::set_native_visible`] on tab activation / window
    ///      occlusion.
    ///
    /// Default: `false` (CEF and other backends without native compositing).
    fn supports_native(&self) -> bool {
        false
    }

    /// Bind the engine to a parent native surface + viewport rect within it.
    ///
    /// `parent` is a [`raw_window_handle::RawWindowHandle`] — Wayland's
    /// `WaylandWindowHandle::surface` carries the host `wl_surface`. The
    /// engine creates a child surface (Wayland subsurface, X11 child window,
    /// etc.) positioned at `rect` and renders into it.
    ///
    /// Calling again with a new parent or rect repositions / re-parents.
    ///
    /// Default: no-op.
    fn set_native_parent(
        &self,
        _parent: raw_window_handle::RawWindowHandle,
        _rect: NativeRect,
    ) {
    }

    /// Show or hide the engine's native surface attachment without tearing
    /// it down. Used by the apps layer for tab switching: only the active
    /// tab's native surface should be visible; others stay attached but
    /// hidden so reactivation is instant.
    ///
    /// Default: no-op.
    fn set_native_visible(&self, _visible: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::{
        EngineError, HintAction, HintStatus, MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState,
        TabId, TabSummary,
        popup::{
            PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink,
            new_popup_create_sink, new_popup_queue,
        },
    };

    /// Minimal no-op stub that compiles the required methods.
    struct NoOpEngine;

    impl BrowserEngine for NoOpEngine {
        fn close_all_browsers(&self) {}
        fn open_tab(&self, _url: &str) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn open_tab_background(&self, _url: &str) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn open_tab_at(&self, _url: &str, _idx: usize) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn close_tab(&self, _id: TabId) -> Result<bool, EngineError> {
            unimplemented!()
        }
        fn close_active(&self) -> Result<bool, EngineError> {
            unimplemented!()
        }
        fn select_tab(&self, _id: TabId) {}
        fn next_tab(&self) {}
        fn prev_tab(&self) {}
        fn move_tab(&self, _from: usize, _to: usize) {}
        fn duplicate_active(&self) -> Result<TabId, EngineError> {
            unimplemented!()
        }
        fn toggle_pin_active(&self) {}
        fn set_pinned(&self, _id: TabId, _pinned: bool) {}
        fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
            unimplemented!()
        }
        fn closed_stack_len(&self) -> usize {
            0
        }
        fn active_tab(&self) -> Option<TabSummary> {
            None
        }
        fn tabs_summary(&self) -> Vec<TabSummary> {
            vec![]
        }
        fn tab_count(&self) -> usize {
            0
        }
        fn pinned_count(&self) -> usize {
            0
        }
        fn active_index(&self) -> Option<usize> {
            None
        }
        fn navigate(&self, _url: &str) -> Result<(), EngineError> {
            unimplemented!()
        }
        fn active_tab_live_url(&self) -> String {
            String::new()
        }
        fn pump_address_changes(&self) -> bool {
            false
        }
        fn resize(&self, _w: u32, _h: u32) {}
        fn set_device_scale(&self, _scale: f32) {}
        fn set_frame_rate(&self, _hz: u32) {}
        fn notify_screen_info_changed(&self) {}
        fn osr_resize(&self, _w: u32, _h: u32) {}
        fn osr_key_event(&self, _event: NeutralKeyEvent) {}
        fn osr_mouse_move(&self, _x: i32, _y: i32, _mods: u32) {}
        fn osr_mouse_click(
            &self,
            _x: i32,
            _y: i32,
            _btn: MouseButton,
            _up: bool,
            _cnt: i32,
            _mods: u32,
        ) {
        }
        fn osr_mouse_leave(&self, _mods: u32) {}
        fn osr_mouse_wheel(&self, _x: i32, _y: i32, _dx: i32, _dy: i32, _mods: u32) {}
        fn osr_focus(&self, _focused: bool) {}
        fn osr_frame(&self) -> SharedOsrFrame {
            Arc::new(Mutex::new(OsrFrame::new(1, 1)))
        }
        fn osr_view(&self) -> SharedOsrViewState {
            Arc::new(OsrViewState::default())
        }
        fn force_repaint_active(&self) {}
        fn osr_sleep(&self, _sleep: bool) {}
        fn osr_invalidate_view(&self) {}
        fn set_osr_wake(&self, _wake: Arc<dyn Fn() + Send + Sync>) {}
        fn start_find(&self, _query: &str, _forward: bool) {}
        fn stop_find(&self) {}
        fn active_zoom_level(&self) -> f64 {
            1.0
        }
        fn any_audio_active(&self) -> bool {
            false
        }
        fn any_video_active(&self) -> bool {
            false
        }
        fn popup_queue(&self) -> PopupQueue {
            new_popup_queue()
        }
        fn popup_create_sink(&self) -> PopupCreateSink {
            new_popup_create_sink()
        }
        fn popup_close_sink(&self) -> PopupCloseSink {
            new_popup_close_sink()
        }
        fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {}
        fn popup_close(&self, _browser_id: i32) {}
        fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
            vec![]
        }
        fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
            vec![]
        }
        fn popup_history_back(&self, _browser_id: i32) {}
        fn popup_history_forward(&self, _browser_id: i32) {}
        fn popup_osr_focus(&self, _browser_id: i32, _focused: bool) {}
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
        fn is_hint_mode(&self) -> bool {
            false
        }
        fn hint_status(&self) -> Option<HintStatus> {
            None
        }
        fn pump_hint_events(&self) -> bool {
            false
        }
        fn feed_hint_key(&self, _c: char) -> Option<HintAction> {
            None
        }
        fn backspace_hint(&self) -> Option<HintAction> {
            None
        }
        fn cancel_hint(&self) {}
    }

    #[test]
    fn trait_default_zoom_methods_no_op() {
        let eng = NoOpEngine;
        // These have default no-op impls; must not panic and return the
        // correct type (no return value — just must not panic).
        eng.zoom_in();
        eng.zoom_out();
        eng.zoom_reset();
    }

    #[test]
    fn trait_default_open_devtools_returns_ok() {
        let eng = NoOpEngine;
        let result = eng.open_devtools(TabId(1));
        assert!(result.is_ok(), "default open_devtools should return Ok");
    }

    #[test]
    fn trait_default_frame_methods_no_op() {
        let eng = NoOpEngine;
        // All frame_* defaults must not panic.
        eng.frame_undo();
        eng.frame_redo();
        eng.frame_cut();
        eng.frame_copy();
        eng.frame_paste();
        eng.frame_paste_plain();
        eng.frame_select_all();
    }

    #[test]
    fn trait_default_media_methods_no_op() {
        let eng = NoOpEngine;
        eng.media_play_pause(100, 200);
        eng.media_picture_in_picture(100, 200);
        eng.media_toggle_controls(100, 200);
        eng.media_toggle_loop(100, 200);
        eng.media_toggle_mute(100, 200);
        eng.image_rotate(100, 200, 90);
    }

    #[test]
    fn trait_default_run_js_returns_unimplemented() {
        let eng = NoOpEngine;
        let r = eng.run_js("alert(1)");
        assert!(
            matches!(r, Err(EngineError::Unimplemented { method: "run_js" })),
            "default run_js should return Unimplemented"
        );
        let r2 = eng.run_main_frame_js("alert(1)", "buffr://test");
        assert!(
            matches!(
                r2,
                Err(EngineError::Unimplemented {
                    method: "run_main_frame_js"
                })
            ),
            "default run_main_frame_js should return Unimplemented"
        );
    }

    #[test]
    fn trait_default_edit_and_misc_methods_no_op() {
        let eng = NoOpEngine;
        eng.run_edit_attach("field-1");
        eng.run_edit_cycle(true);
        eng.run_edit_detach("field-1");
        eng.run_edit_focus("field-1");
        eng.run_media_probe();
        eng.start_download("https://example.com/file.zip");
        eng.show_dev_tools_at(0, 0);
    }
}
