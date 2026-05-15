//! Error types for the Ladybird backend.

use thiserror::Error;

/// Errors specific to the Ladybird backend.
#[derive(Debug, Error)]
pub enum LadybirdError {
    #[error("ladybird is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("ladybird initialization failed: {0}")]
    InitFailed(String),

    #[error("ladybird FFI error: {0}")]
    Ffi(String),

    #[error("tab not found: {0:?}")]
    TabNotFound(buffr_engine::TabId),

    #[error("no active tab")]
    NoActiveTab,
}

impl From<LadybirdError> for buffr_engine::EngineError {
    fn from(e: LadybirdError) -> Self {
        match e {
            LadybirdError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            LadybirdError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            LadybirdError::Ffi(msg) => buffr_engine::EngineError::Other(msg),
            LadybirdError::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab not found: {id:?}"))
            }
            LadybirdError::NoActiveTab => {
                buffr_engine::EngineError::Other("no active tab".to_owned())
            }
        }
    }
}
