//! Per-surface `WindowEvent` enum + winit → bridge mapping.

use super::geometry::Size;
use super::ime::ImeEvent;
use super::keyboard::KeyEvent;
use super::keyboard::Modifiers;
use super::pointer::{PointerButton, PointerButtonState, PointerPosition, ScrollEvent};
use super::touch::TouchEvent;

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

    /// Touch event.
    Touch(TouchEvent),

    /// Keyboard key event.
    Key(KeyEvent),

    /// IME composition event.
    Ime(ImeEvent),
}
