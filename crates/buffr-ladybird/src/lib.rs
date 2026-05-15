//! Ladybird backend for buffr-engine.
//!
//! Phase A stub: all engine methods return `EngineError::Unimplemented`.
//! Phase B will wire real Ladybird integration.

pub mod backend;
pub mod engine;
pub mod error;

pub use backend::LadybirdBackend;
pub use engine::LadybirdEngine;
pub use error::LadybirdError;
