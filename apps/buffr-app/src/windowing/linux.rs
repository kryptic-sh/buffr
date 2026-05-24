//! Linux backend — re-exports wayr types.
//!
//! Most names pass through unchanged; the few divergent surfaces
//! (event variants, key event shape) get bridge types in
//! [`super`].

#![allow(unused_imports)]

// Lifecycle + dispatch.
pub use wayr::{ApplicationHandler, EventLoop, EventLoopProxy};

// Surfaces — wayr names its main window type `Toplevel`. Alias so
// the buffr-app code reads with one name across platforms.
pub use wayr::Surface;
pub use wayr::SurfaceId;
pub use wayr::Toplevel as Window;

// Geometry + cursor — identical shape across toolkits.
pub use wayr::{CursorIcon, Modifiers, Position, Rect, Size};

// Output info — refresh rate / scale used by the paint scheduler.
pub use wayr::OutputInfo;

// Input event types.
pub use wayr::{
    AxisDirection, AxisSource, KeyCode, KeyEvent, KeyState, PointerButton, PointerButtonState,
    PointerPosition, ScanCode, ScrollEvent,
};

// Touch.
pub use wayr::{TouchEvent, TouchId, TouchPhase};

// IME.
pub use wayr::{ContentHint, ContentPurpose, ImeEvent};

// Window event — direct re-export for now. Migration to a bridge
// `BuffrWindowEvent` enum lands in Phase 2 once the macOS/Windows
// `other` module needs the same surface.
pub use wayr::WindowEvent;
