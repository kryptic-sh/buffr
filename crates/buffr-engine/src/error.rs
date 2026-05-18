//! Engine error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine initialization failed: {0}")]
    InitFailed(String),

    #[error("tab not found: {0}")]
    TabNotFound(crate::TabId),

    #[error("no active tab")]
    NoActiveTab,

    #[error("browser creation failed")]
    CreateBrowserFailed,

    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("operation failed: {0}")]
    Other(String),

    /// Returned by backends that have not yet implemented the method.
    ///
    /// `method` carries the trait method name for diagnostics. Stub
    /// backends return this for popup_*, hint_*, find_*, zoom_*,
    /// devtools_*, scheme_handler_*, and other non-core methods.
    #[error("not implemented: {method}")]
    Unimplemented { method: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn engine_error_send_sync() {
        assert_send_sync::<EngineError>();
    }

    #[test]
    fn engine_error_unimplemented_display_includes_method() {
        let err = EngineError::Unimplemented { method: "zoom_in" };
        let msg = err.to_string();
        assert!(
            msg.contains("zoom_in"),
            "Display should include method name, got: {msg}"
        );
    }

    #[test]
    fn engine_error_other_carries_string() {
        let err = EngineError::Other("channel full".to_string());
        let msg = err.to_string();
        assert!(msg.contains("channel full"), "got: {msg}");
    }

    #[test]
    fn engine_error_tab_not_found_includes_id() {
        let err = EngineError::TabNotFound(crate::TabId(42));
        let msg = err.to_string();
        assert!(msg.contains("42"), "got: {msg}");
    }

    #[test]
    fn engine_error_init_failed_carries_message() {
        let err = EngineError::InitFailed("bad flags".into());
        let msg = err.to_string();
        assert!(msg.contains("bad flags"), "got: {msg}");
    }

    #[test]
    fn engine_error_invalid_url_carries_url() {
        let err = EngineError::InvalidUrl("not-a-url".into());
        let msg = err.to_string();
        assert!(msg.contains("not-a-url"), "got: {msg}");
    }
}
