//! Blitz backend for buffr-engine.
//!
//! Phase A stub: all engine methods return `EngineError::Unimplemented`.
//! Phase B will wire real Blitz integration.

pub mod backend;
pub mod engine;
pub mod error;

pub use backend::BlitzBackend;
pub use engine::BlitzEngine;
pub use error::BlitzError;
