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
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as B64Engine;
use serde_json::Value;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

use crate::cdp::{
    CaptureScreenshotParams, CdpCommand, CdpMessage, DispatchKeyEventParams,
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
    _ws: &mut WsClient,
    osr_frame: &SharedOsrFrame,
    url_update_sink: &UrlUpdateSink,
    loading_state: &Arc<Mutex<HashMap<String, bool>>>,
    nav_count: &Arc<Mutex<HashMap<String, usize>>>,
    title_map: &TitleMap,
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
        _ => {}
    }
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
