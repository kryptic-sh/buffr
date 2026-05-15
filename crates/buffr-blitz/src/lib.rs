//! Blitz backend for buffr-engine.
//!
//! # Phase B status
//!
//! Real Blitz DOM/layout integration using:
//!   - `blitz-dom`   — DOM tree, Stylo CSS parsing, Taffy layout
//!   - `blitz-html`  — HTML5 parser
//!   - `blitz-traits` — shared trait surface
//!
//! # wgpu version conflict note
//!
//! The GPU rendering layer (`anyrender_vello`) requires `wgpu ^28`.
//! The workspace pins `wgpu = "29"` (semver-incompatible).  Rather than
//! downgrading the workspace we skip the Vello renderer for Phase B.
//! OSR frames are written as solid-colour fills; real GPU readback lands
//! in Phase C once the wgpu version is aligned.
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
pub mod worker;

pub use backend::BlitzBackend;
pub use engine::BlitzEngine;
pub use error::BlitzError;
