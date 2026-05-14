//! Custom `buffr-src:` scheme served by a [`SchemeHandlerFactory`].
//!
//! Round 2 of issue #30: stop rewriting `buffr-src:` → `view-source:` at the
//! navigation boundary. Instead, register `buffr-src` as a real CEF custom
//! scheme whose handler fetches the underlying URL on a worker thread and
//! renders it with [`buffr_view_source::render`] (bonsai syntax highlighting).
//!
//! # Usage
//!
//! 1. Call [`register_buffr_src_scheme`] from `on_register_custom_schemes`
//!    **before** `cef::initialize`.
//! 2. Call [`register_buffr_src_handler_factory`] once **after**
//!    `cef::initialize` succeeds.

use cef::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::host::BUFFR_SRC_PREFIX;

/// Register the `buffr-src` scheme with CEF.
///
/// Must be called from `ImplApp::on_register_custom_schemes` **before**
/// `cef::initialize`. Mirrors the flags used for `buffr://` in `new_tab.rs`.
pub fn register_buffr_src_scheme(registrar: &mut cef::SchemeRegistrar) {
    let scheme = CefString::from("buffr-src");
    let opts = (SchemeOptions::STANDARD.get_raw()
        | SchemeOptions::SECURE.get_raw()
        | SchemeOptions::CORS_ENABLED.get_raw()
        | SchemeOptions::FETCH_ENABLED.get_raw()) as i32;
    registrar.add_custom_scheme(Some(&scheme), opts);
}

/// Register the scheme handler factory for `buffr-src:`.
///
/// Must be called **after** `cef::initialize` returns successfully.
pub fn register_buffr_src_handler_factory() {
    let scheme = CefString::from("buffr-src");
    let mut factory = BuffrSrcSchemeHandlerFactory::new();
    cef::register_scheme_handler_factory(Some(&scheme), None, Some(&mut factory));
}

// ---------------------------------------------------------------------------
// URL helper
// ---------------------------------------------------------------------------

