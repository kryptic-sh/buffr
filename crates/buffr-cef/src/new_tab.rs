//! buffr internal pages served via the `buffr://` custom scheme.
//!
//! CEF requires custom schemes to be registered **before** `cef::initialize`
//! (via `App::on_register_custom_schemes`). After init, we register a
//! [`SchemeHandlerFactory`] that routes `buffr://` URLs:
//!
//! | URL                   | Content                                   |
//! |---------------------- |------------------------------------------ |
//! | `buffr://new`         | New-tab page (dynamic keybinds/splash).   |
//! | `buffr://settings`    | Settings scaffold (engine routing info).  |
//! | anything else         | Falls back to the new-tab page.           |
//!
//! # Usage
//!
//! 1. Call [`register_buffr_scheme`] from `on_register_custom_schemes`.
//! 2. Call [`register_buffr_handler_factory`] once after `cef::initialize`
//!    succeeds.
//! 3. Use [`NEW_TAB_URL`] wherever a new-tab URL is needed.
//! 4. Use [`SETTINGS_URL`] wherever the settings page URL is needed.

// The wrap_* macros expand to references to bare identifiers like
// `ImplSchemeHandlerFactory`, `WrapSchemeHandlerFactory`, `ResourceHandler`,
// etc. — mirroring how `app.rs` uses `use cef::*`.
use cef::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The URL opened when the user presses `t` (TabNew).
pub const NEW_TAB_URL: &str = "buffr://new";

/// The URL for the engine-routing settings scaffold.
pub const SETTINGS_URL: &str = "buffr://settings";

/// Embedded new-tab HTML template. Contains a `<!--KEYBINDS-->` marker
/// that the apps layer fills in with rendered keybindings each time
/// the page is requested, so a config hot-reload is reflected on the
/// next visit without a binary rebuild.
pub static NEW_TAB_HTML_TEMPLATE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/new_tab.html"));

/// The marker the apps layer replaces with rendered keybinding rows.
pub const NEW_TAB_KEYBINDS_MARKER: &str = "<!--KEYBINDS-->";

/// Marker the apps layer replaces with the static splash wordmark
/// (per-cell HTML grid). Substituted once per page request alongside
/// the keybindings; the splash overlay (`#buffr-splash`) is updated
/// per tick by the host via execute_javascript.
pub const NEW_TAB_SPLASH_ART_MARKER: &str = "<!--SPLASH-ART-->";

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

/// Build a static settings provider that serves placeholder content.
/// Used when the apps layer hasn't wired a live provider yet.
fn static_settings_provider() -> SettingsHtmlProvider {
    Arc::new(|| settings_html(&[], &[]))
}

/// Fallback provider — serves the raw template (with the keybinds
/// marker still in it). Used by callers that don't supply a renderer
/// and by tests.
fn static_provider() -> NewTabHtmlProvider {
    Arc::new(|| NEW_TAB_HTML_TEMPLATE.as_bytes().to_vec())
}

/// Register the `buffr` scheme with CEF.
///
/// Must be called from within `ImplApp::on_register_custom_schemes` **before**
/// `cef::initialize`.
pub fn register_buffr_scheme(registrar: &mut cef::SchemeRegistrar) {
    let scheme = CefString::from("buffr");
    // Standard + Secure + CORS-enabled + Fetch-enabled mirrors the flags
    // Chromium gives its own chrome:// scheme.
    let opts = (SchemeOptions::STANDARD.get_raw()
        | SchemeOptions::SECURE.get_raw()
        | SchemeOptions::CORS_ENABLED.get_raw()
        | SchemeOptions::FETCH_ENABLED.get_raw()) as i32;
    registrar.add_custom_scheme(Some(&scheme), opts);
}

/// Register the scheme handler factory for `buffr://`.
///
/// `provider` is invoked on every `buffr://new` (and unknown path) request.
/// `settings_provider`, if `Some`, is invoked for `buffr://settings`; when
/// `None` a static placeholder is used. Must be called **after**
/// `cef::initialize` returns successfully.
pub fn register_buffr_handler_factory(provider: NewTabHtmlProvider) {
    register_buffr_handler_factory_with_settings(provider, None);
}

/// Register with both a new-tab provider and a settings page provider.
///
/// `settings_provider` is invoked on each `buffr://settings` request so the
/// page can reflect live router state without a restart.
pub fn register_buffr_handler_factory_with_settings(
    provider: NewTabHtmlProvider,
    settings_provider: Option<SettingsHtmlProvider>,
) {
    let settings = settings_provider.unwrap_or_else(static_settings_provider);
    let scheme = CefString::from("buffr");
    let mut factory = BuffrSchemeHandlerFactory::new(provider, settings);
    cef::register_scheme_handler_factory(Some(&scheme), None, Some(&mut factory));
}

/// Register with the static template only (no dynamic content). Useful
/// in tests and as a stop-gap before the apps layer wires its renderer
/// in.
pub fn register_buffr_handler_factory_static() {
    register_buffr_handler_factory(static_provider());
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

wrap_scheme_handler_factory! {
    pub struct BuffrSchemeHandlerFactory {
        provider: NewTabHtmlProvider,
        settings_provider: SettingsHtmlProvider,
    }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut cef::Request>,
        ) -> Option<cef::ResourceHandler> {
            // Dispatch based on the requested URL path.
            // `buffr://settings` → settings scaffold.
            // Everything else (including `buffr://new`) → new-tab provider.
            let url = request
                .map(|r| CefStringUtf16::from(&r.url()).to_string())
                .unwrap_or_default();
            let bytes = if url.starts_with("buffr://settings") {
                (self.settings_provider)()
            } else {
                (self.provider)()
            };
            Some(BuffrResourceHandler::new(
                Arc::new(bytes),
                Arc::new(AtomicUsize::new(0)),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

wrap_resource_handler! {
    pub struct BuffrResourceHandler {
        bytes: Arc<Vec<u8>>,
        cursor: Arc<AtomicUsize>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut cef::Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut cef::Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(hr) = handle_request {
                *hr = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            if let Some(r) = response {
                r.set_status(200);
                let mime = CefString::from("text/html");
                r.set_mime_type(Some(&mime));
            }
            if let Some(len) = response_length {
                *len = self.bytes.len() as i64;
            }
        }

        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut cef::ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let len = self.bytes.len();
            let pos = self.cursor.load(Ordering::SeqCst);
            if pos >= len || bytes_to_read <= 0 {
                if let Some(br) = bytes_read {
                    *br = 0;
                }
                // Return 0 to signal EOF — CEF stops calling read.
                return 0;
            }
            let remaining = len - pos;
            let to_copy = remaining.min(bytes_to_read as usize);
            // Safety: CEF guarantees `data_out` is a valid writable buffer
            // of at least `bytes_to_read` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.bytes.as_ptr().add(pos),
                    data_out,
                    to_copy,
                );
            }
            self.cursor.store(pos + to_copy, Ordering::SeqCst);
            if let Some(br) = bytes_read {
                *br = to_copy as i32;
            }
            1
        }
    }
}
