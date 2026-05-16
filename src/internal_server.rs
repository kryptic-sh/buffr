//! Tiny loopback HTTP server for buffr's internal `buffr://*` pages.
//!
//! Each running buffr instance starts an [`InternalServer`] bound to
//! `127.0.0.1:0` (kernel-assigned ephemeral port, loopback only — never
//! routable). Engines navigate to
//! `http://127.0.0.1:<port>/<token>/<path>` and the server dispatches the
//! request to a registered handler.
//!
//! The `<token>` is a 32-character hex string generated at startup. Any
//! request whose path doesn't begin with this token returns `403 Forbidden`.
//! The token gives a coarse "is this the same buffr instance?" guard
//! against arbitrary localhost processes scraping internal pages: it's
//! reset every launch, never written to disk, and only known to engines
//! that buffr explicitly handed the URL to. It is *not* a hard security
//! barrier — a local attacker with `/proc/<pid>/net/tcp` access can
//! enumerate the port and a peer reading `/proc/<pid>/cmdline` could
//! exfiltrate environment-set tokens.  Defence-in-depth, not authentication.
//!
//! ## Routing
//!
//! Routes are registered as `(method, path)` tuples mapping to a
//! [`Handler`] closure that returns the response body bytes. Currently
//! only `GET` is supported.  Unknown routes return `404`.
//!
//! ## Lifecycle
//!
//! [`InternalServer::start`] spawns a background thread running an
//! `accept` loop. [`InternalServer`]'s `Drop` impl signals shutdown via
//! an atomic flag, connects to the listener once to break the blocking
//! `accept`, and joins the thread.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Body provider for a single route. Invoked on every matching request so
/// the response reflects current host state (keybinds, palette, …).
pub type Handler = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Route table keyed by URL path *after* the auth token has been stripped.
/// Paths must start with `/`. Missing routes return 404.
#[derive(Default, Clone)]
pub struct Routes {
    inner: HashMap<String, RouteEntry>,
}

#[derive(Clone)]
struct RouteEntry {
    handler: Handler,
    content_type: String,
}

impl Routes {
    /// Construct an empty route table.
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Register a route for `GET <path>` returning `text/html; charset=utf-8`.
    pub fn html(mut self, path: impl Into<String>, handler: Handler) -> Self {
        self.inner.insert(
            normalize_path(path),
            RouteEntry {
                handler,
                content_type: "text/html; charset=utf-8".to_string(),
            },
        );
        self
    }

    /// Register a route with a custom Content-Type header value.
    pub fn raw(
        mut self,
        path: impl Into<String>,
        content_type: impl Into<String>,
        handler: Handler,
    ) -> Self {
        self.inner.insert(
            normalize_path(path),
            RouteEntry {
                handler,
                content_type: content_type.into(),
            },
        );
        self
    }

    fn lookup(&self, path: &str) -> Option<&RouteEntry> {
        self.inner.get(&normalize_path(path))
    }
}

fn normalize_path(p: impl Into<String>) -> String {
    let p = p.into();
    if p.starts_with('/') {
        p
    } else {
        format!("/{p}")
    }
}

/// Running loopback HTTP server.
///
/// Listens on `127.0.0.1:<kernel-assigned>` until dropped. Use
/// [`Self::url_for`] to build authenticated URLs that engines can
/// navigate to.
pub struct InternalServer {
    addr: SocketAddr,
    token: String,
    shutdown: Arc<AtomicBool>,
    routes: Arc<Mutex<Routes>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InternalServer {
    /// Start a server bound to `127.0.0.1` on an ephemeral port. The thread
    /// is spawned eagerly and `accept`s in the background.
    pub fn start(routes: Routes) -> io::Result<Self> {
        Self::start_at(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), routes)
    }

