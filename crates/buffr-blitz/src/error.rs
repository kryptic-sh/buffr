//! Error types for the Blitz backend.

use thiserror::Error;

use buffr_engine::TabId;

/// Errors specific to the Blitz backend.
#[derive(Debug, Error)]
pub enum BlitzError {
    #[error("blitz is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("blitz initialization failed: {0}")]
    InitFailed(String),

    #[error("tab {0} not found")]
    TabNotFound(TabId),

    #[error("blitz network fetch failed: {0}")]
    FetchFailed(String),
}

impl From<BlitzError> for buffr_engine::EngineError {
    fn from(e: BlitzError) -> Self {
        match e {
            BlitzError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            BlitzError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            BlitzError::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab {id} not found"))
            }
            BlitzError::FetchFailed(msg) => buffr_engine::EngineError::Other(msg),
        }
    }
}
