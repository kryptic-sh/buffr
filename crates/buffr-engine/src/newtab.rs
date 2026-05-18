//! Engine-agnostic new-tab page constants.
//!
//! These are plain string constants — no CEF dependency. The apps layer reads
//! them directly and the InternalServer routes are wired against them.

/// The URL opened when the user presses `t` (TabNew).
pub const NEW_TAB_URL: &str = "buffr://new";

/// The URL for the engine-routing settings scaffold.
pub const SETTINGS_URL: &str = "buffr://settings";

/// Embedded new-tab HTML template. Contains marker strings that the apps
/// layer fills in at request time, so a config hot-reload is reflected on
/// the next page visit without a binary rebuild.
pub static NEW_TAB_HTML_TEMPLATE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/new_tab.html"));

/// The marker the apps layer replaces with rendered keybinding rows.
pub const NEW_TAB_KEYBINDS_MARKER: &str = "<!--KEYBINDS-->";

/// Marker the apps layer replaces with the static splash wordmark
/// (per-cell HTML grid). Substituted once per page request alongside
/// the keybindings; the splash overlay (`#buffr-splash`) is updated
/// per tick by the host via execute_javascript.
pub const NEW_TAB_SPLASH_ART_MARKER: &str = "<!--SPLASH-ART-->";

/// Minimal placeholder used when no settings provider is wired.
/// Still referenced by `apps/buffr-app` as the `/settings` InternalServer
/// route handler before a live provider is configured.
pub fn default_settings_html() -> Vec<u8> {
    b"<!DOCTYPE html><html><head><meta charset=\"utf-8\"/><title>buffr settings</title></head>\
      <body style=\"font-family:system-ui,sans-serif;background:#1a1a1a;color:#e0e0e0;margin:2rem\">\
      <h1>buffr settings</h1><p>Settings provider not configured.</p></body></html>"
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_html_is_html_document() {
        // Smoke: settings placeholder is at least a complete document so
        // engines that try to render it before a provider is wired don't
        // show the "URL can't be shown" error.
        let bytes = default_settings_html();
        let s = std::str::from_utf8(&bytes).expect("settings html is utf-8");
        assert!(
            s.starts_with("<!DOCTYPE html>"),
            "want DOCTYPE, got: {s:.80}"
        );
        assert!(s.contains("<title>"), "settings html should set a title");
    }
}
