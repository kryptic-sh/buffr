//! Headless Chromium CDP backend for buffr-engine.
//!
//! `buffr-blink-cdp` implements the [`buffr_engine::BrowserEngine`] trait by
//! driving a headless Chromium process over the Chrome DevTools Protocol
//! (CDP).  It serves as the second concrete backend that validates the engine
//! abstraction and enables running buffr without the CEF SDK installed.
//!
//! # Position in the workspace
//!
//! ```text
//! apps/buffr  ──►  buffr-engine (trait)
//!                       ▲
//!              buffr-blink-cdp  (this crate — CDP backend)
//! ```
//!
//! No CEF headers or libraries are needed.  A Chromium or Google Chrome
//! binary must be present on `PATH` at runtime (see System requirement below).
//!
//! # System requirement
//!
//! A Chromium or Google Chrome binary must be installed and reachable via
//! `PATH`.  Checked candidates (in order):
//!
//! - Linux: `chromium-browser`, `chromium`, `google-chrome`, `google-chrome-stable`
//! - macOS: `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`,
//!   then the same PATH candidates
//!
//! The binary is **not** bundled — no Chromium binaries are downloaded or
//! embedded.  If none is found, [`engine::BlinkCdpEngine::new`] returns
//! [`error::BlinkError::ChromiumNotFound`].
//!
//! # Implements
//!
//! [`BrowserEngine`](buffr_engine::BrowserEngine) methods fully covered:
//! - Tab lifecycle: `open_tab`, `close_tab`, `activate_tab`
//! - Navigation: `navigate`, `go_back`, `go_forward`, `reload`, `stop`
//! - OSR: `osr_resize` + `Page.startScreencast` push (ack-throttled)
//! - Input: `send_key_event`, `send_mouse_event`, `send_scroll_event`
//!
//! Deferred (return [`buffr_engine::EngineError::Unimplemented`]):
//! `duplicate_active`, `reopen_closed_tab`, clipboard ops, edit ops,
//! context menu handling, find-in-page, downloads, audio, dev tools,
//! custom schemes, and OSR sleep.
//!
//! # Phases
//!
//! The v0.9.0 capability surface targets:
//! - Permissions: `Page.handleJavaScriptDialog` + `Browser.grantPermissions`
//! - Downloads: `Page.downloadWillBegin` + `Page.downloadProgress` CDP events
//! - Find-in-page: `Page.find` + `Page.findNextResult`
//! - Context menu: `Page.contextMenuShown` CDP event + build model bridge
//! - IME: `Input.imeSetComposition` + `Input.insertText`
//! - Custom schemes: `fetch` intercept via `Network.requestIntercepted`
//! - Picture-in-Picture: JS `video.requestPictureInPicture()` via
//!   `Runtime.evaluate`

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
