//! `buffr-blink-cdp` — headless Chromium backend for `buffr-engine`.
//!
//! Phase 4 spike: validates the `BrowserEngine` trait against a real second
//! implementation driven over Chrome DevTools Protocol.
//!
//! ## System requirement
//!
//! A Chromium or Google Chrome binary must be installed and reachable via
//! `PATH`.  Checked candidates (in order):
//!   - Linux: `chromium-browser`, `chromium`, `google-chrome`, `google-chrome-stable`
//!   - macOS: `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`,
//!     then the same PATH candidates
//!
//! The binary is **not** bundled — no Chromium binaries are downloaded or
//! embedded.  If none is found, [`engine::BlinkCdpEngine::new`] returns
//! [`error::BlinkError::ChromiumNotFound`].
//!
//! ## Phase 4 scope
//!
//! Working: tab create/close, navigate, OSR frame via `Page.startScreencast`
//! push (Chromium-native cadence, ack-throttled), mouse click/move/wheel,
//! keyboard events, viewport resize.
//!
//! Stubbed (return `EngineError::Unimplemented`): `duplicate_active`,
//! `reopen_closed_tab`, and any method not listed above.

pub mod backend;
pub mod cdp;
pub mod context_menu;
pub mod engine;
pub mod error;
pub mod find;
pub mod permissions;
pub mod pip;
pub mod subprocess;
pub mod worker;
pub mod ws;

pub use backend::BlinkCdpBackend;
pub use engine::BlinkCdpEngine;
pub use error::BlinkError;
