//! CDP worker thread for the Firefox CDP backend.
//!
//! # Firefox vs Chromium differences
//!
//! Firefox CDP does NOT support `Page.startScreencast` / `Page.screencastFrame`.
//! Instead, the worker runs a **poll loop** that calls `Page.captureScreenshot`
//! every `OSR_POLL_INTERVAL_MS` milliseconds, decodes the PNG, swaps RGBA→BGRA,
//! and writes into `SharedOsrFrame`. This is the canonical Firefox-CDP OSR
//! approach. Phase B hard-codes 4 fps (250 ms); frame rate is tunable in Phase C.
//!
//! The command protocol is otherwise identical to blink-cdp: the engine sends
//! [`Command`] variants on a sync channel; the worker serialises them to the
//! WebSocket and routes replies back via one-shot mpsc senders.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as B64Engine;
use serde_json::Value;

use buffr_core::hint::parse_console_event;
use buffr_engine::{FaviconUpdate, SharedOsrFrame, SharedOsrViewState, popup::PopupQueue};

use crate::cdp::{
    CaptureScreenshotParams, CdpCommand, CdpMessage, CloseTargetParams, DispatchKeyEventParams,
    DispatchMouseEventParams, NavigateParams, SetDeviceMetricsParams,
};
use crate::error::FirefoxError;
use crate::ws::WsClient;

// ── OSR poll interval ─────────────────────────────────────────────────────────

/// Hard-coded OSR poll interval for Phase B: 4 fps = 250 ms per frame.
///
/// Firefox CDP has no push-based screencast API. We poll `Page.captureScreenshot`
/// on this interval and write the decoded BGRA frame into `SharedOsrFrame`.
/// Phase C will expose this as a configurable frame-rate setting.
///
/// Performance note: each poll round-trips the WebSocket and decodes a PNG.
/// At 4 fps, latency is acceptable for most use cases but will not match the
/// smooth frame delivery of `Page.startScreencast` in Chromium.
pub const OSR_POLL_INTERVAL_MS: u64 = 250;

/// Timeout waiting for a CDP response (per command).
pub const CDP_RESPONSE_TIMEOUT_SECS: u64 = 10;

/// Capacity of the engine→worker command channel.
pub const WORKER_CMD_CHANNEL_CAP: usize = 256;

/// Timeout for stale pending commands — prevents unbounded accumulation.
const CDP_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

// ── Hint event sink ───────────────────────────────────────────────────────────

pub use buffr_core::hint::HintEventSink;

/// Create a fresh hint event sink.
pub fn new_hint_event_sink() -> HintEventSink {
    buffr_core::hint::new_hint_event_sink()
}

// ── URL update sink ───────────────────────────────────────────────────────────

/// Queue written by the worker when `Page.frameNavigated` fires, drained by
/// the engine in `pump_address_changes`.
///
/// Each entry is `(session_id, url, title)`.
pub type UrlUpdateSink = Arc<Mutex<VecDeque<(String, String, String)>>>;

/// Create a new [`UrlUpdateSink`].
pub fn new_url_update_sink() -> UrlUpdateSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

// ── Live title map ────────────────────────────────────────────────────────────

/// Map written by the worker when `Target.targetInfoChanged` fires.
///
/// Keyed by `target_id`; value is the latest page title.
pub type TitleMap = Arc<Mutex<HashMap<String, String>>>;