    /// Like [`Self::start`] but lets the caller pin the bind address.
    /// Exposed for tests; production code should always use `127.0.0.1:0`.
    pub fn start_at(addr: SocketAddr, routes: Routes) -> io::Result<Self> {
        // Hard guard: never let a caller bind to anything but loopback.
        // External services on the same machine must NOT see buffr's
        // internal pages even if they can reach our listener socket.
        if !addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "InternalServer refuses to bind to a non-loopback address",
            ));
        }

        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(false)?;
        let actual_addr = listener.local_addr()?;

        let token = generate_token();
        let shutdown = Arc::new(AtomicBool::new(false));
        let routes_mu = Arc::new(Mutex::new(routes));

        let thread = {
            let shutdown = Arc::clone(&shutdown);
            let routes = Arc::clone(&routes_mu);
            let token = token.clone();
            thread::Builder::new()
                .name("buffr-internal-server".into())
                .spawn(move || accept_loop(listener, shutdown, routes, token))?
        };

        Ok(Self {
            addr: actual_addr,
            token,
            shutdown,
            routes: routes_mu,
            thread: Some(thread),
        })
    }

    /// `127.0.0.1:<port>` the server is bound to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Auth token; required as the first path segment of every request.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Build a fully-qualified, authenticated URL for `path` (e.g. `/new`).
    /// Engines can navigate to the returned string directly.
    pub fn url_for(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("http://{}/{}/{}", self.addr, self.token, path)
    }

    /// Replace the routing table. Useful for hot-reloading internal pages
    /// without restarting the server (tests use this to swap handlers).
    pub fn set_routes(&self, routes: Routes) {
        if let Ok(mut guard) = self.routes.lock() {
            *guard = routes;
        }
    }
}

impl Drop for InternalServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the accept loop by connecting to ourselves once. We
        // don't care if this fails; the listener may already be dead.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(100));
        if let Some(handle) = self.thread.take() {
            // Best-effort join; if the thread panicked the server is going
            // away anyway.
            let _ = handle.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    routes: Arc<Mutex<Routes>>,
    token: String,
) {
    // 50 ms poll cadence: keeps Drop responsive without burning a core.
    listener
        .set_nonblocking(false)
        .expect("TcpListener::set_nonblocking should not fail");

    while !shutdown.load(Ordering::SeqCst) {
        // Apply a short read timeout so accept doesn't block forever; the
        // shutdown path also self-connects to wake us, this is the belt &
        // braces.
        match listener.accept() {
            Ok((stream, _peer)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let routes_snapshot = {
                    match routes.lock() {
                        Ok(g) => g.clone(),
                        Err(p) => p.into_inner().clone(),
                    }
                };
                let token = token.clone();
                // Per-connection thread so a slow client can't block the
                // accept loop. Internal pages are small; we don't need
                // keep-alive or HTTP/2.
                let _ = thread::Builder::new()
                    .name("buffr-internal-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle_connection(stream, &routes_snapshot, &token) {
                            tracing::debug!(error = %e, "internal_server: connection error");
                        }
                    });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                tracing::warn!(error = %e, "internal_server: accept failed");
                break;
            }
        }
    }
}

fn handle_connection(stream: TcpStream, routes: &Routes, token: &str) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Drain request headers (we don't use any). Bail if the client sends
    // pathological garbage past 16 KiB.
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        header_bytes += n;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if header_bytes > 16 * 1024 {
            return write_status(reader.into_inner(), 413, "Payload Too Large", b"");
        }
    }

    let stream = reader.into_inner();
    let parsed = match parse_request_line(&request_line) {
        Some(p) => p,
        None => return write_status(stream, 400, "Bad Request", b""),
    };

    if parsed.method != "GET" {
        return write_status(stream, 405, "Method Not Allowed", b"");
    }

    // Strip auth token: path must start with /<token>/...
    let after_token = match parsed.path.strip_prefix('/') {
        Some(rest) => rest,
        None => return write_status(stream, 400, "Bad Request", b""),
    };
    let (got_token, route_path) = match after_token.split_once('/') {
        Some((t, rest)) => (t, format!("/{rest}")),
        None => (after_token, "/".to_string()),
    };
    if !constant_time_eq(got_token.as_bytes(), token.as_bytes()) {
        return write_status(stream, 403, "Forbidden", b"");
    }

    match routes.lookup(&route_path) {
        Some(entry) => {
            let body = (entry.handler)();
            write_response(stream, 200, "OK", &entry.content_type, &body)
        }
        None => write_status(stream, 404, "Not Found", b""),
    }
}

