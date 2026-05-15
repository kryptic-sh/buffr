//! Error types for the Blitz backend.

use thiserror::Error;

/// Errors specific to the Blitz backend.
#[derive(Debug, Error)]
pub enum BlitzError {
    #[error("blitz is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("blitz initialization failed: {0}")]
    InitFailed(String),
}

impl From<BlitzError> for buffr_engine::EngineError {
    fn from(e: BlitzError) -> Self {
        match e {
            BlitzError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            BlitzError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
