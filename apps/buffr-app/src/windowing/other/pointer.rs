//! Pointer / mouse input types. Verbatim copy of wayr's pointer.rs.

use super::geometry::Position;

/// Logical pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PointerButton {
    /// Primary (typically left).
    Left,
    /// Secondary (typically right).
    Right,
    /// Middle / wheel-click.
    Middle,
    /// "Back" thumb button.
    Back,
    /// "Forward" thumb button.
    Forward,
    /// Any other button code.
    Other(u32),
}

/// Whether a pointer button transitioned to pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButtonState {
    /// Button just pressed.
    Pressed,
    /// Button just released.
    Released,
}

/// Source of an axis (scroll) event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AxisSource {
    /// Discrete scroll wheel.
    Wheel,
    /// Touchpad two-finger scroll (smooth, sub-pixel).
    Finger,
    /// Continuous-motion device.
    Continuous,
    /// Tilt of the scroll wheel sideways.
    WheelTilt,
}

/// Scroll axis: vertical or horizontal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisDirection {
    /// Vertical (most common).
    Vertical,
    /// Horizontal (Shift+wheel, touchpad two-finger horizontal).
    Horizontal,
}

/// A scroll / wheel event.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ScrollEvent {
    /// Which axis scrolled.
    pub axis: AxisDirection,
    /// Smooth delta in logical pixels. Positive = down / right.
    pub delta: f64,
    /// Discrete detent count (0 if source is not a wheel).
    pub discrete_steps: i32,
    /// High-resolution scroll, in 1/120ths of a logical wheel detent.
    /// `0` when not a high-res wheel source.
    pub high_res_120: i32,
    /// What kind of input produced the event.
    pub source: AxisSource,
}

/// Pointer position relative to the surface's origin, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerPosition(pub Position);

impl From<Position> for PointerPosition {
    fn from(p: Position) -> Self {
        PointerPosition(p)
    }
}
