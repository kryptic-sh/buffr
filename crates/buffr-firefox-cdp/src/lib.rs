//! Firefox CDP backend for buffr-engine.
//!
//! Phase A stub: all engine methods return `EngineError::Unimplemented`.
//! Phase B will wire real Firefox CDP integration.

pub mod backend;
pub mod engine;
pub mod error;

pub use backend::FirefoxCdpBackend;
pub use engine::FirefoxCdpEngine;
pub use error::FirefoxError;
