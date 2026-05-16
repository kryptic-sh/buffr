//! Blitz backend for buffr-engine.
//!
//! # Status
//!
//! Real Blitz DOM/layout + CPU rasterisation using:
//!   - `blitz-dom`          — DOM tree, Stylo CSS parsing, Taffy layout
//!   - `blitz-html`         — HTML5 parser
//!   - `blitz-traits`       — shared trait surface
//!   - `blitz-paint`        — `paint_scene` bridge (DOM → anyrender draw commands)
//!   - `anyrender_vello_cpu` — pure-CPU rasteriser; no wgpu dependency
//!
//! # Renderer strategy
//!
//! The workspace pins `wgpu = "29"`.  The GPU rendering layer
//! (`anyrender_vello`) requires `wgpu ^28` (semver-incompatible), so we use
//! the CPU path instead.  `anyrender_vello_cpu 0.10` requires `anyrender ^0.8`
//! which matches `blitz-paint 0.3.0-alpha.2` exactly.
//!
//! OSR frames are premultiplied BGRA8 (R↔B-swapped from vello_cpu's
//! premultiplied RGBA8 output).
//!
//! # Architecture
//!
//! `BlitzEngine` owns a `Mutex<BlitzState>` holding:
//!   - `Vec<BlitzTab>` — each tab owns an `HtmlDocument` (real Blitz DOM)
//!   - `SharedOsrFrame` / `SharedOsrViewState` — stable Arcs returned by
//!     `osr_frame()` / `osr_view()`
//!
//! All `BrowserEngine` methods are implemented; none return `Unimplemented`
//! for the core surface.  Popup / hint / find / edit / media families keep
//! their default no-op impls.

pub mod backend;
pub mod document;
pub mod engine;
pub mod error;
pub mod input;
pub(crate) mod render;
pub(crate) mod shell;
pub mod worker;

pub use backend::BlitzBackend;
pub use engine::BlitzEngine;
pub use error::BlitzError;
