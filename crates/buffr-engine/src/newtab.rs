//! Engine-agnostic new-tab page constants.
//!
//! These are plain string constants — no CEF dependency. The CEF backend
//! (`buffr-cef::new_tab`) imports them and wires them into its scheme handler.
//! The apps layer can read them directly without importing `buffr-cef`.

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

/// Engine-agnostic translator: turn a `buffr://*` URL into a
/// `data:text/html;base64,…` URL that any web engine can navigate to,
/// without needing a custom URI scheme handler.
///
/// - `buffr://settings*` → invokes `settings_html` if provided, else uses
///   a built-in placeholder.
/// - `buffr://*` (anything else, including `buffr://new`, `buffr://newtab`)
///   → invokes `newtab_html` if provided, else uses
///   `NEW_TAB_HTML_TEMPLATE` verbatim.
///
/// Returns `None` for non-internal URLs (just pass them straight through to
/// the engine).
///
/// Designed for engines that can't easily register a custom URI scheme
/// (e.g. WPE WebKit's `WebKitWebContext::register_uri_scheme` is awkward
/// from FFI; Chromium-via-CDP rejects unknown schemes before `Fetch` can
/// intercept). The base64-encoded data URL keeps the resource self-contained
/// so the engine never sees `buffr://`.
pub fn translate_internal_url<F, G>(url: &str, newtab_html: F, settings_html: G) -> Option<String>
where
    F: FnOnce() -> Vec<u8>,
    G: FnOnce() -> Vec<u8>,
{
    use base64::Engine;
    if url.starts_with("buffr://settings") {
        let bytes = settings_html();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(format!("data:text/html;base64,{encoded}"))
    } else if url.starts_with("buffr://") {
        let bytes = newtab_html();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(format!("data:text/html;base64,{encoded}"))
    } else {
        None
    }
}

/// Minimal placeholder used when no settings provider is wired.
pub fn default_settings_html() -> Vec<u8> {
    b"<!DOCTYPE html><html><head><meta charset=\"utf-8\"/><title>buffr settings</title></head>\
      <body style=\"font-family:system-ui,sans-serif;background:#1a1a1a;color:#e0e0e0;margin:2rem\">\
      <h1>buffr settings</h1><p>Settings provider not configured.</p></body></html>"
        .to_vec()
}

/// Fallback used when no new-tab provider is wired — raw template without
/// keybind / splash substitution. Renders correctly but lacks the
/// host-rendered keybinds table.
pub fn default_newtab_html() -> Vec<u8> {
    NEW_TAB_HTML_TEMPLATE.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn newtab() -> Vec<u8> {
        b"<html><body>NEWTAB</body></html>".to_vec()
    }
    fn settings() -> Vec<u8> {
        b"<html><body>SETTINGS</body></html>".to_vec()
    }

    fn decode(data_url: &str) -> Vec<u8> {
        let payload = data_url
            .strip_prefix("data:text/html;base64,")
            .expect("data:text/html;base64, prefix");
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("valid base64")
    }

    #[test]
    fn translates_buffr_new_to_newtab_html() {
        let url = translate_internal_url("buffr://new", newtab, settings).expect("translated");
        assert_eq!(decode(&url), newtab());
    }

    #[test]
    fn translates_buffr_newtab_alias_to_newtab_html() {
        // `buffr://newtab` (anything past buffr:// that isn't settings) is
        // funnelled into the newtab handler — historical config files use
        // this spelling.
        let url = translate_internal_url("buffr://newtab", newtab, settings).expect("translated");
        assert_eq!(decode(&url), newtab());
    }

    #[test]
    fn translates_buffr_settings_to_settings_html() {
        let url = translate_internal_url("buffr://settings", newtab, settings).expect("translated");
        assert_eq!(decode(&url), settings());
    }

    #[test]
    fn translates_buffr_settings_subpath_to_settings_html() {
        // Anything starting with `buffr://settings` routes to settings —
        // covers future `buffr://settings/engines` etc.
        let url = translate_internal_url("buffr://settings/engines", newtab, settings)
            .expect("translated");
        assert_eq!(decode(&url), settings());
    }

    #[test]
    fn external_https_url_returns_none() {
        assert!(translate_internal_url("https://example.com", newtab, settings).is_none());
    }

    #[test]
    fn external_http_url_returns_none() {
        assert!(translate_internal_url("http://example.com", newtab, settings).is_none());
    }

    #[test]
    fn other_scheme_returns_none() {
        // file://, ftp://, custom schemes — only buffr:// is in our domain.
        assert!(translate_internal_url("file:///tmp/x.html", newtab, settings).is_none());
        assert!(translate_internal_url("about:blank", newtab, settings).is_none());
    }

    #[test]
    fn default_newtab_html_matches_template() {
        // Round-trip the embedded template; guards against accidental
        // re-encoding by a CI that touches line endings.
        assert_eq!(default_newtab_html(), NEW_TAB_HTML_TEMPLATE.as_bytes());
    }

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

    #[test]
    fn translator_invokes_provider_on_each_call() {
        // Providers must run per-request so a hot-reloaded keymap is
        // reflected on the next visit. We pass a counting closure.
        use std::cell::Cell;
        let calls = Cell::new(0);
        for _ in 0..3 {
            let _ = translate_internal_url(
                "buffr://new",
                || {
                    calls.set(calls.get() + 1);
                    newtab()
                },
                settings,
            );
        }
        assert_eq!(calls.get(), 3);
    }
}
