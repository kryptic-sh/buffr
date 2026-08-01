//! buffr internal page constants.
//!
//! The `buffr://` custom-scheme handler that used to live here has been
//! removed. Navigation to `buffr://new` etc. is now rewritten to the
//! shared [`buffr_engine::internal_server::InternalServer`] HTTP loopback by
//! `BrowserHost::cef_navigation_url` — eliminating `ERR_UNKNOWN_URL_SCHEME`
//! without a CEF scheme registration.
//!
//! `buffr-src:` (view-source) still uses a custom CEF scheme — its handler
//! lives in `view_source_scheme.rs`.
//!
//! The `settings_html` builder, its private `html_escape`, and the
//! `NewTabHtmlProvider` / `SettingsHtmlProvider` aliases were removed (L17,
//! L13): the apps layer uses `buffr_engine::newtab::default_settings_html`
//! and `buffr_engine::NewTabHtmlProvider`, and the strict HTML escaper now
//! lives in `crate::html`.

// Constants moved to buffr-engine::newtab (Phase 6e, #95).
// Re-exported here so existing `buffr_cef::NEW_TAB_*` imports keep resolving.
pub use buffr_engine::newtab::{
    NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_SPLASH_ART_MARKER, NEW_TAB_URL,
    SETTINGS_URL,
};