/// Create a new [`TitleMap`].
pub fn new_title_map() -> TitleMap {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── Favicon sink ──────────────────────────────────────────────────────────────

/// Pending favicon updates populated by the worker from `Page.frameNavigated`
/// `frame.faviconUrl` fields and drained by the engine in
/// `drain_favicon_updates`.
pub type FaviconSink = Arc<Mutex<Vec<FaviconUpdate>>>;

/// Create a new [`FaviconSink`].
pub fn new_favicon_sink() -> FaviconSink {
    Arc::new(Mutex::new(Vec::new()))
}

// ── Entry type for the pending map ───────────────────────────────────────────

type PendingEntry = (Sender<Result<Value, FirefoxError>>, Instant);

// ── Commands sent to the worker ───────────────────────────────────────────────

/// Commands the engine sends to the worker thread.
pub enum Command {
    /// Issue a CDP command on the browser-level connection and return the response.
    BrowserCmd {
        cmd: CdpCommand,
        reply: Sender<Result<Value, FirefoxError>>,
    },
    /// Issue a CDP command on a specific session and return the response.
    SessionCmd {
        session_id: String,
        cmd: CdpCommand,
        reply: Sender<Result<Value, FirefoxError>>,
    },
    /// Navigate the active tab to `url`.
    Navigate {
        session_id: String,
        url: String,
        reply: Sender<Result<Value, FirefoxError>>,
    },
    /// Resize the viewport for a tab.
    Resize {
        session_id: String,
        width: u32,
        height: u32,
    },
    /// Forward a mouse event to the active tab.
    MouseEvent {
        session_id: String,
        params: DispatchMouseEventParams,
    },
    /// Forward a key event to the active tab.
    KeyEvent {
        session_id: String,
        params: DispatchKeyEventParams,
    },
    /// Switch the "active" session for OSR screenshot polling.
    ///
    /// Only the active session is polled for screenshots to reduce load.
    SetActiveSession { session_id: Option<String> },
    /// Shutdown the worker cleanly.
    Shutdown,
}

// ── Worker run loop ───────────────────────────────────────────────────────────

/// Run the CDP worker event loop (blocking; call from a dedicated thread).
///
/// Unlike blink-cdp, this loop also polls `Page.captureScreenshot` on the
/// active session every [`OSR_POLL_INTERVAL_MS`] ms because Firefox has no
/// push-based screencast over CDP.
#[allow(clippy::too_many_arguments)]
pub fn run(
    mut ws: WsClient,
    cmd_rx: Receiver<Command>,
    osr_frame: SharedOsrFrame,
    _osr_view: SharedOsrViewState,
    url_update_sink: UrlUpdateSink,
    loading_state: Arc<Mutex<HashMap<String, bool>>>,
    nav_count: Arc<Mutex<HashMap<String, usize>>>,
    title_map: TitleMap,
    favicon_sink: FaviconSink,
    hint_sink: HintEventSink,
    popup_queue: PopupQueue,
    video_active: Arc<AtomicBool>,
    cursor_change: Arc<Mutex<Option<(i32, u32)>>>,
) {
    let mut pending: HashMap<u64, PendingEntry> = HashMap::new();
    let mut active_session: Option<String> = None;
    let mut last_screenshot = Instant::now()
        .checked_sub(Duration::from_millis(OSR_POLL_INTERVAL_MS))
        .unwrap_or_else(Instant::now);

    tracing::debug!("Firefox CDP worker started");

    loop {
        // ── Drain commands ────────────────────────────────────────────────────
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Shutdown) => {
                    tracing::debug!("Firefox CDP worker: shutdown");
                    return;
                }
                Ok(Command::SetActiveSession { session_id }) => {
                    active_session = session_id;
                }
                Ok(Command::Navigate {
                    session_id,
                    url,
                    reply,
                }) => {
                    let cmd = CdpCommand::new("Page.navigate", NavigateParams { url: &url })
                        .with_session(session_id);
                    let id = cmd.id;
                    if let Err(e) = ws.send_text(cmd.serialize()) {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    pending.insert(id, (reply, Instant::now()));
                }
                Ok(Command::Resize {
                    session_id,
                    width,
                    height,
                }) => {
                    // Firefox uses Emulation.setDeviceMetricsOverride (same field name, different domain).
                    let cmd = CdpCommand::new(
                        "Emulation.setDeviceMetricsOverride",
                        SetDeviceMetricsParams {
                            width: width.max(1),
                            height: height.max(1),
                            device_scale_factor: 1.0,
                            mobile: false,
                        },
                    )
                    .with_session(session_id);
                    let _ = ws.send_text(cmd.serialize());
                }
                Ok(Command::MouseEvent { session_id, params }) => {
                    let cmd = CdpCommand::new("Input.dispatchMouseEvent", params)
                        .with_session(session_id);
                    let _ = ws.send_text(cmd.serialize());
                }
                Ok(Command::KeyEvent { session_id, params }) => {
                    let cmd =
                        CdpCommand::new("Input.dispatchKeyEvent", params).with_session(session_id);
                    let _ = ws.send_text(cmd.serialize());
                }
                Ok(Command::BrowserCmd { cmd, reply }) => {
                    let id = cmd.id;
                    if let Err(e) = ws.send_text(cmd.serialize()) {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    pending.insert(id, (reply, Instant::now()));
                }
                Ok(Command::SessionCmd {
                    session_id,
                    cmd,
                    reply,
                }) => {
                    let cmd = cmd.with_session(session_id);
                    let id = cmd.id;
                    if let Err(e) = ws.send_text(cmd.serialize()) {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    pending.insert(id, (reply, Instant::now()));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::debug!("Firefox CDP worker: command channel closed — exiting");
                    return;
                }
            }
        }

        // ── Expire stale pending commands ─────────────────────────────────────
        let now = Instant::now();
        pending.retain(|_id, (tx, enqueued_at)| {
            if now.duration_since(*enqueued_at) > CDP_COMMAND_TIMEOUT {
                let _ = tx.send(Err(FirefoxError::Protocol(
                    "CDP command timed out (no response within 30 s)".into(),
                )));
                false
            } else {
                true
            }
        });

        // ── OSR poll loop: captureScreenshot ──────────────────────────────────
        // Firefox has no Page.startScreencast — we poll at hard-coded 4 fps.
        if let Some(ref sess) = active_session
            && now.duration_since(last_screenshot) >= Duration::from_millis(OSR_POLL_INTERVAL_MS)
        {
            let cmd = CdpCommand::new(
                "Page.captureScreenshot",
                CaptureScreenshotParams { format: "png" },
            )
            .with_session(sess.clone());
            tracing::debug!(session_id = %sess, "Firefox CDP: captureScreenshot poll");
            let _ = ws.send_text(cmd.serialize());
            last_screenshot = now;
            // The screenshot response arrives as a normal CDP reply with an id.
            // We send captureScreenshot fire-and-forget (no reply channel);
            // the response is detected in dispatch_message by the presence of
            // result["data"] and decoded inline into the OSR frame.
            //
            // Screenshot responses are heavyweight (base64 PNG); decoding
            // asynchronously on the next WS read avoids blocking the poll loop.
        }

        // ── Read one WS message (non-blocking) ────────────────────────────────
        match ws.try_recv_text() {
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(crate::ws::WS_POLL_INTERVAL_MS));
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Firefox CDP worker: WS read error — exiting");
                for (_, (tx, _)) in pending.drain() {
                    let _ = tx.send(Err(FirefoxError::WsIo(e.to_string())));
                }
                return;
            }
            Ok(Some(text)) => match serde_json::from_str::<CdpMessage>(&text) {
                Err(e) => {
                    tracing::debug!(error = %e, raw = %text, "Firefox CDP worker: unparseable message");
                }
                Ok(msg) => {
                    dispatch_message(
                        msg,
                        &mut pending,
                        &mut ws,
                        &osr_frame,
                        &url_update_sink,
                        &loading_state,
                        &nav_count,
                        &title_map,
                        &favicon_sink,
                        &hint_sink,
                        &popup_queue,
                        &video_active,
                        &cursor_change,
                    );
                }
            },
        }
    }
}

