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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::{
        EngineError, MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState, TabId, TabSummary,
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
}
