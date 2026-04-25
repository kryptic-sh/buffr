//! CEF callback handlers that bridge browser events into buffr's
//! data layer.
//!
//! Phase 5 wires up three:
//!
//! - [`make_load_handler`] — `LoadHandler::on_load_end` records every
//!   main-frame navigation into [`buffr_history::History`].
//! - [`make_display_handler`] — `DisplayHandler::on_title_change`
//!   updates the most recent visit's title via
//!   [`buffr_history::History::update_latest_title`]. CEF emits
//!   `on_title_change` slightly after `on_load_end`, so the visit row
//!   already exists.
//! - [`make_download_handler`] — `DownloadHandler::on_before_download`
//!   resolves a target path under
//!   [`buffr_config::DownloadsConfig::default_dir`] and routes
//!   progress / lifecycle ticks into [`buffr_downloads::Downloads`].
//!
//! All three are exposed through [`make_client`], which spins a tiny
//! `BuffrClient` whose only job is to hand the load + display +
//! download handlers to CEF when it asks. `BrowserHost::new` passes
//! the resulting `Client` to `browser_host_create_browser_sync` so CEF
//! actually invokes our callbacks (without a custom `Client`, CEF
//! defaults to a no-op client and our handlers never fire).

use std::path::PathBuf;
use std::sync::Arc;

use buffr_config::DownloadsConfig;
use buffr_downloads::{DownloadStatus, Downloads};
use buffr_history::{History, Transition};
use buffr_zoom::ZoomStore;

use crate::find::{FindResult, FindResultSink};
// `wrap_client!` / `wrap_load_handler!` / `wrap_display_handler!` /
// `wrap_download_handler!` expand to references to bare `Client`,
// `WrapClient`, `ImplClient`, `LoadHandler`, `DownloadHandler`, etc.,
// so the upstream cef-rs examples (and our `app.rs`) glob-import the
// whole crate. We do the same here.
use cef::*;

use crate::open_finder::{OsSpawn, open_path};

/// Build a CEF `Client` that returns our load + display + download
/// handlers when CEF asks for them. This is the entry point
/// `BrowserHost::new` uses; consumers don't construct `BuffrClient`
/// directly.
pub fn make_client(
    history: Arc<History>,
    downloads: Arc<Downloads>,
    downloads_config: Arc<DownloadsConfig>,
    zoom: Arc<ZoomStore>,
    find_sink: FindResultSink,
) -> Client {
    BuffrClient::new(history, downloads, downloads_config, zoom, find_sink)
}

/// Standalone factory for the load handler — exposed so future
/// `BrowserHost` flavors (OSR, multi-tab) can build their own client
/// while still funnelling visits into the same history store.
pub fn make_load_handler(history: Arc<History>, zoom: Arc<ZoomStore>) -> LoadHandler {
    BuffrLoadHandler::new(history, zoom)
}

/// Standalone factory for the display handler — same rationale as
/// [`make_load_handler`].
pub fn make_display_handler(history: Arc<History>) -> DisplayHandler {
    BuffrDisplayHandler::new(history)
}

/// Standalone factory for the download handler.
pub fn make_download_handler(
    downloads: Arc<Downloads>,
    downloads_config: Arc<DownloadsConfig>,
) -> DownloadHandler {
    BuffrDownloadHandler::new(downloads, downloads_config)
}

/// Standalone factory for the find handler. Takes the same
/// [`FindResultSink`] [`BrowserHost`] uses so callbacks land in one
/// place.
pub fn make_find_handler(sink: FindResultSink) -> FindHandler {
    BuffrFindHandler::new(sink)
}

wrap_client! {
    pub struct BuffrClient {
        history: Arc<History>,
        downloads: Arc<Downloads>,
        downloads_config: Arc<DownloadsConfig>,
        zoom: Arc<ZoomStore>,
        find_sink: FindResultSink,
    }

    impl Client {
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(BuffrLoadHandler::new(self.history.clone(), self.zoom.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(BuffrDisplayHandler::new(self.history.clone()))
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(BuffrDownloadHandler::new(
                self.downloads.clone(),
                self.downloads_config.clone(),
            ))
        }

        fn find_handler(&self) -> Option<FindHandler> {
            Some(BuffrFindHandler::new(self.find_sink.clone()))
        }
    }
}

wrap_find_handler! {
    pub struct BuffrFindHandler {
        sink: FindResultSink,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: ::std::os::raw::c_int,
            count: ::std::os::raw::c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: ::std::os::raw::c_int,
            final_update: ::std::os::raw::c_int,
        ) {
            // CEF emits a stream of partial results during a search;
            // we always overwrite the previous tick's count so the
            // statusline reflects the latest known state. `count` is
            // the total match count for the page; `active_match_ordinal`
            // is 1-based (CEF returns 0 before the first match is
            // located).
            let count = count.max(0) as u32;
            let current = active_match_ordinal.max(0) as u32;
            let result = FindResult {
                count,
                current,
                final_update: final_update != 0,
            };
            if let Ok(mut guard) = self.sink.lock() {
                *guard = Some(result);
            }
        }
    }
}

