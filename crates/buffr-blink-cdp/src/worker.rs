//! CDP worker thread.
//!
//! A single `std::thread` owns the WebSocket connection and runs a loop:
//!   1. Send pending commands from the `cmd_rx` channel.
//!   2. Read a CDP message from the WebSocket.
//!   3. If it's a response (has an `id`), route it to the waiting `Sender`.
//!   4. If it's an event, handle known events (e.g. `Page.frameNavigated`).
//!   5. On a configurable tick, issue `Page.captureScreenshot` for the active
//!      page and decode the result into the shared `OsrFrame`.
//!
//! The worker exits when the `cmd_rx` channel is dropped (engine shutdown).

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use base64::Engine as B64Engine;
use serde_json::Value;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState};

use crate::cdp::{
    CaptureScreenshotParams, CdpCommand, CdpMessage, DispatchKeyEventParams,
    DispatchMouseEventParams, NavigateParams, SetDeviceMetricsParams, next_id,
};
use crate::error::BlinkError;
use crate::ws::WsClient;

// ── Commands sent to the worker ───────────────────────────────────────────────

/// Commands the engine sends to the worker thread.
pub enum Command {
    /// Issue a CDP command on the browser-level connection and return the response.
    BrowserCmd {
        cmd: CdpCommand,
        reply: Sender<Result<Value, BlinkError>>,
    },
    /// Issue a CDP command on a specific session and return the response.
    SessionCmd {
        session_id: String,
        cmd: CdpCommand,
        reply: Sender<Result<Value, BlinkError>>,
    },
    /// Navigate the active tab to `url`.
    Navigate {
        session_id: String,
        url: String,
        reply: Sender<Result<(), BlinkError>>,
    },
    /// Resize the viewport for the active tab.
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
    /// Apply a CSS zoom level to the given tab session via `Runtime.evaluate`.
    ///
    /// `level` is the linear scale factor (e.g. `1.25` = 125 %).
    SetZoom { session_id: String, level: f64 },
    /// Update which session is "active" for OSR screenshot polling.
    SetActiveSession { session_id: Option<String> },
    /// Shutdown the worker cleanly.
    Shutdown,
}

// ── Worker ────────────────────────────────────────────────────────────────────

/// OSR screenshot poll interval.
const SCREENSHOT_INTERVAL: Duration = Duration::from_millis(200); // ~5 FPS

/// Timeout waiting for a CDP response (per command).
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the CDP worker event loop (blocking; call from a dedicated thread).
pub fn run(
    mut ws: WsClient,
    cmd_rx: Receiver<Command>,
    osr_frame: SharedOsrFrame,
    osr_view: SharedOsrViewState,
) {
    // Map from CDP message-id → reply sender.
    let mut pending: HashMap<u64, Sender<Result<Value, BlinkError>>> = HashMap::new();
    // Current active session for OSR polling.
    let mut active_session: Option<String> = None;
    // Per-session zoom levels (re-applied after each navigation).
    let mut session_zoom: HashMap<String, f64> = HashMap::new();
    // Next screenshot capture time.
    let mut next_screenshot = Instant::now() + SCREENSHOT_INTERVAL;
    // A one-shot pending screenshot id so we can route the response.
    let mut screenshot_pending_id: Option<u64> = None;

    tracing::debug!("CDP worker started");

    loop {
        // ── Drain commands ────────────────────────────────────────────────────
        // Use a tight non-blocking drain so we batch multiple commands before
        // going back to read the WS.
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Shutdown) => {
                    tracing::debug!("CDP worker: shutdown command received");
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
                    // Wrap reply to convert Value → ()
                    let (tx, rx) = mpsc::channel();
                    pending.insert(id, tx);
                    // Spin-wait in a side thread to avoid blocking the loop.
                    std::thread::spawn(move || {
                        let res = match rx.recv_timeout(CMD_TIMEOUT) {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(BlinkError::Timeout {
                                method: "Page.navigate",
                            }),
                        };
                        let _ = reply.send(res);
                    });
                }
                Ok(Command::Resize {
                    session_id,
                    width,
                    height,
                }) => {
                    let cmd = CdpCommand::new(
                        "Page.setDeviceMetricsOverride",
                        SetDeviceMetricsParams {
                            width,
                            height,
                            device_scale_factor: 1.0,
                            mobile: false,
                        },
                    )
                    .with_session(session_id);
                    let _ = ws.send_text(cmd.serialize());
                    // No reply expected; fire-and-forget.
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
                Ok(Command::SetZoom { session_id, level }) => {
                    // Inject CSS zoom via Runtime.evaluate — simplest cross-page approach.
                    let expr = format!("document.body.style.zoom = '{level}'");
                    let cmd = CdpCommand::new(
                        "Runtime.evaluate",
                        serde_json::json!({ "expression": expr }),
                    )
                    .with_session(session_id.clone());
                    let _ = ws.send_text(cmd.serialize());
                    // Track so we can re-apply after Page.frameNavigated.
                    if (level - 1.0_f64).abs() < f64::EPSILON {
                        session_zoom.remove(&session_id);
                    } else {
                        session_zoom.insert(session_id, level);
                    }
                    // Fire-and-forget; no reply expected.
                }
                Ok(Command::BrowserCmd { cmd, reply }) => {
                    let id = cmd.id;
                    if let Err(e) = ws.send_text(cmd.serialize()) {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    pending.insert(id, reply);
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
                    pending.insert(id, reply);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::debug!("CDP worker: command channel closed — exiting");
                    return;
                }
            }
        }

        // ── OSR screenshot poll ───────────────────────────────────────────────
        let now = Instant::now();
        if screenshot_pending_id.is_none() && now >= next_screenshot && active_session.is_some() {
            if let Some(ref sess) = active_session {
                let cmd = CdpCommand::new(
                    "Page.captureScreenshot",
                    CaptureScreenshotParams {
                        format: "png",
                        quality: 80,
                    },
                )
                .with_session(sess.clone());
                let id = cmd.id;
                if ws.send_text(cmd.serialize()).is_ok() {
                    screenshot_pending_id = Some(id);
                }
            }
            next_screenshot = now + SCREENSHOT_INTERVAL;
        }

        // ── Read one WS message (non-blocking) ────────────────────────────────
        match ws.try_recv_text() {
            Ok(None) => {
                // No message ready — yield briefly to avoid a hot spin.
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "CDP worker: WS read error — exiting");
                // Fail all pending with the error string.
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(BlinkError::WsIo(e.to_string())));
                }
                return;
            }
            Ok(Some(text)) => {
                // Parse and dispatch.
                match serde_json::from_str::<CdpMessage>(&text) {
                    Err(e) => {
                        tracing::debug!(error = %e, raw = %text, "CDP worker: unparse-able message");
                    }
                    Ok(msg) => {
                        dispatch_message(
                            msg,
                            &mut pending,
                            &mut screenshot_pending_id,
                            &session_zoom,
                            &mut ws,
                            &osr_frame,
                            &osr_view,
                        );
                    }
                }
            }
        }
    }
}

