//! Error types for the Firefox CDP backend.

use thiserror::Error;

/// Errors specific to the Firefox CDP backend.
#[derive(Debug, Error)]
pub enum FirefoxError {
    #[error("firefox-cdp is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("firefox-cdp initialization failed: {0}")]
    InitFailed(String),
}

impl From<FirefoxError> for buffr_engine::EngineError {
    fn from(e: FirefoxError) -> Self {
        match e {
            FirefoxError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            FirefoxError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
