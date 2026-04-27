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
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use buffr_config::DownloadsConfig;
use buffr_downloads::{DownloadStatus, Downloads};
use buffr_history::{History, Transition};
use buffr_permissions::{Decision, Permissions};
use buffr_zoom::ZoomStore;

use crate::download_notice::{DownloadNotice, DownloadNoticeKind, DownloadNoticeQueue, push};
use crate::edit::{EditEventSink, build_inject_script as build_edit_inject_script};
use crate::find::{FindResult, FindResultSink};
use crate::hint::{HintEventSink, parse_console_event};
use crate::permissions::{
    PendingPermission, PermissionsQueue, capabilities_for_media_mask,
    capabilities_for_request_mask, precheck,
};
use crate::telemetry::{KEY_DOWNLOADS_COMPLETED, KEY_PAGES_LOADED, UsageCounters};
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
#[allow(clippy::too_many_arguments)]
pub fn make_client(
    history: Arc<History>,
    downloads: Arc<Downloads>,
    downloads_config: Arc<DownloadsConfig>,
    zoom: Arc<ZoomStore>,
    permissions: Arc<Permissions>,
    permissions_queue: PermissionsQueue,
    find_sink: FindResultSink,
    hint_sink: HintEventSink,
    edit_sink: EditEventSink,
    counters: Option<Arc<UsageCounters>>,
    notice_queue: DownloadNoticeQueue,
) -> Client {
    BuffrClient::new(
        history,
        downloads,
        downloads_config,
        zoom,
        permissions,
        permissions_queue,
        find_sink,
        hint_sink,
        edit_sink,
        counters,
        notice_queue,
    )
}

/// Standalone factory for the load handler — exposed so future
/// `BrowserHost` flavors (OSR, multi-tab) can build their own client
/// while still funnelling visits into the same history store.
pub fn make_load_handler(
    history: Arc<History>,
    zoom: Arc<ZoomStore>,
    counters: Option<Arc<UsageCounters>>,
    edit_sink: EditEventSink,
) -> LoadHandler {
    BuffrLoadHandler::new(
        history,
        zoom,
        counters,
        Arc::new(Mutex::new(HashMap::new())),
        edit_sink,
    )
}

/// Standalone factory for the display handler — same rationale as
/// [`make_load_handler`].
pub fn make_display_handler(
    history: Arc<History>,
    hint_sink: HintEventSink,
    edit_sink: EditEventSink,
) -> DisplayHandler {
    BuffrDisplayHandler::new(history, hint_sink, edit_sink)
}

/// Standalone factory for the download handler.
pub fn make_download_handler(
    downloads: Arc<Downloads>,
    downloads_config: Arc<DownloadsConfig>,
    counters: Option<Arc<UsageCounters>>,
    notice_queue: DownloadNoticeQueue,
) -> DownloadHandler {
    BuffrDownloadHandler::new(downloads, downloads_config, counters, notice_queue)
}

/// Standalone factory for the find handler. Takes the same
/// [`FindResultSink`] [`BrowserHost`] uses so callbacks land in one
/// place.
pub fn make_find_handler(sink: FindResultSink) -> FindHandler {
    BuffrFindHandler::new(sink)
}

/// Standalone factory for the permission handler. Pre-checks the
/// store synchronously; otherwise enqueues the request for the UI
/// thread.
pub fn make_permission_handler(
    permissions: Arc<Permissions>,
    queue: PermissionsQueue,
) -> PermissionHandler {
    BuffrPermissionHandler::new(permissions, queue)
}