fn dispatch_message(
    msg: CdpMessage,
    pending: &mut HashMap<u64, Sender<Result<Value, BlinkError>>>,
    screenshot_pending_id: &mut Option<u64>,
    session_zoom: &HashMap<String, f64>,
    ws: &mut WsClient,
    osr_frame: &SharedOsrFrame,
    _osr_view: &SharedOsrViewState,
) {
    // Command response.
    if let Some(id) = msg.id {
        // Check if this is the screenshot response first.
        if *screenshot_pending_id == Some(id) {
            *screenshot_pending_id = None;
            if let Some(result) = &msg.result
                && let Some(data_str) = result.get("data").and_then(|v| v.as_str())
            {
                decode_screenshot(data_str, osr_frame);
            }
            return;
        }

        if let Some(tx) = pending.remove(&id) {
            let result = if let Some(err) = msg.error {
                Err(BlinkError::Protocol(format!(
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
    if let Some(ref method) = msg.method {
        tracing::debug!(method, "CDP event");

        // Re-apply zoom after each navigation: a new page resets
        // `document.body.style.zoom` to its default, so we re-inject.
        if method == "Page.frameNavigated"
            && let Some(ref session_id) = msg.session_id
            && let Some(&level) = session_zoom.get(session_id)
        {
            let expr = format!("document.body.style.zoom = '{level}'");
            let cmd = CdpCommand::new(
                "Runtime.evaluate",
                serde_json::json!({ "expression": expr }),
            )
            .with_session(session_id.clone());
            let _ = ws.send_text(cmd.serialize());
        }
    }
}

fn decode_screenshot(b64: &str, osr_frame: &SharedOsrFrame) {
    let data = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "screenshot base64 decode failed");
            return;
        }
    };

    let img = match image::load_from_memory_with_format(&data, image::ImageFormat::Png) {
        Ok(i) => i,
        Err(e) => {
            tracing::debug!(error = %e, "screenshot PNG decode failed");
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
    }
}

// ── Helper: synchronous command from the engine side ─────────────────────────

/// Send a command to the worker and wait for the response.
///
/// Called from the engine's trait methods (which are sync).
pub fn send_command_blocking(
    cmd_tx: &std::sync::mpsc::SyncSender<Command>,
    session_id: Option<String>,
    method: &'static str,
    params: impl serde::Serialize,
) -> Result<Value, BlinkError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    let cdp_cmd = CdpCommand {
        id: next_id(),
        method,
        params: Some(serde_json::to_value(params).unwrap_or(Value::Null)),
        session_id,
    };
    let cmd = Command::BrowserCmd {
        cmd: cdp_cmd,
        reply: reply_tx,
    };
    cmd_tx.try_send(cmd).map_err(|_| BlinkError::WorkerDead)?;
    reply_rx
        .recv_timeout(CMD_TIMEOUT)
        .map_err(|_| BlinkError::Timeout { method })
        .and_then(|r| r)
}
