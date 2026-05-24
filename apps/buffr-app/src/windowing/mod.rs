//! Platform-conditional windowing abstraction.
//!
//! buffr's chrome compositor and input dispatch are
//! windowing-toolkit-agnostic above this module. Two concrete
//! backends sit underneath:
//!
//! - Linux: [`wayr`] — Wayland-native, with `wl_subsurface` support
//!   needed by the planned WPE WebKit engine path.
//! - macOS / Windows: [`winit`] — cross-platform; CEF in OSR mode
//!   renders into a wgpu surface, no native window-embedding needed.
//!
//! Bridge types live here: [`BuffrWindowEvent`] flattens the few
//! variants that wayr and winit shape differently into a single
//! enum the rest of buffr-app pattern-matches on. Where the two
//! toolkits agree (e.g. `CursorIcon`, modifier shape, `Size`), this
//! module simply re-exports the native type per target.
//!
//! Migration is incremental — see the `windowing/` module commits
//! for the Phase-by-Phase log.

// During Phases 1-2 the re-exports above are scaffolding; main.rs
// hasn't migrated yet so the names look unused. Phase 3 flips
// main.rs to import from here, at which point the allow comes off.
#![allow(unused_imports)]

// Concrete backend re-exports. Each platform module re-exports a
// `Window`, `EventLoop`, `EventLoopProxy`, `WindowBuilder`, etc.,
// shaped so that buffr-app can use one set of names regardless of
// which toolkit is underneath.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod other;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(target_os = "linux"))]
pub use other::*;
