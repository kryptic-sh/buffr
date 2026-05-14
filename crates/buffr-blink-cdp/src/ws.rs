//! Synchronous WebSocket wrapper over `tungstenite`.
//!
//! `tungstenite::client::connect` returns a `MaybeTlsStream<TcpStream>`.
//! We only connect to `ws://127.0.0.1:…` (localhost, no TLS), so the stream
//! is always `Plain(TcpStream)` at runtime — but we have to carry the enum
//! wrapper in the type signature.

use std::net::TcpStream;
use tungstenite::{Message, WebSocket, client::connect as ws_connect, stream::MaybeTlsStream};

use crate::error::BlinkError;

/// Concrete socket type returned by tungstenite's connect helper.
type Sock = WebSocket<MaybeTlsStream<TcpStream>>;

/// A connected synchronous WebSocket client.
pub struct WsClient {
    socket: Sock,
}

impl WsClient {
    /// Connect to a WebSocket URL (e.g. `ws://127.0.0.1:9222/devtools/...`).
    pub fn connect(url: &str) -> Result<Self, BlinkError> {
        tracing::debug!(url, "connecting CDP WebSocket");
        let (socket, _response) =
            ws_connect(url).map_err(|e| BlinkError::WsConnect(e.to_string()))?;
        Ok(Self { socket })
    }

    /// Send a text frame.
    pub fn send_text(&mut self, text: String) -> Result<(), BlinkError> {
        self.socket
            .send(Message::Text(text))
            .map_err(|e| BlinkError::WsIo(e.to_string()))
    }

    /// Read the next message, blocking.
    ///
    /// Skips Ping/Pong/Close control frames and loops until a Text or Binary
    /// frame arrives (or an error occurs).
    pub fn recv_text(&mut self) -> Result<String, BlinkError> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(t)) => return Ok(t.to_string()),
                Ok(Message::Binary(b)) => {
                    return String::from_utf8(b)
                        .map_err(|e| BlinkError::WsIo(format!("binary frame utf8: {e}")));
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => {
                    return Err(BlinkError::WsIo("connection closed by server".into()));
                }
                Ok(Message::Frame(_)) => continue,
                Err(e) => return Err(BlinkError::WsIo(e.to_string())),
            }
        }
    }

    /// Try to read with a short read timeout (returns `None` on timeout).
    ///
    /// CDP connects over localhost (`ws://127.0.0.1:…`) so the stream is
    /// always `MaybeTlsStream::Plain(TcpStream)`.  We set a 5 ms read timeout
    /// to make the worker loop non-blocking without busy-spinning.
    pub fn try_recv_text(&mut self) -> Result<Option<String>, BlinkError> {
        use std::time::Duration;
        use tungstenite::stream::MaybeTlsStream;

        let timeout = Some(Duration::from_millis(5));
        // Extract the TcpStream regardless of the MaybeTlsStream variant.
        // On localhost CDP is always Plain, but we handle the wildcard arm
        // to stay forward-compatible.
        let set_ok = match self.socket.get_mut() {
            MaybeTlsStream::Plain(tcp) => tcp.set_read_timeout(timeout).is_ok(),
            _ => false, // TLS on localhost is unusual; fall through without timeout
        };

        let result = match self.socket.read() {
            Ok(Message::Text(t)) => Ok(Some(t.to_string())),
            Ok(Message::Binary(b)) => String::from_utf8(b)
                .map(Some)
                .map_err(|e| BlinkError::WsIo(format!("binary frame utf8: {e}"))),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => Ok(None),
            Ok(Message::Close(_)) => Err(BlinkError::WsIo("connection closed by server".into())),
            Err(tungstenite::Error::Io(ref io_err))
                if io_err.kind() == std::io::ErrorKind::WouldBlock
                    || io_err.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => Err(BlinkError::WsIo(e.to_string())),
        };

        // Restore blocking (no timeout).
        if set_ok && let MaybeTlsStream::Plain(tcp) = self.socket.get_mut() {
            let _ = tcp.set_read_timeout(None);
        }

        result
    }
}