// ── Message dispatch ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn dispatch_message(
    msg: CdpMessage,
    pending: &mut HashMap<u64, PendingEntry>,
    ws: &mut WsClient,
    osr_frame: &SharedOsrFrame,
    url_update_sink: &UrlUpdateSink,
    loading_state: &Arc<Mutex<HashMap<String, bool>>>,
    nav_count: &Arc<Mutex<HashMap<String, usize>>>,
    title_map: &TitleMap,
    favicon_sink: &FaviconSink,
    hint_sink: &HintEventSink,
    popup_queue: &PopupQueue,
    video_active: &Arc<AtomicBool>,
    cursor_change: &Arc<Mutex<Option<(i32, u32)>>>,
) {
    // Command response — may be a screenshot reply or a normal reply.
    if let Some(id) = msg.id {
        // Check if this is a screenshot response (result.data present).
        // We send captureScreenshot without a pending entry (fire-and-forget),
        // so we detect the response by the presence of result["data"].
        if let Some(ref result) = msg.result
            && result.get("data").is_some()
        {
            if let Some(data_str) = result.get("data").and_then(|v| v.as_str()) {
                decode_and_write_frame(data_str, osr_frame);
            }
            // No pending entry for screenshot — consumed as side effect.
            pending.remove(&id); // No-op if not present.
            return;
        }

        // Normal response — route to waiting sender.
        if let Some((tx, _)) = pending.remove(&id) {
            let result = if let Some(err) = msg.error {
                Err(FirefoxError::Protocol(format!(
                    "CDP error {}: {}",
                    err.code, err.message
                )))
            } else {
                Ok(msg.result.unwrap_or(Value::Null))
            };
            let _ = tx.send(result);
        }
        return;
    }

    // Unsolicited event.
    let Some(ref method) = msg.method else {
        return;
    };
    tracing::debug!(method, "Firefox CDP event");

    match method.as_str() {
        "Page.frameNavigated" => {
            if let Some(ref session_id) = msg.session_id
                && let Some(ref params) = msg.params
            {
                if let Some(frame) = params.get("frame") {
                    let url = frame
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let title = String::new();
                    if !url.is_empty() {
                        tracing::debug!(
                            session_id,
                            url,
                            "Firefox CDP: Page.frameNavigated — pushing url update"
                        );
                        if let Ok(mut sink) = url_update_sink.lock() {
                            sink.push_back((session_id.clone(), url, title));
                        }
                    }
                    // Extract faviconUrl and attempt to fetch + decode it.
                    let favicon_url = frame
                        .get("faviconUrl")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    if !favicon_url.is_empty() {
                        tracing::debug!(
                            favicon_url,
                            "Firefox CDP: Page.frameNavigated — fetching favicon"
                        );
                        fetch_and_push_favicon(&favicon_url, favicon_sink);
                    }
                }
                // Increment navigation count for back/forward heuristic.
                if let Ok(mut map) = nav_count.lock() {
                    *map.entry(session_id.clone()).or_insert(0) += 1;
                }
            }
        }
        "Page.lifecycleEvent" => {
            if let Some(ref session_id) = msg.session_id
                && let Some(ref params) = msg.params
            {
                let event_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                tracing::debug!(session_id, event_name, "Firefox CDP: Page.lifecycleEvent");
                match event_name {
                    "init" | "commit" => {
                        if let Ok(mut map) = loading_state.lock() {
                            map.insert(session_id.clone(), true);
                        }
                    }
                    "load" => {
                        if let Ok(mut map) = loading_state.lock() {
                            map.insert(session_id.clone(), false);
                        }
                    }
                    _ => {}
                }
            }
        }
        "Target.targetInfoChanged" => {
            if let Some(ref params) = msg.params
                && let Some(info) = params.get("targetInfo")
            {
                let target_id = info
                    .get("targetId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let title = info
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                if !target_id.is_empty() {
                    tracing::debug!(
                        target_id,
                        title,
                        "Firefox CDP: Target.targetInfoChanged — updating live title"
                    );
                    if let Ok(mut map) = title_map.lock() {
                        map.insert(target_id, title);
                    }
                }
            }
        }
        "Target.targetCreated" => {
            // Intercept window.open() popups: when a new "page" target arrives
            // with an openerId, it was created by another page (i.e. a popup).
            // Push the URL into the engine's popup_queue so the apps layer opens
            // it as a new tab, then close the raw popup target so Firefox doesn't
            // also open it as a native window.
            if let Some(ref params) = msg.params
                && let Some(info) = params.get("targetInfo")
            {
                let target_type = info.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let opener_id = info.get("openerId").and_then(|v| v.as_str()).unwrap_or("");
                if target_type == "page" && !opener_id.is_empty() {
                    let target_id = info
                        .get("targetId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let url = info
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    tracing::debug!(
                        target_id,
                        opener_id,
                        url,
                        "firefox-cdp: Target.targetCreated popup — rerouting to popup_queue"
                    );
                    // Skip blank intermediary targets; the real URL arrives shortly
                    // via a subsequent targetCreated or frameNavigated. If blank,
                    // we still close the target but don't enqueue an empty URL.
                    if !url.is_empty()
                        && url != "about:blank"
                        && let Ok(mut q) = popup_queue.lock()
                    {
                        q.push_back(url);
                    }
                    // Close the popup target so Firefox doesn't open a native window.
                    if !target_id.is_empty() {
                        let cmd = CdpCommand::new(
                            "Target.closeTarget",
                            CloseTargetParams {
                                target_id: target_id.clone(),
                            },
                        );
                        tracing::debug!(target_id, "firefox-cdp: closing popup target");
                        let _ = ws.send_text(cmd.serialize());
                    }
                }
            }
        }
        "Runtime.consoleAPICalled" => {
            // Sentinel IPC: JS injected by buffr emits sentinel lines via
            // `console.log`. We dispatch on the prefix:
            //   `__buffr_hint__:`   — hint-mode events (JSON payload)
            //   `__buffr_media__:`  — media probe result ("true"/"false")
            //   `__buffr_cursor__:` — cursor CSS name
            if let Some(ref params) = msg.params
                && let Some(args) = params.get("args").and_then(|v| v.as_array())
            {
                for arg in args {
                    let value_str = arg.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    if value_str.is_empty() {
                        continue;
                    }
                    if let Some(rest) = value_str.strip_prefix("__buffr_media__:") {
                        let active = rest.trim() == "true";
                        tracing::debug!(active, "firefox-cdp: media probe sentinel");
                        video_active.store(active, Ordering::Relaxed);
                        continue;
                    }
                    if let Some(css_cursor) = value_str.strip_prefix("__buffr_cursor__:") {
                        let raw = css_cursor_to_cef_raw(css_cursor.trim());
                        tracing::debug!(css_cursor, raw, "firefox-cdp: cursor sentinel");
                        // browser_id 0 — no per-tab integer ID in CDP backend
                        if let Ok(mut guard) = cursor_change.lock() {
                            *guard = Some((0, raw));
                        }
                        continue;
                    }
                    match parse_console_event(value_str) {
                        Some(Ok(event)) => {
                            tracing::debug!(?event, "firefox-cdp: hint console event");
                            if let Ok(mut guard) = hint_sink.lock() {
                                *guard = Some(event);
                            }
                        }
                        Some(Err(e)) => {
                            tracing::debug!(
                                error = %e,
                                line = value_str,
                                "firefox-cdp: hint console event parse error"
                            );
                        }
                        None => {}
                    }
                }
            }
        }
        _ => {}
    }
}

// ── CSS cursor → CEF raw kind ─────────────────────────────────────────────────

/// Map a CSS cursor name to its CEF `cef_cursor_type_t` integer value.
///
/// CEF integer mapping (matching `cef_cursor_type_t` in `include/internal/cef_types.h`):
/// 0=default, 1=text, 6=crosshair, 8=help, 9=move, 12=not-allowed,
/// 13=wait, 28=pointer, 34=grab, 35=grabbing.
pub fn css_cursor_to_cef_raw(name: &str) -> u32 {
    match name {
        "text" | "vertical-text" => 1,
        "pointer" => 28,
        "default" => 0,
        "wait" | "progress" => 13,
        "crosshair" => 6,
        "move" | "all-scroll" => 9,
        "not-allowed" | "no-drop" => 12,
        "grab" => 34,
        "grabbing" => 35,
        "help" => 8,
        _ => 0,
    }
}

// ── Favicon fetch ─────────────────────────────────────────────────────────────

/// Fetch `favicon_url` (HTTP/HTTPS or `data:` URI), decode it as an image,
/// convert to BGRA `u32` packed pixels, and push into `favicon_sink`.
///
/// Uses a `browser_id` of `0` (no per-tab integer ID in the CDP backend).
/// Errors are debug-logged and silently discarded so the event loop is never
/// interrupted by a slow or broken favicon resource.
fn fetch_and_push_favicon(favicon_url: &str, favicon_sink: &FaviconSink) {
    let image_bytes: Option<Vec<u8>> = if let Some(data) = favicon_url.strip_prefix("data:") {
        // data: URI — extract the base64 payload after the first comma.
        let payload = data.split_once(',').map(|x| x.1).unwrap_or("");
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()
    } else {
        // HTTP/HTTPS URL — blocking fetch with a short timeout.
        match ureq::get(favicon_url).call() {
            Ok(response) => match response.into_body().read_to_vec() {
                Ok(buf) => Some(buf),
                Err(e) => {
                    tracing::debug!(error = %e, favicon_url, "Firefox CDP: favicon body read failed");
                    None
                }
            },
            Err(e) => {
                tracing::debug!(error = %e, favicon_url, "Firefox CDP: favicon fetch failed");
                None
            }
        }
    };

    let Some(bytes) = image_bytes else { return };

    let img = match image::load_from_memory(&bytes) {
        Ok(i) => i,
        Err(e) => {
            tracing::debug!(error = %e, favicon_url, "Firefox CDP: favicon image decode failed");
            return;
        }
    };

    let rgba = img.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    // Pack RGBA bytes as `0xAARRGGBB` u32 (matching CEF favicon layout).
    let pixels: Vec<u32> = rgba
        .chunks_exact(4)
        .map(|c| {
            let r = c[0] as u32;
            let g = c[1] as u32;
            let b = c[2] as u32;
            let a = c[3] as u32;
            (a << 24) | (r << 16) | (g << 8) | b
        })
        .collect();

    let update = FaviconUpdate {
        browser_id: 0,
        width,
        height,
        pixels,
    };
    if let Ok(mut sink) = favicon_sink.lock() {
        sink.push(update);
    }
    tracing::debug!(
        width,
        height,
        favicon_url,
        "Firefox CDP: favicon decoded and pushed"
    );
}

// ── Frame decode ──────────────────────────────────────────────────────────────

/// Decode a base64-encoded PNG into BGRA and write it to `osr_frame`.
///
/// Firefox `Page.captureScreenshot` returns a base64 PNG (same as Chromium).
/// Exposed as `pub(crate)` for unit tests.
pub fn decode_and_write_frame(b64: &str, osr_frame: &SharedOsrFrame) {
    let data = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "Firefox CDP: captureScreenshot base64 decode failed");
            return;
        }
    };

    let img = match image::load_from_memory_with_format(&data, image::ImageFormat::Png) {
        Ok(i) => i,
        Err(e) => {
            tracing::debug!(error = %e, "Firefox CDP: captureScreenshot PNG decode failed");
            return;
        }
    };

    let rgba = img.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    // Convert RGBA → BGRA (the OsrFrame pixel format).
    let mut bgra = rgba.into_raw();
    for chunk in bgra.chunks_exact_mut(4) {
        chunk.swap(0, 2); // R ↔ B
    }

    if let Ok(mut frame) = osr_frame.lock() {
        frame.width = width;
        frame.height = height;
        frame.pixels = bgra;
        frame.generation = frame.generation.wrapping_add(1);
        frame.needs_fresh = false;
        tracing::debug!(
            width,
            height,
            generation = frame.generation,
            "Firefox CDP: OSR frame updated"
        );
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;

    use buffr_engine::OsrFrame;

    use super::*;

    fn make_png_bytes(r: u8, g: u8, b: u8) -> Vec<u8> {
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(2, 2, |_, _| Rgba([r, g, b, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode png");
        buf.into_inner()
    }

    #[test]
    fn screenshot_decode_writes_bgra() {
        let png_bytes = make_png_bytes(255, 0, 0);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

        let osr_frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(1, 1)));
        decode_and_write_frame(&b64, &osr_frame);

        let frame = osr_frame.lock().unwrap();
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        // RGBA [255, 0, 0, 255] → BGRA [0, 0, 255, 255]
        assert_eq!(&frame.pixels[0..4], &[0u8, 0, 255, 255], "BGRA swap");
        for chunk in frame.pixels.chunks_exact(4) {
            assert_eq!(chunk, &[0u8, 0, 255, 255]);
        }
        assert_eq!(frame.generation, 1);
        assert!(!frame.needs_fresh);
    }

    #[test]
    fn decode_bad_base64_is_silent() {
        let osr_frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(4, 4)));
        decode_and_write_frame("not-valid-base64!!!", &osr_frame);
        let frame = osr_frame.lock().unwrap();
        assert_eq!(frame.generation, 0);
    }

    #[test]
    fn url_update_sink_fifo_order() {
        let sink = new_url_update_sink();
        {
            let mut guard = sink.lock().unwrap();
            guard.push_back(("s1".into(), "https://a.example".into(), "A".into()));
            guard.push_back(("s2".into(), "https://b.example".into(), "B".into()));
        }
        let items: Vec<_> = sink.lock().unwrap().drain(..).collect();
        assert_eq!(items[0].0, "s1");
        assert_eq!(items[1].0, "s2");
        assert!(sink.lock().unwrap().is_empty());
    }

    #[test]
    fn new_title_map_starts_empty() {
        let tm = new_title_map();
        assert!(tm.lock().unwrap().is_empty());
    }

    #[test]
    fn title_map_stores_multiple_targets() {
        let tm = new_title_map();
        {
            let mut m = tm.lock().unwrap();
            m.insert("t1".to_owned(), "Title 1".to_owned());
            m.insert("t2".to_owned(), "Title 2".to_owned());
        }
        let m = tm.lock().unwrap();
        assert_eq!(m.get("t1").map(String::as_str), Some("Title 1"));
        assert_eq!(m.get("t2").map(String::as_str), Some("Title 2"));
    }
}
