//! Firefox CDP backend for buffr-engine.
//!
//! `buffr-firefox-cdp` implements the [`buffr_engine::BrowserEngine`] trait by
//! driving a headless Firefox process over the Chrome DevTools Protocol (CDP)
//! subset supported by Firefox's Remote Agent.
//!
//! # Position in the workspace
//!
//! ```text
//! apps/buffr  ──►  buffr-engine (trait)
//!                       ▲
//!              buffr-firefox-cdp  (this crate — Firefox CDP backend)
//! ```
//!
//! # System requirement
//!
//! A Firefox binary must be installed. Checked candidates (in order):
//!
//! - Linux: `firefox`, `firefox-developer-edition`, `firefox-esr`, `firefox-nightly`
//! - macOS: `/Applications/Firefox.app/Contents/MacOS/firefox`,
//!   `/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox`,
//!   then the same PATH candidates
//!
//! The binary is **not** bundled. If none is found, `FirefoxCdpEngine::new`
//! returns [`error::FirefoxError::FirefoxNotFound`].
//!
//! # CDP subset
//!
//! Firefox supports a subset of Chrome DevTools Protocol via its Remote Agent.
//! The following methods are **working** as of late 2025:
//!
//! - `Target.*` (limited): `createTarget`, `closeTarget`, `getTargets`, `attachToTarget`
//! - `Page.navigate`, `Page.reload`, `Page.stop`
//! - `Page.getNavigationHistory` + `Page.navigateToHistoryEntry`
//! - `Page.captureScreenshot` (used for OSR — Firefox has no `Page.startScreencast`)
//! - `Runtime.evaluate`
//! - `Input.dispatchKeyEvent`, `Input.dispatchMouseEvent`
//! - `Page.frameNavigated` event
//! - `Emulation.setDeviceMetricsOverride`
//!
//! The following are **missing** in Firefox CDP (deferred to Phase C / BiDi):
//!
//! - `Page.startScreencast` / `Page.screencastFrame` — no streaming screencast
//!   over CDP. This backend uses a `Page.captureScreenshot` poll loop at 4 fps.
//! - `Browser.grantPermissions` — defer to Phase C (BiDi)
//! - `Page.handleJavaScriptDialog` — partial; deferred

pub mod backend;
pub mod cdp;
pub mod engine;
pub mod error;
pub mod subprocess;
pub mod worker;
pub mod ws;

pub use backend::FirefoxCdpBackend;
pub use engine::FirefoxCdpEngine;
pub use error::FirefoxError;