wrap_client! {
    pub struct BuffrClient {
        history: Arc<History>,
        downloads: Arc<Downloads>,
        downloads_config: Arc<DownloadsConfig>,
        zoom: Arc<ZoomStore>,
        permissions: Arc<Permissions>,
        permissions_queue: PermissionsQueue,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        edit_sink: EditEventSink,
        counters: Option<Arc<UsageCounters>>,
        notice_queue: DownloadNoticeQueue,
    }

    impl Client {
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(BuffrLoadHandler::new(
                self.history.clone(),
                self.zoom.clone(),
                self.counters.clone(),
                Arc::new(Mutex::new(HashMap::new())),
                self.edit_sink.clone(),
            ))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(BuffrDisplayHandler::new(
                self.history.clone(),
                self.hint_sink.clone(),
                self.edit_sink.clone(),
            ))
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(BuffrDownloadHandler::new(
                self.downloads.clone(),
                self.downloads_config.clone(),
                self.counters.clone(),
                self.notice_queue.clone(),
            ))
        }

        fn find_handler(&self) -> Option<FindHandler> {
            Some(BuffrFindHandler::new(self.find_sink.clone()))
        }

        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(BuffrPermissionHandler::new(
                self.permissions.clone(),
                self.permissions_queue.clone(),
            ))
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
        counters: Option<Arc<UsageCounters>>,
        pending_transitions: Arc<Mutex<HashMap<i32, Transition>>>,
        // Shared with BuffrDisplayHandler: write the injected edit.js
        // script on load; display handler reads events from the queue.
        edit_sink: EditEventSink,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            transition_type: TransitionType,
        ) {
            // Only track main-frame navigations; subframes never reach
            // `record_visit` in `on_load_end` anyway.
            let Some(frame) = frame else { return };
            if frame.is_main() == 0 {
                return;
            }
            let Some(browser) = browser else { return };
            let id = cef::ImplBrowser::identifier(browser);
            let transition = decode_transition(transition_type);
            if let Ok(mut map) = self.pending_transitions.lock() {
                map.insert(id, transition);
            }
        }

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
            // Phase 6 telemetry: count one main-frame load. Gated on
            // the same `is_main` check as the history recorder so
            // counts and history rows stay in sync.
            if let Some(c) = self.counters.as_ref() {
                c.increment(KEY_PAGES_LOADED);
            }
            // Retrieve the transition stashed by `on_load_start`. If
            // none is present (e.g. the load started before this
            // handler was wired), fall back to `Link`.
            let transition = browser
                .as_ref()
                .and_then(|b| {
                    let id = cef::ImplBrowser::identifier(*b);
                    self.pending_transitions.lock().ok()?.remove(&id)
                })
                .unwrap_or(Transition::Link);
            if let Err(err) =
                self.history.record_visit(&url, None, transition)
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

            // Edit-mode Stage 1: inject edit.js once per main-frame load.
            // The script is idempotent (`window.__buffrEditWired` guard)
            // so SPA soft-navigations that re-trigger on_load_end are safe.
            // We gate on `frame.is_main()` (already checked above) so
            // iframes and subframes never get the listener installed.
            let script = build_edit_inject_script();
            let cef_script = CefString::from(script.as_str());
            let cef_url = CefString::from("buffr://edit-inject");
            frame.execute_java_script(Some(&cef_script), Some(&cef_url), 1);
            tracing::trace!(%url, "edit: injected edit.js");
            // `self.edit_sink` is held for Stage 2 sink ownership; the
            // display handler writes into it when console events arrive.
            let _ = &self.edit_sink;
        }
    }
}

/// Map a CEF [`TransitionType`] to our [`Transition`] enum.
///
/// Source bits live in the low byte (mask `0xFF`). Flag bits above that
/// are ignored — a reload navigated via forward/back is still a reload.
fn decode_transition(tt: TransitionType) -> Transition {
    // Cast via i32 (the C enum's underlying repr) then widen to u32 so
    // negative-looking flag constants don't sign-extend strangely.
    let raw = (*tt.as_ref() as i32) as u32;
    decode_transition_raw(raw)
}

/// Inner decoder operating on the raw `u32` representation of a
/// `cef_transition_type_t`. Separated so tests can pass arbitrary
/// bitwise combinations without constructing invalid enum values.
fn decode_transition_raw(raw: u32) -> Transition {
    use cef::sys::cef_transition_type_t as T;
    // Strip qualifier flags; keep only the source nibble.
    let source = raw & 0xFF;
    // Compare against known discriminants. Flag constants (TT_BLOCKED_FLAG
    // etc.) live well above 0xFF, so masking is safe.
    if source == T::TT_RELOAD as u32 {
        Transition::Reload
    } else if source == T::TT_FORM_SUBMIT as u32 {
        Transition::FormSubmit
    } else if source == T::TT_GENERATED as u32
        || source == T::TT_KEYWORD as u32
        || source == T::TT_KEYWORD_GENERATED as u32
    {
        Transition::Generated
    } else if source == T::TT_LINK as u32 {
        Transition::Link
    } else {
        // TT_EXPLICIT, TT_AUTO_BOOKMARK, TT_AUTO_TOPLEVEL,
        // TT_AUTO_SUBFRAME, TT_MANUAL_SUBFRAME, TT_NUM_VALUES, …
        Transition::Other
    }
}