wrap_load_handler! {
    pub struct BuffrLoadHandler {
        history: Arc<History>,
        zoom: Arc<ZoomStore>,
    }

    impl LoadHandler {
        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
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

            // Restore persisted zoom level for this domain. Skip when
            // the level is 0.0 (CEF default — no point round-tripping
            // through `set_zoom_level`). On_load_end (rather than
            // on_load_start) is the safe insertion point: the frame's
            // committed URL is final, and CEF's renderer is ready to
            // accept zoom changes.
            let domain = buffr_zoom::domain_for_url(&url);
            match self.zoom.get(&domain) {
                Ok(level) if level != 0.0 => {
                    if let Some(browser) = browser
                        && let Some(host) = cef::ImplBrowser::host(browser)
                    {
                        host.set_zoom_level(level);
                        tracing::trace!(%domain, level, "zoom: applied persisted");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, %domain, "zoom: get failed");
                }
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

wrap_download_handler! {
    pub struct BuffrDownloadHandler {
        downloads: Arc<Downloads>,
        config: Arc<DownloadsConfig>,
    }

    impl DownloadHandler {
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            // Resolve a target path under the configured default_dir
            // and continue the download. Without `cont`, CEF cancels
            // the download silently.
            let Some(callback) = callback else { return 0; };
            let Some(item) = download_item else {
                // No item → can't record. Tell CEF to use its
                // built-in default path so the user still gets the
                // file.
                callback.cont(None, 1);
                return 0;
            };

            let suggested = suggested_name
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    CefStringUtf16::from(&item.suggested_file_name()).to_string()
                });
            let url = CefStringUtf16::from(&item.url()).to_string();
            let mime_str = CefStringUtf16::from(&item.mime_type()).to_string();
            let mime = if mime_str.is_empty() { None } else { Some(mime_str) };
            let total = item.total_bytes();
            let total_opt = if total > 0 { Some(total as u64) } else { None };
            let cef_id = item.id();

            let target_dir = buffr_config::resolve_default_dir(&self.config);
            // Best-effort directory creation. If this fails the user
            // will see CEF's fallback path; that's acceptable.
            let _ = std::fs::create_dir_all(&target_dir);
            let safe_name = sanitise_filename(&suggested);
            let target_path: PathBuf = target_dir.join(safe_name);

            if let Err(err) = self.downloads.record_started(
                cef_id,
                &url,
                &suggested,
                mime.as_deref(),
                total_opt,
            ) {
                tracing::warn!(error = %err, %url, "downloads: record_started failed");
            }

            let target_str = target_path.to_string_lossy();
            let target_cef = CefString::from(target_str.as_ref());
            let show_dialog = if self.config.ask_each_time { 1 } else { 0 };
            callback.cont(Some(&target_cef), show_dialog);
            0
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let cef_id = item.id();
            let row = match self.downloads.get_by_cef_id(cef_id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    // No row for this cef_id (handler races?) — log
                    // and bail. We can't fabricate a started_at.
                    tracing::trace!(cef_id, "downloads: tick for unknown cef_id");
                    return;
                }
                Err(err) => {
                    tracing::warn!(error = %err, cef_id, "downloads: get_by_cef_id failed");
                    return;
                }
            };

            // Already terminal — CEF can emit one trailing tick after
            // `is_complete`. Skip writing.
            if row.status != DownloadStatus::InFlight {
                return;
            }

            let received = item.received_bytes();
            let total = item.total_bytes();
            let received_u = if received > 0 { received as u64 } else { 0 };
            let total_u = if total > 0 { Some(total as u64) } else { None };

            if item.is_complete() != 0 {
                let path_str = CefStringUtf16::from(&item.full_path()).to_string();
                let path = PathBuf::from(&path_str);
                if let Err(err) = self.downloads.record_completed(row.id, &path) {
                    tracing::warn!(error = %err, "downloads: record_completed failed");
                    return;
                }
                if self.config.open_on_finish && !path_str.is_empty() {
                    open_path(&OsSpawn, &path);
                }
                return;
            }

            if item.is_canceled() != 0 {
                if let Err(err) = self.downloads.record_canceled(row.id) {
                    tracing::warn!(error = %err, "downloads: record_canceled failed");
                }
                return;
            }

            if item.is_interrupted() != 0 {
                let reason = format!("interrupted (code {:?})", item.interrupt_reason());
                if let Err(err) = self.downloads.record_failed(row.id, &reason) {
                    tracing::warn!(error = %err, "downloads: record_failed failed");
                }
                return;
            }

            if let Err(err) = self.downloads.update_progress(row.id, received_u, total_u) {
                tracing::warn!(error = %err, "downloads: update_progress failed");
            }
        }
    }
}

/// Strip path-traversal bits and OS-meaningful separators from a CEF
/// suggested filename. We don't attempt full sanitisation — CEF
/// already filters most malicious cases — but a `Path::file_name`
/// pass guards against `../` prefixes leaking through.
fn sanitise_filename(name: &str) -> String {
    let trimmed = name.trim();
    let stripped = std::path::Path::new(trimmed)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stripped.is_empty() {
        // Last-resort fallback — a download with no filename and no
        // way to derive one. CEF sometimes emits this for blob: URLs.
        "download".to_string()
    } else {
        stripped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_filename_strips_path() {
        assert_eq!(sanitise_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitise_filename("/tmp/foo.zip"), "foo.zip");
        assert_eq!(sanitise_filename("clean.txt"), "clean.txt");
    }

    #[test]
    fn sanitise_filename_empty_falls_back() {
        assert_eq!(sanitise_filename(""), "download");
        assert_eq!(sanitise_filename("   "), "download");
        // Pure path-traversal with no real basename also resolves to
        // the fallback after `Path::file_name` strips dot-segments.
        assert_eq!(sanitise_filename("/"), "download");
    }
}
