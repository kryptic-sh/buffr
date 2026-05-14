//! CDP worker thread.
//!
//! A single `std::thread` owns the WebSocket connection and runs a loop:
//!   1. Send pending commands from the `cmd_rx` channel.
//!   2. Read a CDP message from the WebSocket.
//!   3. If it's a response (has an `id`), route it to the waiting `Sender`.
//!   4. If it's an event, handle known events:
//!      - `Page.screencastFrame`: decode base64 PNG → BGRA, write to
//!        `SharedOsrFrame`, send `Page.screencastFrameAck`.
//!      - `Page.frameNavigated`: re-apply per-session zoom.
//!
//! The polling `Page.captureScreenshot` loop is gone.  Chromium pushes frames
//! via `Page.startScreencast` instead, throttled naturally by the ack protocol.
//!
//! The worker exits when the `cmd_rx` channel is dropped (engine shutdown).

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as B64Engine;
use serde_json::Value;

use buffr_engine::{PermissionsQueue, SharedOsrFrame, SharedOsrViewState};
use buffr_permissions::Capability;

use crate::cdp::{
    CdpCommand, CdpMessage, DispatchKeyEventParams, DispatchMouseEventParams, NavigateParams,
    ScreencastFrameAckParams, SetDeviceMetricsParams, StartScreencastParams, next_id,
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
    /// Resize the viewport for a tab and restart screencast with new dimensions.
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
    /// Switch the "active" screencast session.
    ///
    /// Sends `Page.stopScreencast` on the previous session (if any) and
    /// `Page.startScreencast` on the new one.  `None` stops all screencasting.
    SetActiveSession {
        session_id: Option<String>,
        /// Viewport dimensions at the time of the switch (for startScreencast).
        width: u32,
        height: u32,
    },
    /// Shutdown the worker cleanly.
    Shutdown,
}

// ── Worker ────────────────────────────────────────────────────────────────────

/// Timeout waiting for a CDP response (per command).
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the CDP worker event loop (blocking; call from a dedicated thread).
pub fn run(
    mut ws: WsClient,
    cmd_rx: Receiver<Command>,
    osr_frame: SharedOsrFrame,
    osr_view: SharedOsrViewState,
    permissions_queue: PermissionsQueue,
    perm_session_map: Arc<Mutex<HashMap<String, String>>>,
) {
    // Map from CDP message-id → reply sender.
    let mut pending: HashMap<u64, Sender<Result<Value, BlinkError>>> = HashMap::new();
    // Current active screencast session.
    let mut active_session: Option<String> = None;
    // Per-session zoom levels (re-applied after each navigation).
    let mut session_zoom: HashMap<String, f64> = HashMap::new();

    tracing::debug!("CDP worker started");

    loop {
        // ── Drain commands ────────────────────────────────────────────────────
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Shutdown) => {
                    tracing::debug!("CDP worker: shutdown command received");
                    return;
                }
                Ok(Command::SetActiveSession {
                    session_id,
                    width,
                    height,
                }) => {
                    // Stop the old screencast.
                    if let Some(ref old) = active_session {
                        send_stop_screencast(&mut ws, old);
                    }
                    active_session = session_id;
                    // Start the new one.
                    if let Some(ref new_sess) = active_session {
                        send_start_screencast(&mut ws, new_sess, width.max(1), height.max(1));
                    }
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
                    let (tx, rx) = mpsc::channel();
                    pending.insert(id, tx);
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
                    // Update device metrics.
                    let cmd = CdpCommand::new(
                        "Page.setDeviceMetricsOverride",
                        SetDeviceMetricsParams {
                            width,
                            height,
                            device_scale_factor: 1.0,
                            mobile: false,
                        },
                    )
                    .with_session(session_id.clone());
                    let _ = ws.send_text(cmd.serialize());

                    // Restart screencast at the new dimensions if this is the
                    // active session.
                    if active_session.as_deref() == Some(&session_id) {
                        send_stop_screencast(&mut ws, &session_id);
                        send_start_screencast(&mut ws, &session_id, width.max(1), height.max(1));
                    }
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
                    let expr = format!("document.body.style.zoom = '{level}'");
                    let cmd = CdpCommand::new(
                        "Runtime.evaluate",
                        serde_json::json!({ "expression": expr }),
                    )
                    .with_session(session_id.clone());
                    let _ = ws.send_text(cmd.serialize());
                    if (level - 1.0_f64).abs() < f64::EPSILON {
                        session_zoom.remove(&session_id);
                    } else {
                        session_zoom.insert(session_id, level);
                    }
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

        // ── Read one WS message (non-blocking) ────────────────────────────────
        match ws.try_recv_text() {
            Ok(None) => {
                // No message ready — yield briefly to avoid a hot spin.
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "CDP worker: WS read error — exiting");
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(BlinkError::WsIo(e.to_string())));
                }
                return;
            }
            Ok(Some(text)) => match serde_json::from_str::<CdpMessage>(&text) {
                Err(e) => {
                    tracing::debug!(error = %e, raw = %text, "CDP worker: unparse-able message");
                }
                Ok(msg) => {
                    dispatch_message(
                        msg,
                        &mut pending,
                        &session_zoom,
                        &mut ws,
                        &osr_frame,
                        &osr_view,
                        &permissions_queue,
                        &perm_session_map,
                    );
                }
            },
        }
    }
}

