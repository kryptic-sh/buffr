//! CEF callback handlers that bridge browser events into buffr's
//! data layer.
//!
//! Phase 5 wires up two:
//!
//! - [`make_load_handler`] — `LoadHandler::on_load_end` records every
//!   main-frame navigation into [`buffr_history::History`].
//! - [`make_display_handler`] — `DisplayHandler::on_title_change`
//!   updates the most recent visit's title via
//!   [`buffr_history::History::update_latest_title`]. CEF emits
//!   `on_title_change` slightly after `on_load_end`, so the visit row
//!   already exists.
//!
//! Both are exposed through [`make_client`], which spins a tiny
//! `BuffrClient` whose only job is to hand the load + display handlers
//! to CEF when it asks. `BrowserHost::new` passes the resulting
//! `Client` to `browser_host_create_browser_sync` so CEF actually
//! invokes our callbacks (without a custom `Client`, CEF defaults to
//! a no-op client and our handlers never fire).

use std::sync::Arc;

use buffr_history::{History, Transition};
// `wrap_client!` / `wrap_load_handler!` / `wrap_display_handler!`
// expand to references to bare `Client`, `WrapClient`, `ImplClient`,
// `LoadHandler`, etc., so the upstream cef-rs examples (and our
// `app.rs`) glob-import the whole crate. We do the same here.
use cef::*;

/// Build a CEF `Client` that returns our load + display handlers when
/// CEF asks for them. This is the entry point `BrowserHost::new`
/// uses; consumers don't construct `BuffrClient` directly.
pub fn make_client(history: Arc<History>) -> Client {
    BuffrClient::new(history)
}

/// Standalone factory for the load handler — exposed so future
/// `BrowserHost` flavors (OSR, multi-tab) can build their own client
/// while still funnelling visits into the same history store.
pub fn make_load_handler(history: Arc<History>) -> LoadHandler {
    BuffrLoadHandler::new(history)
}

/// Standalone factory for the display handler — same rationale as
/// [`make_load_handler`].
pub fn make_display_handler(history: Arc<History>) -> DisplayHandler {
    BuffrDisplayHandler::new(history)
}

wrap_client! {
    pub struct BuffrClient {
        history: Arc<History>,
    }

    impl Client {
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(BuffrLoadHandler::new(self.history.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(BuffrDisplayHandler::new(self.history.clone()))
        }
    }
}

wrap_load_handler! {
    pub struct BuffrLoadHandler {
        history: Arc<History>,
    }

    impl LoadHandler {
        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: ::std::os::raw::c_int,
        ) {
            // Subframes (iframes, embedded ads, etc.) must not pollute
            // history. CEF fires `on_load_end` per frame; gate on
            // `is_main` so we only record the top-level document.
            let Some(frame) = frame else { return };
            if frame.is_main() == 0 {
                return;
            }
            let url = CefStringUtf16::from(&frame.url()).to_string();
            // Phase 5 always records as `Link`. Differentiating
            // `Reload` requires hooking `on_load_start`'s
            // `transition_type` — punted to Phase 5 follow-up so we
            // don't conflate the dedupe + transition wiring.
            if let Err(err) =
                self.history.record_visit(&url, None, Transition::Link)
            {
                tracing::warn!(error = %err, %url, "history: record_visit failed");
            }
        }
    }
}

wrap_display_handler! {
    pub struct BuffrDisplayHandler {
        history: Arc<History>,
    }

    impl DisplayHandler {
        fn on_title_change(
            &self,
            browser: Option<&mut Browser>,
            title: Option<&CefString>,
        ) {
            let Some(browser) = browser else { return };
            let Some(title) = title else { return };
            // `browser.main_frame()` returns the live main frame; we
            // need its URL so the title attaches to the right row.
            let frame = match cef::ImplBrowser::main_frame(browser) {
                Some(f) => f,
                None => return,
            };
            let url = CefStringUtf16::from(&frame.url()).to_string();
            let title = title.to_string();
            if let Err(err) = self.history.update_latest_title(&url, &title) {
                tracing::warn!(error = %err, %url, "history: update_latest_title failed");
            }
        }
    }
}
