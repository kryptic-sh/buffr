//! Error types for the Servo backend.

use thiserror::Error;

/// Errors specific to the Servo backend.
#[derive(Debug, Error)]
pub enum ServoError {
    #[error("servo is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("servo initialization failed: {0}")]
    InitFailed(String),
}

impl From<ServoError> for buffr_engine::EngineError {
    fn from(e: ServoError) -> Self {
        match e {
            ServoError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            ServoError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