/// Strip the `buffr-src:` prefix from a CEF-routed URL to obtain the
/// underlying URL that should be fetched.
///
/// - `"buffr-src:https://example.com"` → `"https://example.com"`
/// - `"buffr-src:"` → `""` (empty underlying URL)
/// - Anything that does **not** start with the prefix is returned as-is
///   (defensive: CEF only routes registered-scheme URLs to this factory).
pub(crate) fn underlying_url(buffr_src_url: &str) -> &str {
    buffr_src_url
        .strip_prefix(BUFFR_SRC_PREFIX)
        .unwrap_or(buffr_src_url)
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

wrap_scheme_handler_factory! {
    pub struct BuffrSrcSchemeHandlerFactory {}

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut cef::Request>,
        ) -> Option<cef::ResourceHandler> {
            // Extract the `buffr-src:` URL from the incoming request.
            let buffr_src_url = request
                .map(|r| CefStringUtf16::from(&r.url()).to_string())
                .unwrap_or_default();

            let underlying = underlying_url(&buffr_src_url).to_owned();

            Some(BuffrSrcResourceHandler::new(
                Arc::new(Mutex::new(None)),
                underlying,
                Arc::new(AtomicUsize::new(0)),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Handler state
// ---------------------------------------------------------------------------

/// Shared rendered body. `None` until the worker thread finishes.
type BodySlot = Arc<Mutex<Option<Vec<u8>>>>;

// ---------------------------------------------------------------------------
// Resource handler
// ---------------------------------------------------------------------------

wrap_resource_handler! {
    pub struct BuffrSrcResourceHandler {
        // Rendered HTML body. Populated by the worker thread.
        body: BodySlot,
        // Underlying URL to fetch (everything after `buffr-src:`).
        underlying_url: String,
        // Read cursor into `body`.
        cursor: Arc<AtomicUsize>,
    }

    impl ResourceHandler {
        // CEF calls `open` first. Spawn a worker thread to fetch + render,
        // then call `callback.cont()` when bytes are ready.
        // Returning false (0) from `open` makes CEF wait for callback.cont().
        fn open(
            &self,
            _request: Option<&mut cef::Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            callback: Option<&mut cef::Callback>,
        ) -> ::std::os::raw::c_int {
            // Signal that we will handle this request but NOT synchronously:
            // returning `false` (0) from `open` makes CEF wait for the
            // callback before proceeding.
            if let Some(hr) = handle_request {
                *hr = 0;
            }

            let body_slot = Arc::clone(&self.body);
            let url = self.underlying_url.clone();

            // CEF callback must be called from another thread to continue
            // the resource load once the body is ready.
            //
            // Safety: cef::Callback is Send per cef-rs's design; we ship it
            // across the thread boundary via the closure.
            let callback_arc: Option<cef::Callback> = callback.map(|c| {
                // `c` is `&mut cef::Callback`; we need an owned copy.
                // The CEF wrapper objects are ref-counted, so this clone
                // is safe and keeps the callback alive on the worker.
                c.clone()
            });

            std::thread::spawn(move || {
                let html = fetch_and_render(&url);
                let bytes = html.into_bytes();
                if let Ok(mut slot) = body_slot.lock() {
                    *slot = Some(bytes);
                }
                // Tell CEF the response is ready.
                if let Some(cb) = callback_arc {
                    cb.cont();
                }
            });

            // Return 0 (false): request is pending, wait for callback.cont().
            0
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let body_len = self
                .body
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|b| b.len()))
                .unwrap_or(0);

            if let Some(r) = response {
                r.set_status(200);
                let mime = CefString::from("text/html; charset=utf-8");
                r.set_mime_type(Some(&mime));
            }
            if let Some(len) = response_length {
                *len = body_len as i64;
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
            let guard = match self.body.lock() {
                Ok(g) => g,
                Err(_) => {
                    if let Some(br) = bytes_read {
                        *br = 0;
                    }
                    return 0;
                }
            };

            let bytes = match guard.as_ref() {
                Some(b) => b,
                None => {
                    if let Some(br) = bytes_read {
                        *br = 0;
                    }
                    return 0;
                }
            };

            let len = bytes.len();
            let pos = self.cursor.load(Ordering::SeqCst);

            if pos >= len || bytes_to_read <= 0 {
                if let Some(br) = bytes_read {
                    *br = 0;
                }
                // EOF
                return 0;
            }

            let remaining = len - pos;
            let to_copy = remaining.min(bytes_to_read as usize);

            // Safety: CEF guarantees `data_out` is a valid writable buffer of
            // at least `bytes_to_read` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr().add(pos), data_out, to_copy);
            }

            self.cursor.store(pos + to_copy, Ordering::SeqCst);

            if let Some(br) = bytes_read {
                *br = to_copy as i32;
            }

            1
        }
    }
}

// ---------------------------------------------------------------------------
// Worker: fetch + render
// ---------------------------------------------------------------------------

/// Maximum response body size before aborting (10 MiB, matches the renderer).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Fetch `url` via ureq and render it with `buffr_view_source::render`.
///
/// On any error (empty URL, network failure, non-2xx, body too large)
/// returns an HTML error page so the user sees *something* useful rather
/// than a CEF error overlay.
fn fetch_and_render(url: &str) -> String {
    if url.is_empty() {
        return error_page(url, "no URL to fetch (buffr-src: prefix with empty suffix)");
    }

    let result = (|| -> Result<Vec<u8>, String> {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_recv_response(Some(std::time::Duration::from_secs(10)))
            .build();
        let agent = ureq::Agent::new_with_config(config);

        let mut resp = agent
            .get(url)
            .call()
            .map_err(|e| format!("network error: {e}"))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }

        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES as u64)
            .read_to_vec()
            .map_err(|e| format!("read error: {e}"))?;

        Ok(body)
    })();

    match result {
        Ok(body) => buffr_view_source::render(url, &body),
        Err(err) => error_page(url, &err),
    }
}

/// Build a minimal HTML error page shown when fetching fails.
fn error_page(url: &str, reason: &str) -> String {
    let escaped_url = html_escape(url);
    let escaped_reason = html_escape(reason);
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>buffr-src: error</title>
<style>
html,body{{margin:0;padding:1em 1.5em;background:#1a1b26;color:#c0caf5;
font-family:"SF Mono",Menlo,Consolas,monospace;font-size:13px;line-height:1.5}}
.err{{color:#f7768e;}}
</style>
</head>
<body>
<p class="err"><strong>Failed to fetch source for <code>{escaped_url}</code>:</strong></p>
<pre>{escaped_reason}</pre>
</body>
</html>"#
    )
}

/// Minimal HTML escaping for error page content.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underlying_url_strips_prefix() {
        assert_eq!(
            underlying_url("buffr-src:https://example.com"),
            "https://example.com"
        );
    }

    #[test]
    fn underlying_url_empty_suffix() {
        assert_eq!(underlying_url("buffr-src:"), "");
    }

    #[test]
    fn underlying_url_no_prefix_passthrough() {
        // Defensive: should not happen in practice since CEF only routes
        // registered-scheme URLs to this factory.
        assert_eq!(underlying_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn error_page_contains_url() {
        let page = error_page("https://example.com", "timeout");
        assert!(page.contains("https://example.com"));
        assert!(page.contains("timeout"));
    }

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
    }
}