// ── Screencast helpers ────────────────────────────────────────────────────────

fn send_start_screencast(ws: &mut WsClient, session_id: &str, width: u32, height: u32) {
    let cmd = CdpCommand::new(
        "Page.startScreencast",
        StartScreencastParams {
            format: "png",
            quality: 100,
            max_width: width,
            max_height: height,
            every_nth_frame: 1,
        },
    )
    .with_session(session_id.to_owned());
    tracing::debug!(session_id, width, height, "CDP: startScreencast");
    let _ = ws.send_text(cmd.serialize());
}

fn send_stop_screencast(ws: &mut WsClient, session_id: &str) {
    let cmd = CdpCommand::new_bare("Page.stopScreencast").with_session(session_id.to_owned());
    tracing::debug!(session_id, "CDP: stopScreencast");
    let _ = ws.send_text(cmd.serialize());
}

// ── Message dispatch ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn dispatch_message(
    msg: CdpMessage,
    pending: &mut HashMap<u64, Sender<Result<Value, BlinkError>>>,
    session_zoom: &HashMap<String, f64>,
    ws: &mut WsClient,
    osr_frame: &SharedOsrFrame,
    _osr_view: &SharedOsrViewState,
    permissions_queue: &PermissionsQueue,
    perm_session_map: &Arc<Mutex<HashMap<String, String>>>,
) {
    // Command response.
    if let Some(id) = msg.id {
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
    let Some(ref method) = msg.method else {
        return;
    };
    tracing::debug!(method, "CDP event");

    match method.as_str() {
        "Page.screencastFrame" => {
            handle_screencast_frame(&msg, ws, osr_frame);
        }
        "Page.frameNavigated" => {
            // Re-apply zoom after each navigation.
            if let Some(ref session_id) = msg.session_id
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
        "Runtime.bindingCalled" => {
            // Phase 8a (#88): handle permission binding calls from the JS shim.
            if let Some(ref params) = msg.params {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name == "__buffrPermissionRequest" {
                    let payload = params.get("payload").and_then(|v| v.as_str()).unwrap_or("");
                    handle_permission_binding(
                        payload,
                        msg.session_id.as_deref(),
                        permissions_queue,
                        perm_session_map,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Handle a `Runtime.bindingCalled` event for `__buffrPermissionRequest`.
///
/// Parses the JSON payload from the JS shim, maps the capability string to a
/// [`Capability`], pushes a neutral [`buffr_engine::permissions::PendingPermission`]
/// onto the queue, and records the `resolve_id → session_id` mapping so
/// `resolve_permission` can target the right tab.
fn handle_permission_binding(
    payload: &str,
    session_id: Option<&str>,
    permissions_queue: &PermissionsQueue,
    perm_session_map: &Arc<Mutex<HashMap<String, String>>>,
) {
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, payload, "blink-cdp: invalid permission binding payload");
            return;
        }
    };

    let id = match v.get("id").and_then(|v| v.as_str()) {
        Some(s) => s.to_owned(),
        None => {
            tracing::debug!("blink-cdp: permission binding payload missing 'id'");
            return;
        }
    };
    let cap_str = v.get("capability").and_then(|v| v.as_str()).unwrap_or("");
    let origin = v
        .get("origin")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    let cap: Capability =
        crate::permissions::capability_from_str(cap_str).unwrap_or(Capability::Other(0));

    tracing::debug!(
        id,
        cap_str,
        origin,
        "blink-cdp: permission request from JS shim"
    );

    // Record resolve_id → session_id.
    if let Some(sess) = session_id
        && let Ok(mut map) = perm_session_map.lock()
    {
        map.insert(id.clone(), sess.to_owned());
    }

    // Push neutral entry to the queue.
    let perm = buffr_engine::permissions::PendingPermission {
        origin,
        capabilities: vec![cap],
        resolve_id: Some(id),
    };
    if let Ok(mut q) = permissions_queue.lock() {
        q.push_back(perm);
    }
}

fn handle_screencast_frame(msg: &CdpMessage, ws: &mut WsClient, osr_frame: &SharedOsrFrame) {
    let params = match &msg.params {
        Some(p) => p,
        None => {
            tracing::debug!("screencastFrame: missing params");
            return;
        }
    };

    let data_str = match params.get("data").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            tracing::debug!("screencastFrame: missing data field");
            return;
        }
    };

    // The `sessionId` in the screencast frame params is a CDP screencast
    // sequence number (i64), not the session string.
    let screencast_session_id = params
        .get("sessionId")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // Decode and write frame.
    decode_and_write_frame(data_str, osr_frame);

    // Must ack every frame or Chromium stalls the screencast.
    let ack = CdpCommand::new(
        "Page.screencastFrameAck",
        ScreencastFrameAckParams {
            session_id: screencast_session_id,
        },
    );
    // Ack is session-scoped: use the session_id from the enclosing CdpMessage.
    let ack = match &msg.session_id {
        Some(s) => ack.with_session(s.clone()),
        None => ack,
    };
    tracing::debug!(screencast_session_id, "CDP: screencastFrameAck");
    let _ = ws.send_text(ack.serialize());
}

// ── Frame decode ──────────────────────────────────────────────────────────────

/// Decode a base64-encoded PNG into BGRA and write it to `osr_frame`.
///
/// Exposed as `pub(crate)` for unit tests.
pub(crate) fn decode_and_write_frame(b64: &str, osr_frame: &SharedOsrFrame) {
    let data = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "screencastFrame base64 decode failed");
            return;
        }
    };

    let img = match image::load_from_memory_with_format(&data, image::ImageFormat::Png) {
        Ok(i) => i,
        Err(e) => {
            tracing::debug!(error = %e, "screencastFrame PNG decode failed");
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
            "OSR frame updated"
        );
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;

    use buffr_engine::OsrFrame;

    use super::*;

    // Build a 2×2 solid-colour PNG in memory and return it.
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
    fn screencast_frame_decode_writes_bgra() {
        // Build a 2×2 red PNG (RGBA = [255, 0, 0, 255]).
        let png_bytes = make_png_bytes(255, 0, 0);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

        let osr_frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(1, 1)));
        decode_and_write_frame(&b64, &osr_frame);

        let frame = osr_frame.lock().unwrap();
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        // RGBA [255, 0, 0, 255] → BGRA [0, 0, 255, 255]
        assert_eq!(&frame.pixels[0..4], &[0u8, 0, 255, 255], "BGRA swap");
        // All 4 pixels identical.
        for chunk in frame.pixels.chunks_exact(4) {
            assert_eq!(chunk, &[0u8, 0, 255, 255]);
        }
        assert_eq!(frame.generation, 1);
        assert!(!frame.needs_fresh);
    }

    #[test]
    fn screencast_ack_message_shape() {
        // Verify the JSON shape of a screencastFrameAck message.
        let ack = CdpCommand::new(
            "Page.screencastFrameAck",
            ScreencastFrameAckParams { session_id: 42 },
        )
        .with_session("sess-abc".to_owned());

        let json = ack.serialize();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(v["method"], "Page.screencastFrameAck");
        assert_eq!(v["params"]["sessionId"], 42);
        assert_eq!(v["sessionId"], "sess-abc");
        assert!(v["id"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn decode_bad_base64_is_silent() {
        let osr_frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(4, 4)));
        decode_and_write_frame("not-valid-base64!!!", &osr_frame);
        // Frame should be untouched.
        let frame = osr_frame.lock().unwrap();
        assert_eq!(frame.generation, 0);
    }
}
