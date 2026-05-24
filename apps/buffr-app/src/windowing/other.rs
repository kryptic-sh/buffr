//! Non-Linux backend (macOS / Windows) — re-exports winit types.
//!
//! Phase 1 (this commit): scaffold-only. Full event mapping lands in
//! Phase 2 — see issue tracking the windowing-abstraction migration.

#![allow(unused_imports, dead_code)]

// Lifecycle + dispatch. winit's shape differs from wayr's:
//   wayr::ApplicationHandler<T> ↔ winit::application::ApplicationHandler (no generic; user events use a different proxy)
//   wayr::EventLoop<T> ↔ winit::event_loop::EventLoop<T>
//   wayr::EventLoopProxy<T> ↔ winit::event_loop::EventLoopProxy<T>
//
// Phase 2 wraps these so the buffr-app dispatch loop reads the same
// either way. For now, just expose the bare winit names.
pub use winit::event_loop::{EventLoop, EventLoopProxy};

// Surfaces.
pub use winit::window::{Window, WindowAttributes};

// Geometry + cursor.
pub use winit::dpi::PhysicalSize as Size;
pub use winit::window::CursorIcon;

// `WindowEvent` re-export. Variants differ from wayr's enum, so the
// buffr-app side of the migration introduces a bridge enum in
// Phase 2 (BuffrWindowEvent) that maps both toolkits' events onto a
// common surface. For now: bare re-export.
pub use winit::event::WindowEvent;