struct RequestLine<'a> {
    method: &'a str,
    path: String,
}

fn parse_request_line(line: &str) -> Option<RequestLine<'_>> {
    let line = line.trim_end_matches('\n').trim_end_matches('\r');
    let mut parts = line.splitn(3, ' ');
    let method = parts.next()?;
    let raw_path = parts.next()?;
    // Strip query string for routing; the handler doesn't see it. Keep
    // path-only — query parsing is YAGNI for the current internal pages.
    let path = raw_path.split('?').next().unwrap_or("/").to_string();
    Some(RequestLine { method, path })
}

fn write_response(
    mut stream: TcpStream,
    code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         \r\n",
        code = code,
        reason = reason,
        content_type = content_type,
        len = body.len(),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_status(stream: TcpStream, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    write_response(stream, code, reason, "text/plain; charset=utf-8", body)
}

/// Generate a fresh 32-char hex token (128 bits of entropy from the OS).
fn generate_token() -> String {
    let mut buf = [0u8; 16];
    // getrandom is the same API CSPRNG underneath as `rand::rngs::OsRng`
    // but with no transitive deps. Falling back to a timestamp hash would
    // hand a local attacker a trivially-guessable token, so we surface
    // the failure as a panic — a system without /dev/urandom / equivalent
    // is broken in ways buffr can't recover from anyway.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    let mut s = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Constant-time byte slice comparison. Prevents a timing oracle on the
/// auth token, even though the threat model is local-only — defensive
/// hygiene because the cost is negligible.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicU32;

    fn count_handler(counter: Arc<AtomicU32>, body: &'static str) -> Handler {
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            body.as_bytes().to_vec()
        })
    }

    /// Perform a raw `GET <path>` against `addr`. Returns
    /// (status_code, body_bytes). Verbose so tests don't depend on a
    /// real HTTP client.
    fn raw_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("send request");
        stream.flush().unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read response");

        // Parse status line.
        let nl = buf.iter().position(|&b| b == b'\n').expect("status line");
        let status_line = std::str::from_utf8(&buf[..nl]).unwrap();
        let code: u16 = status_line
            .split(' ')
            .nth(1)
            .expect("code")
            .parse()
            .expect("parse code");

        // Find header/body separator.
        let sep = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len() - 4);
        let body = buf[sep + 4..].to_vec();
        (code, body)
    }

    #[test]
    fn server_binds_to_loopback_only() {
        // Sanity: ephemeral port and a real loopback addr.
        let routes = Routes::new();
        let server = InternalServer::start(routes).expect("start");
        assert!(
            server.addr().ip().is_loopback(),
            "server.addr() must be on loopback"
        );
        assert!(server.addr().port() > 0, "kernel must assign a port");
    }

    #[test]
    fn rejects_non_loopback_bind() {
        // Pinning a non-loopback address must be rejected at start_at.
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
        let err = match InternalServer::start_at(addr, Routes::new()) {
            Ok(_) => panic!("non-loopback bind should have been rejected"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_missing_token() {
        let routes = Routes::new().html("/new", Arc::new(|| b"unused".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        // Path without any token prefix — first segment is "new",
        // doesn't match the random hex token, so 403.
        let (code, _) = raw_get(server.addr(), "/new");
        assert_eq!(code, 403, "missing token must 403");
    }

    #[test]
    fn rejects_wrong_token() {
        let routes = Routes::new().html("/new", Arc::new(|| b"unused".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let (code, _) = raw_get(
            server.addr(),
            "/0000000000000000000000000000000000000000/new",
        );
        assert_eq!(code, 403);
    }

    #[test]
    fn serves_route_with_correct_token() {
        let counter = Arc::new(AtomicU32::new(0));
        let routes =
            Routes::new().html("/new", count_handler(Arc::clone(&counter), "<h1>hi</h1>"));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new", server.token());
        let (code, body) = raw_get(server.addr(), &path);
        assert_eq!(code, 200);
        assert_eq!(body, b"<h1>hi</h1>");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "handler invoked once");
    }

    #[test]
    fn handler_invoked_per_request() {
        // Fresh body on every GET — internal pages need this for live
        // keybinds / palette updates.
        let counter = Arc::new(AtomicU32::new(0));
        let routes =
            Routes::new().html("/new", count_handler(Arc::clone(&counter), "<h1>hi</h1>"));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new", server.token());
        for _ in 0..3 {
            let (code, _) = raw_get(server.addr(), &path);
            assert_eq!(code, 200);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn unknown_route_returns_404() {
        let routes = Routes::new().html("/new", Arc::new(|| b"unused".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/does-not-exist", server.token());
        let (code, _) = raw_get(server.addr(), &path);
        assert_eq!(code, 404);
    }

    #[test]
    fn method_not_allowed_for_post() {
        let routes = Routes::new().html("/new", Arc::new(|| b"unused".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new", server.token());

        let mut stream =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        let req =
            format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let status = std::str::from_utf8(&buf).unwrap();
        assert!(status.starts_with("HTTP/1.1 405"), "got: {status}");
    }

    #[test]
    fn url_for_round_trips() {
        let routes = Routes::new().html(
            "/new",
            Arc::new(|| b"<!DOCTYPE html><p>round trip</p>".to_vec()),
        );
        let server = InternalServer::start(routes).expect("start");
        let url = server.url_for("/new");

        // Parse the host:port out of the http://host:port/... URL — we
        // don't depend on a URL crate, just reuse the bound addr.
        assert!(url.starts_with(&format!("http://{}/", server.addr())));
        let path = url.trim_start_matches(&format!("http://{}", server.addr()));
        let (code, body) = raw_get(server.addr(), path);
        assert_eq!(code, 200);
        assert!(body.starts_with(b"<!DOCTYPE html>"));
    }

    #[test]
    fn shutdown_releases_port() {
        // Drop must terminate the accept thread; otherwise we'd leak
        // sockets across tests.
        let routes = Routes::new();
        let server = InternalServer::start(routes).expect("start");
        let port = server.addr().port();
        drop(server);
        // Re-binding succeeds quickly when the prior server actually
        // released the port. Loop briefly for TIME_WAIT in case kernel
        // takes a moment.
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let mut bound = false;
        for _ in 0..20 {
            if TcpListener::bind(bind_addr).is_ok() {
                bound = true;
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        // It's OK if we couldn't rebind on the same exact port (TIME_WAIT
        // varies); we mainly care that the server thread is dead. Smoke-
        // test that by reconnecting and seeing the connection refused.
        let connect = TcpStream::connect_timeout(&bind_addr, Duration::from_millis(200));
        if !bound {
            assert!(
                connect.is_err(),
                "old server still accepting after Drop on port {port}"
            );
        }
    }

    #[test]
    fn token_is_32_hex_chars() {
        let server = InternalServer::start(Routes::new()).expect("start");
        let token = server.token();
        assert_eq!(token.len(), 32);
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "token must be hex, got {token:?}"
        );
    }

    #[test]
    fn tokens_differ_across_instances() {
        let a = InternalServer::start(Routes::new()).expect("start a");
        let b = InternalServer::start(Routes::new()).expect("start b");
        assert_ne!(a.token(), b.token(), "tokens must be per-instance");
    }

    #[test]
    fn query_string_stripped_for_routing() {
        let counter = Arc::new(AtomicU32::new(0));
        let routes =
            Routes::new().html("/new", count_handler(Arc::clone(&counter), "<h1>q</h1>"));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new?foo=bar&baz=qux", server.token());
        let (code, body) = raw_get(server.addr(), &path);
        assert_eq!(code, 200);
        assert_eq!(body, b"<h1>q</h1>");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn constant_time_eq_matches_normal_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
