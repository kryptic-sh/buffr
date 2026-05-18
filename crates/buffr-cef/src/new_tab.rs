//! buffr internal page type aliases and helpers.
//!
//! The `buffr://` custom-scheme handler that used to live here has been
//! removed. Navigation to `buffr://new` etc. is now rewritten to the
//! shared [`buffr_engine::internal_server::InternalServer`] HTTP loopback by
//! `BrowserHost::cef_navigation_url` — eliminating `ERR_UNKNOWN_URL_SCHEME`
//! without a CEF scheme registration.
//!
//! `buffr-src:` (view-source) still uses a custom CEF scheme — its handler
//! lives in `view_source_scheme.rs`.

use std::sync::Arc;

// Constants moved to buffr-engine::newtab (Phase 6e, #95).
// Re-exported here so existing `buffr_cef::NEW_TAB_*` imports keep resolving.
pub use buffr_engine::newtab::{
    NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_SPLASH_ART_MARKER, NEW_TAB_URL,
    SETTINGS_URL,
};

/// Closure invoked on each `buffr://new` request to produce the page
/// bytes. Returning a fresh `Vec<u8>` each call lets the apps layer
/// re-render the dynamic keybinding section without restarting CEF.
pub type NewTabHtmlProvider = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Closure invoked on each `buffr://settings` request to produce the
/// settings page bytes. Returning a fresh `Vec<u8>` each call lets the
/// page reflect live config state without a restart.
pub type SettingsHtmlProvider = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Build the default settings page HTML. Contains scaffolding only —
/// engine-rules are listed as read-only text; editing is not yet wired.
///
/// Accepts `engines` (registered engine ids) and `rules` (one
/// `"<pattern> → <engine>"` string per routing rule) so the caller can
/// pass the router state at request time.
pub fn settings_html(engines: &[&str], rules: &[&str]) -> Vec<u8> {
    let engine_rows: String = if engines.is_empty() {
        "<li><em>(none registered)</em></li>".to_string()
    } else {
        engines
            .iter()
            .map(|id| format!("<li><code>{}</code></li>", html_escape(id)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let rule_rows: String = if rules.is_empty() {
        "<li><em>(no routing rules — all URLs use the default engine)</em></li>".to_string()
    } else {
        rules
            .iter()
            .map(|r| format!("<li><code>{}</code></li>", html_escape(r)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8" />
  <title>buffr settings</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 2rem; color: #e0e0e0; background: #1a1a1a; }}
    h1 {{ font-size: 1.6rem; margin-bottom: 0.25rem; }}
    h2 {{ font-size: 1.1rem; margin-top: 1.5rem; color: #aaa; }}
    ul {{ list-style: disc; padding-left: 1.5rem; }}
    code {{ background: #2a2a2a; padding: 0.1em 0.4em; border-radius: 3px; font-size: 0.9em; }}
    .note {{ margin-top: 1.5rem; font-size: 0.85rem; color: #888; border-left: 3px solid #444; padding-left: 0.75rem; }}
  </style>
</head>
<body>
  <h1>buffr settings</h1>
  <h2>Engine routing</h2>
  <p>Registered engines:</p>
  <ul>
    {engine_rows}
  </ul>
  <p>Active routing rules (matched top-to-bottom against URL host):</p>
  <ul>
    {rule_rows}
  </ul>
  <p class="note">
    Editing rules in the UI is not yet implemented — edit <code>config.toml</code> directly
    and restart buffr to apply changes.
  </p>
</body>
</html>"#,
    );
    html.into_bytes()
}

/// HTML-escape `<`, `>`, and `&` in `s` to avoid injection in the
/// settings page. URLs and engine ids are untrusted user config.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
