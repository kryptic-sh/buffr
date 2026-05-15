//! Real (stub) WebView2 implementation — compiled on Windows only.

pub mod backend;
pub mod engine;
pub mod error;

pub use backend::WebView2Backend;
pub use engine::WebView2Engine;
pub use error::WebView2Error;
