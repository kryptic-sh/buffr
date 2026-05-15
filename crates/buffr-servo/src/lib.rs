//! Servo backend for buffr-engine.
//!
//! Phase A stub: all engine methods return `EngineError::Unimplemented`.
//! Phase B will wire real Servo integration.

pub mod backend;
pub mod engine;
pub mod error;

pub use backend::ServoBackend;
pub use engine::ServoEngine;
pub use error::ServoError;