wrap_display_handler! {
    pub struct BuffrDisplayHandler {
        history: Arc<History>,
        hint_sink: HintEventSink,
        // Receives parsed EditConsoleEvents scraped from
        // `__buffr_edit__:`-prefixed console lines. Stage 2 drains
        // this from the UI render loop to drive EditSession lifecycle.
        edit_sink: EditEventSink,
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

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // IPC fallback: injected JS writes sentinel-prefixed lines
            // via `console.log`. We scrape them here and route to the
            // appropriate sink. Returning 0 lets CEF continue logging;
            // returning 1 would suppress the message from devtools.
            let Some(message) = message else { return 0; };
            let text = message.to_string();

            // ---- hint mode IPC ------------------------------------------
            // hint.js emits `__buffr_hint__:{...}` lines.
            if let Some(parsed) = parse_console_event(&text) {
                match parsed {
                    Ok(event) => {
                        if let Ok(mut guard) = self.hint_sink.lock() {
                            *guard = Some(event);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, line = %text, "hint: malformed console event");
                    }
                }
            }

            // ---- edit mode IPC ------------------------------------------
            // edit.js emits `__buffr_edit__:{...}` lines on focus/blur/mutate.
            if let Some(parsed) = crate::edit::parse_console_event(&text) {
                match parsed {
                    Ok(event) => {
                        if let Ok(mut guard) = self.edit_sink.lock() {
                            guard.push_back(event);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, line = %text, "edit: malformed console event");
                    }
                }
            }

            0
        }
    }
}

wrap_download_handler! {
    pub struct BuffrDownloadHandler {
        downloads: Arc<Downloads>,
        config: Arc<DownloadsConfig>,
        counters: Option<Arc<UsageCounters>>,
        notice_queue: DownloadNoticeQueue,
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

            // Push a Started notice only for the silent (default_dir)
            // path. When ask_each_time=true the OS native Save-As dialog
            // already provides feedback to the user.
            if !self.config.ask_each_time && self.config.show_notifications {
                push(
                    &self.notice_queue,
                    DownloadNotice {
                        kind: DownloadNoticeKind::Started,
                        filename: suggested.clone(),
                        path: target_path.to_string_lossy().into_owned(),
                        created_at: std::time::Instant::now(),
                    },
                );
            }

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
                // Phase 6 telemetry: count one completed download.
                // Failed/canceled downloads do not increment.
                if let Some(c) = self.counters.as_ref() {
                    c.increment(KEY_DOWNLOADS_COMPLETED);
                }
                // Completed notice — only for the silent path.
                if !self.config.ask_each_time && self.config.show_notifications {
                    push(
                        &self.notice_queue,
                        DownloadNotice {
                            kind: DownloadNoticeKind::Completed,
                            filename: row.suggested_name.clone(),
                            path: path_str.clone(),
                            created_at: std::time::Instant::now(),
                        },
                    );
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
                // Canceled = user-initiated; skip notification noise.
                return;
            }

            if item.is_interrupted() != 0 {
                let reason = format!("interrupted (code {:?})", item.interrupt_reason());
                if let Err(err) = self.downloads.record_failed(row.id, &reason) {
                    tracing::warn!(error = %err, "downloads: record_failed failed");
                }
                // Failed notice.
                if !self.config.ask_each_time && self.config.show_notifications {
                    push(
                        &self.notice_queue,
                        DownloadNotice {
                            kind: DownloadNoticeKind::Failed,
                            filename: row.suggested_name.clone(),
                            path: String::new(),
                            created_at: std::time::Instant::now(),
                        },
                    );
                }
                return;
            }

            if let Err(err) = self.downloads.update_progress(row.id, received_u, total_u) {
                tracing::warn!(error = %err, "downloads: update_progress failed");
            }
        }
    }
}

