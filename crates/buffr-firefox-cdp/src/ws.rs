//! Synchronous WebSocket wrapper over `tungstenite`.
//!
//! CDP connects to `ws://127.0.0.1:…` (localhost, no TLS). The stream is
//! always `Plain(TcpStream)` at runtime — we carry the `MaybeTlsStream` enum
//! wrapper in the type signature because tungstenite requires it.

use std::net::TcpStream;
use tungstenite::{Message, WebSocket, client::connect as ws_connect, stream::MaybeTlsStream};

use crate::error::FirefoxError;

/// Short read timeout applied during non-blocking polls.
///
/// 5 ms gives enough time to drain an in-flight frame without busy-spinning.
pub const WS_POLL_INTERVAL_MS: u64 = 5;

/// Concrete socket type returned by tungstenite's connect helper.
type Sock = WebSocket<MaybeTlsStream<TcpStream>>;

/// A connected synchronous WebSocket client.
pub struct WsClient {
    socket: Sock,
}

impl WsClient {
    /// Connect to a WebSocket URL (e.g. `ws://127.0.0.1:9222/devtools/...`).
    pub fn connect(url: &str) -> Result<Self, FirefoxError> {
        tracing::debug!(url, "connecting Firefox CDP WebSocket");
        let (socket, _response) =
            ws_connect(url).map_err(|e| FirefoxError::WsConnect(e.to_string()))?;
        Ok(Self { socket })
    }

    /// Send a text frame.
    pub fn send_text(&mut self, text: String) -> Result<(), FirefoxError> {
        self.socket
            .send(Message::Text(text))
            .map_err(|e| FirefoxError::WsIo(e.to_string()))
    }

    /// Read the next message, blocking.
    ///
    /// Skips Ping/Pong/Close control frames and loops until a Text or Binary
    /// frame arrives (or an error occurs).
    pub fn recv_text(&mut self) -> Result<String, FirefoxError> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(t)) => return Ok(t.to_string()),
                Ok(Message::Binary(b)) => {
                    return String::from_utf8(b)
                        .map_err(|e| FirefoxError::WsIo(format!("binary frame utf8: {e}")));
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => {
                    return Err(FirefoxError::WsIo("connection closed by server".into()));
                }
                Ok(Message::Frame(_)) => continue,
                Err(e) => return Err(FirefoxError::WsIo(e.to_string())),
            }
        }
    }

    /// Try to read with a short read timeout (returns `None` on timeout).
    ///
    /// Uses a 5 ms TCP read timeout to make the worker loop non-blocking
    /// without busy-spinning.
    pub fn try_recv_text(&mut self) -> Result<Option<String>, FirefoxError> {
        use std::time::Duration;
        use tungstenite::stream::MaybeTlsStream;

        let timeout = Some(Duration::from_millis(WS_POLL_INTERVAL_MS));
        let set_ok = match self.socket.get_mut() {
            MaybeTlsStream::Plain(tcp) => tcp.set_read_timeout(timeout).is_ok(),
            _ => {
                // Non-Plain stream: cannot set a timeout — bail immediately.
                return Ok(None);
            }
        };

        let result = match self.socket.read() {
            Ok(Message::Text(t)) => Ok(Some(t.to_string())),
            Ok(Message::Binary(b)) => String::from_utf8(b)
                .map(Some)
                .map_err(|e| FirefoxError::WsIo(format!("binary frame utf8: {e}"))),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => Ok(None),
            Ok(Message::Close(_)) => Err(FirefoxError::WsIo("connection closed by server".into())),
            Err(tungstenite::Error::Io(ref io_err))
                if io_err.kind() == std::io::ErrorKind::WouldBlock
                    || io_err.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => Err(FirefoxError::WsIo(e.to_string())),
        };

        // Restore blocking (no timeout).
        if set_ok && let MaybeTlsStream::Plain(tcp) = self.socket.get_mut() {
            let _ = tcp.set_read_timeout(None);
        }

        result
    }
}
