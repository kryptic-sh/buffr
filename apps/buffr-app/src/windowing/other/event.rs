//! Per-surface `WindowEvent` enum + winit → bridge mapping.

use super::geometry::Size;
use super::ime::ImeEvent;
use super::keyboard::KeyEvent;
use super::keyboard::Modifiers;
use super::pointer::{PointerButton, PointerButtonState, PointerPosition, ScrollEvent};

/// Per-surface event variants — verbatim shape of wayr's `WindowEvent`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WindowEvent {
    /// Surface logical size changed.
    Resized(Size),

    /// Surface scale factor changed.
    ScaleFactorChanged {
        /// New scale factor (e.g. `1.0`, `1.25`, `1.5`, `2.0`).
        new_scale_factor: f64,
        /// Suggested surface size in logical pixels.
        suggested_size: Size,
    },

    /// Window manager / OS requested the surface close.
    CloseRequested,

    /// OS told us the surface should redraw now.
    RedrawRequested,

    /// Surface gained keyboard focus.
    Focused,

    /// Surface lost keyboard focus.
    Unfocused,

    /// Surface occlusion changed. `true` when obscured.
    Occluded(bool),

    /// Pointer entered the surface.
    PointerEntered {
        /// Initial pointer position.
        position: PointerPosition,
    },

    /// Pointer left the surface.
    PointerLeft,

    /// Pointer moved while inside the surface.
    PointerMoved {
        /// New pointer position.
        position: PointerPosition,
    },

    /// Pointer button transitioned.
    PointerButton {
        /// Which button.
        button: PointerButton,
        /// Pressed or released.
        state: PointerButtonState,
        /// Current modifier state.
        modifiers: Modifiers,
    },

    /// Scroll / wheel event.
    Scroll(ScrollEvent),

    /// Keyboard key event.
    Key(KeyEvent),

    /// Modifier state changed without an accompanying key event.
    ///
    /// Diverges from wayr — wayr bakes the modifier state into every
    /// `Key` / `PointerButton` event because Wayland delivers it that
    /// way. winit fires a separate `ModifiersChanged` and may dispatch
    /// it AFTER the corresponding key release on some backends,
    /// stranding consumers that only sync modifiers from key events
    /// (caused the Ctrl-sticky regression in v0.14.2). Bridge exposes
    /// it so `self.modifiers` can be kept in lockstep with winit's
    /// authoritative cache.
    ModifiersChanged(Modifiers),

    /// IME composition event.
    Ime(ImeEvent),
}
