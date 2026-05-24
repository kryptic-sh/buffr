//! Touch input types. Verbatim copy of wayr's touch.rs shape.
//!
//! Touch is not currently emitted on the non-Linux backend (winit's
//! touch story is platform-specific and buffr-app does not use it).

use super::geometry::Position;

/// Identifier for a single touch contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TouchId(pub i32);

/// Lifecycle phase of a single touch event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TouchPhase {
    /// Finger / stylus first contacted the surface.
    Started,
    /// Contact moved while still down.
    Moved,
    /// Contact lifted normally.
    Ended,
    /// System cancelled the gesture.
    Cancelled,
}

/// A single touch event.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct TouchEvent {
    /// Which contact this event refers to.
    pub id: TouchId,
    /// Phase of this event in the contact's lifecycle.
    pub phase: TouchPhase,
    /// Surface-local position.
    pub position: Position,
}
