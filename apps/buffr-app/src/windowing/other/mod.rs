//! Non-Linux backend (macOS / Windows) — wraps winit.
//!
//! Re-exports the wayr-shaped surface area so the buffr-app code
//! reads the same on every platform. CEF runs OSR-only on macOS /
//! Windows so no native subsurface embedding is required; only the
//! windowing + input + event-loop primitives need a winit bridge.
//!
//! Module layout:
//! - [`geometry`] — `Position`, `Size`, `Rect` (verbatim copies of
//!   wayr's shape).
//! - [`pointer`] — pointer/scroll/button types.
//! - [`keyboard`] — `KeyEvent`, `KeyCode`, `Modifiers`, `ScanCode`
//!   plus winit-to-bridge mappers.
//! - [`cursor`] — `CursorIcon` enum + winit mapping.
//! - [`ime`] — IME types + variants.
//! - [`output`] — `OutputInfo` snapshot type used by the refresh-rate
//!   pacing logic in main.rs.
//! - [`event`] — `WindowEvent` bridge enum.
//! - [`surface`] — `Surface` trait + `SurfaceId`.
//! - [`window`] — `Window` (wraps `winit::window::Window`) + builder.
//! - [`event_loop`] — `EventLoop`, `EventLoopProxy`, dispatch bridge.

mod cursor;
mod event;
mod event_loop;
mod geometry;
mod ime;
mod keyboard;
mod output;
mod pointer;
mod surface;
mod window;

pub use cursor::CursorIcon;
pub use event::WindowEvent;
pub use event_loop::{ApplicationHandler, EventLoop, EventLoopProxy};
pub use geometry::Size;
pub use ime::ImeEvent;
pub use keyboard::{KeyCode, KeyEvent, KeyState, Modifiers, ScanCode};
pub use pointer::{AxisDirection, AxisSource, PointerButton, PointerButtonState, ScrollEvent};
pub use surface::{Surface, SurfaceId};
pub use window::Window;
