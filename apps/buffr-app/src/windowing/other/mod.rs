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
//! - [`touch`] — touch types (stubbed; not emitted on the winit path
//!   in v0.1).
//! - [`cursor`] — `CursorIcon` enum + winit mapping.
//! - [`ime`] — IME types + variants.
//! - [`output`] — `OutputInfo` snapshot type used by the refresh-rate
//!   pacing logic in main.rs.
//! - [`event`] — `WindowEvent` bridge enum.
//! - [`surface`] — `Surface` trait + `SurfaceId`.
//! - [`window`] — `Window` (wraps `winit::window::Window`) + builder.
//! - [`event_loop`] — `EventLoop`, `EventLoopProxy`, dispatch bridge.

#![allow(unused_imports)]

mod cursor;
mod event;
mod event_loop;
mod geometry;
mod ime;
mod keyboard;
mod output;
mod pointer;
mod surface;
mod touch;
mod window;

pub use cursor::CursorIcon;
pub use event::WindowEvent;
pub use event_loop::{ApplicationHandler, EventLoop, EventLoopProxy};
pub use geometry::{Position, Rect, Size};
pub use ime::{ContentHint, ContentPurpose, ImeEvent};
pub use keyboard::{KeyCode, KeyEvent, KeyState, Modifiers, ScanCode};
pub use output::{OutputId, OutputInfo};
pub use pointer::{
    AxisDirection, AxisSource, PointerButton, PointerButtonState, PointerPosition, ScrollEvent,
};
pub use surface::{RawWindowHandlePlaceholder, Surface, SurfaceId};
pub use touch::{TouchEvent, TouchId, TouchPhase};
pub use window::{ActivationError, BuildError, ToplevelBuilder, Window};
