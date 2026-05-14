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
//!   - hint-mode methods (`enter_hint_mode`, `feed_hint_key`, etc.)
//!   - popup-bookkeeping (`popup_osr_*`, `popup_history_*`, `popup_resize`,
//!     `popup_close`, `popup_drain_*`)
//!   - favicon accessors (`favicon_sink`, `favicon_enabled`, etc.)
//!   - permissions accessors (`permissions`, `permissions_queue`)
//!   - context-menu drain (`drain_context_menu_requests`)
//!   - audio event queue drain (`drain_audio_events`)
//!   - edit / clipboard helpers
//!   - media probe helpers
//!
//! Phase 2 will widen the trait as the apps layer is further decoupled.

use std::sync::Arc;

use crate::{
    EngineError, MouseButton, NeutralKeyEvent, SharedOsrFrame, SharedOsrViewState, TabId,
    TabSummary,
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

    /// Active tab's CEF zoom level. 0.0 = default.
    fn active_zoom_level(&self) -> f64;

    // ── Audio / video ────────────────────────────────────────────────────────

    /// `true` when at least one browser has an active audio stream.
    fn any_audio_active(&self) -> bool;

    /// `true` when the last JS media probe reported a video signal active.
    fn any_video_active(&self) -> bool;
}