wrap_permission_handler! {
    pub struct BuffrPermissionHandler {
        permissions: Arc<Permissions>,
        queue: PermissionsQueue,
    }

    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            // CEF emits this for `getUserMedia` (camera/mic). Returning
            // 0 hands the request back to CEF (which will deny by
            // default in headless builds); returning 1 commits us to
            // invoking the callback exactly once.
            let Some(callback) = callback else {
                tracing::warn!("permissions: media-access callback was None");
                return 0;
            };
            let origin = requesting_origin
                .map(|s| s.to_string())
                .unwrap_or_default();
            let caps = capabilities_for_media_mask(requested_permissions);
            if caps.is_empty() {
                // Nothing buffr knows how to ask about — let CEF apply
                // its default policy.
                tracing::trace!(
                    %origin,
                    requested_permissions,
                    "permissions: media request with no recognised bits — declining"
                );
                callback.cancel();
                return 1;
            }

            // Pre-check the store. Sticky decisions short-circuit the
            // prompt: every cap must agree (all-allow or any-deny).
            match precheck(&self.permissions, &origin, &caps) {
                Ok(Some(Decision::Allow)) => {
                    callback.cont(requested_permissions);
                    return 1;
                }
                Ok(Some(Decision::Deny)) => {
                    callback.cancel();
                    return 1;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(error = %err, %origin, "permissions: precheck failed — falling through to prompt");
                }
            }

            // Enqueue. We clone the callback (RefGuard::Clone bumps
            // refcount) so it survives until the UI thread resolves
            // the request.
            let pending = PendingPermission::MediaAccess {
                origin,
                capabilities: caps,
                callback: callback.clone(),
                requested_mask: requested_permissions,
            };
            if let Ok(mut q) = self.queue.lock() {
                q.push_back(pending);
            } else {
                tracing::warn!("permissions: queue mutex poisoned — denying");
                callback.cancel();
            }
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else {
                tracing::warn!("permissions: prompt callback was None");
                return 0;
            };
            let origin = requesting_origin
                .map(|s| s.to_string())
                .unwrap_or_default();
            let caps = capabilities_for_request_mask(requested_permissions);
            if caps.is_empty() {
                tracing::trace!(
                    %origin,
                    requested_permissions,
                    "permissions: prompt request with no recognised bits — dismissing"
                );
                callback.cont(PermissionRequestResult::DISMISS);
                return 1;
            }

            match precheck(&self.permissions, &origin, &caps) {
                Ok(Some(Decision::Allow)) => {
                    callback.cont(PermissionRequestResult::ACCEPT);
                    return 1;
                }
                Ok(Some(Decision::Deny)) => {
                    callback.cont(PermissionRequestResult::DENY);
                    return 1;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(error = %err, %origin, "permissions: precheck failed — falling through to prompt");
                }
            }

            let pending = PendingPermission::Prompt {
                origin,
                capabilities: caps,
                callback: callback.clone(),
                prompt_id,
            };
            if let Ok(mut q) = self.queue.lock() {
                q.push_back(pending);
            } else {
                tracing::warn!("permissions: queue mutex poisoned — dismissing");
                callback.cont(PermissionRequestResult::DISMISS);
            }
            1
        }

        fn on_dismiss_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            _result: PermissionRequestResult,
        ) {
            // Fired when CEF cancels the prompt itself (e.g. tab
            // navigated away). We don't have a stable identifier on
            // the queue entry yet, so this is informational — the
            // pending entry will eventually be resolved by the user or
            // by `drain_with_defer` at shutdown.
            tracing::trace!(prompt_id, "permissions: dismissed by CEF");
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
    use cef::sys::cef_transition_type_t as T;

    // Test `decode_transition_raw` directly: pass arbitrary bitwise
    // combinations without constructing invalid `cef_transition_type_t`
    // enum discriminants.
    fn raw(source: T) -> u32 {
        source as u32
    }

    fn raw_flagged(source: T, flag: T) -> u32 {
        source as u32 | flag as u32
    }

    #[test]
    fn decode_transition_link() {
        assert_eq!(decode_transition_raw(raw(T::TT_LINK)), Transition::Link);
    }

    #[test]
    fn decode_transition_reload() {
        assert_eq!(decode_transition_raw(raw(T::TT_RELOAD)), Transition::Reload);
    }

    #[test]
    fn decode_transition_form_submit() {
        assert_eq!(
            decode_transition_raw(raw(T::TT_FORM_SUBMIT)),
            Transition::FormSubmit
        );
    }

    #[test]
    fn decode_transition_generated_variants() {
        assert_eq!(
            decode_transition_raw(raw(T::TT_GENERATED)),
            Transition::Generated
        );
        assert_eq!(
            decode_transition_raw(raw(T::TT_KEYWORD)),
            Transition::Generated
        );
        assert_eq!(
            decode_transition_raw(raw(T::TT_KEYWORD_GENERATED)),
            Transition::Generated
        );
    }

    #[test]
    fn decode_transition_other_variants() {
        assert_eq!(
            decode_transition_raw(raw(T::TT_EXPLICIT)),
            Transition::Other
        );
        assert_eq!(
            decode_transition_raw(raw(T::TT_AUTO_TOPLEVEL)),
            Transition::Other
        );
        assert_eq!(
            decode_transition_raw(raw(T::TT_AUTO_BOOKMARK)),
            Transition::Other
        );
        assert_eq!(
            decode_transition_raw(raw(T::TT_AUTO_SUBFRAME)),
            Transition::Other
        );
        assert_eq!(
            decode_transition_raw(raw(T::TT_MANUAL_SUBFRAME)),
            Transition::Other
        );
    }

    #[test]
    fn decode_transition_flag_bits_stripped() {
        // TT_LINK | TT_FORWARD_BACK_FLAG — flag bits must not change the source.
        assert_eq!(
            decode_transition_raw(raw_flagged(T::TT_LINK, T::TT_FORWARD_BACK_FLAG)),
            Transition::Link
        );
        // TT_RELOAD | TT_DIRECT_LOAD_FLAG — still a reload.
        assert_eq!(
            decode_transition_raw(raw_flagged(T::TT_RELOAD, T::TT_DIRECT_LOAD_FLAG)),
            Transition::Reload
        );
    }

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
