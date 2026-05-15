//! Error types for the Ladybird backend.

use thiserror::Error;

/// Errors specific to the Ladybird backend.
#[derive(Debug, Error)]
pub enum LadybirdError {
    #[error("ladybird is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("ladybird initialization failed: {0}")]
    InitFailed(String),
}

impl From<LadybirdError> for buffr_engine::EngineError {
    fn from(e: LadybirdError) -> Self {
        match e {
            LadybirdError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            LadybirdError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
