//! buffr internal new-tab page served via the `buffr://` custom scheme.
//!
//! CEF requires custom schemes to be registered **before** `cef::initialize`
//! (via `App::on_register_custom_schemes`). After init, we register a
//! [`SchemeHandlerFactory`] that serves a static HTML page for any
//! `buffr://` URL.
//!
//! # Usage
//!
//! 1. Call [`register_buffr_scheme`] from `on_register_custom_schemes`.
//! 2. Call [`register_buffr_handler_factory`] once after `cef::initialize`
//!    succeeds.
//! 3. Use [`NEW_TAB_URL`] wherever a new-tab URL is needed.

// The wrap_* macros expand to references to bare identifiers like
// `ImplSchemeHandlerFactory`, `WrapSchemeHandlerFactory`, `ResourceHandler`,
// etc. — mirroring how `app.rs` uses `use cef::*`.
use cef::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The URL opened when the user presses `t` (TabNew).
pub const NEW_TAB_URL: &str = "buffr://new";

/// Embedded new-tab HTML — included at compile time from the assets directory.
static NEW_TAB_HTML: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/new_tab.html"));

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
/// Must be called **after** `cef::initialize` returns successfully.
pub fn register_buffr_handler_factory() {
    let scheme = CefString::from("buffr");
    let mut factory = BuffrSchemeHandlerFactory::new();
    cef::register_scheme_handler_factory(Some(&scheme), None, Some(&mut factory));
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

wrap_scheme_handler_factory! {
    pub struct BuffrSchemeHandlerFactory;

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            _scheme_name: Option<&CefString>,
            _request: Option<&mut cef::Request>,
        ) -> Option<cef::ResourceHandler> {
            Some(BuffrResourceHandler::new(Arc::new(AtomicUsize::new(0))))
        }
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

wrap_resource_handler! {
    pub struct BuffrResourceHandler {
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
                *len = NEW_TAB_HTML.len() as i64;
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
            let len = NEW_TAB_HTML.len();
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
                    NEW_TAB_HTML.as_ptr().add(pos),
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
