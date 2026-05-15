//! Real WKWebView implementation — compiled on macOS only.

// The platform module's objc2 Retained<> optional chains + dispatch_async
// closures read more clearly as nested `if let Some + if cond` than the
// collapsed `if let ... && let Some(...) =` form clippy suggests. Same
// for the explicit returns inside `#[cfg(target_os = "macos")] { ... }`
// blocks (the unreachable_code fallback below needs them) and a few
// `&*RcBlock` derefs that clippy thinks are needless.
#![allow(
    clippy::collapsible_if,
    clippy::needless_return,
    clippy::explicit_auto_deref
)]

pub mod backend;
pub mod engine;
pub mod error;
pub mod input;
pub mod osr;
pub mod runtime;
pub mod worker;

pub use backend::WebKitCocoaBackend;
pub use engine::WebKitCocoaEngine;
pub use error::WebKitCocoaError;
