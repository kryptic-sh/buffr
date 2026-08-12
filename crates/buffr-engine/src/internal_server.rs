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
//! [`InternalServer::start`] spawns a background thread running a
//! *non-blocking* `accept` loop that polls a shutdown flag every
//! [`ACCEPT_POLL_INTERVAL`]. [`InternalServer`]'s `Drop` impl sets the
//! flag and joins the thread — no self-connect, so shutdown can't be
//! wedged by a saturated backlog.
//!
//! ## Limits
//!
//! Every input is bounded, because the listener is reachable from any
//! local process and from any web page that guesses the port (a
//! cross-origin `fetch` still transmits the request even though CORS
//! blocks reading the response):
//!
//! - request line: [`MAX_REQUEST_LINE_BYTES`] → `414`
//! - header block: [`MAX_HEADER_BYTES`], enforced *while* reading so a
//!   single newline-less line can't grow unbounded → `413`
//! - concurrent connections: [`MAX_INFLIGHT_CONNECTIONS`] → `503`

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How often the accept loop wakes to check the shutdown flag. Keeps
/// `Drop` responsive without burning a core.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Hard cap on the request line (method + path + version).
const MAX_REQUEST_LINE_BYTES: usize = 32 * 1024;

/// Hard cap on the whole header block. Enforced as a `take` over the
/// reader, so it also bounds one pathological newline-less line.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Maximum connections being served concurrently. Each connection owns
/// an OS thread that can idle for up to the read timeout, so this is
/// also the cap on threads a flood of `fetch()` calls can create.
/// Internal pages are served to one browser process; 32 is generous.
const MAX_INFLIGHT_CONNECTIONS: usize = 32;

/// RAII token for one in-flight connection. Releasing it on `Drop`
/// means panics in a handler can't leak capacity.
struct InflightGuard(Arc<AtomicUsize>);

impl InflightGuard {
    /// Reserve a slot, or `None` when the server is already at
    /// [`MAX_INFLIGHT_CONNECTIONS`].
    fn acquire(counter: &Arc<AtomicUsize>) -> Option<Self> {
        if counter.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT_CONNECTIONS {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(counter)))
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

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
        // Non-blocking accept + shutdown-flag polling is what lets
        // `Drop` join the thread without any wake-up trickery.
        listener.set_nonblocking(true)?;
        let actual_addr = listener.local_addr()?;

        let token = generate_token();
        let shutdown = Arc::new(AtomicBool::new(false));
        let routes_mu = Arc::new(Mutex::new(routes));

        let thread = {
            let shutdown = Arc::clone(&shutdown);
            let routes = Arc::clone(&routes_mu);
            let token = token.clone();
            let port = actual_addr.port();
            thread::Builder::new()
                .name("buffr-internal-server".into())
                .spawn(move || accept_loop(listener, shutdown, routes, token, port))?
        };

        Ok(Self {
            addr: actual_addr,
            token,
            shutdown,
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
}

impl Drop for InternalServer {
    fn drop(&mut self) {
        // The accept loop is non-blocking and re-reads this flag every
        // ACCEPT_POLL_INTERVAL, so it exits on its own — no self-connect
        // (which used to wedge shutdown whenever the connect failed).
        self.shutdown.store(true, Ordering::SeqCst);
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
    port: u16,
) {
    // `start_at` already put the listener in non-blocking mode; assert
    // it here too because the loop's shutdown responsiveness depends on
    // `accept` returning WouldBlock rather than parking. If the socket
    // can't be made non-blocking we fail closed (stop serving) instead
    // of parking forever in `accept` and hanging the process on exit.
    if let Err(e) = listener.set_nonblocking(true) {
        tracing::error!(error = %e, "internal_server: set_nonblocking(true) failed; not serving");
        return;
    }

    let inflight = Arc::new(AtomicUsize::new(0));

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                // Backpressure: over the cap we answer 503 inline and
                // close, rather than spawning an unbounded number of
                // threads that each idle for the full read timeout.
                let Some(guard) = InflightGuard::acquire(&inflight) else {
                    tracing::warn!(
                        cap = MAX_INFLIGHT_CONNECTIONS,
                        "internal_server: connection cap reached, rejecting with 503"
                    );
                    reject_overloaded(stream);
                    continue;
                };
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
                if let Err(e) = thread::Builder::new()
                    .name("buffr-internal-conn".into())
                    .spawn(move || {
                        // Held for the connection's lifetime; released
                        // even if the handler panics.
                        let _guard = guard;
                        if let Err(e) = handle_connection(stream, &routes_snapshot, &token, port) {
                            tracing::debug!(error = %e, "internal_server: connection error");
                        }
                    })
                {
                    tracing::warn!(error = %e, "internal_server: failed to spawn connection handler");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                // A8: EMFILE (transient fd exhaustion) and ECONNABORTED (reset
                // before accept) are common under load and must not take the
                // internal server down for the process lifetime — the old `break`
                // silently killed every buffr://new / buffr://settings page. There
                // is no portable errno distinction between those and a truly dead
                // listener, so retry every unexpected error after the poll interval;
                // a corrupted listener fd (the only fatal case) costs one warn per
                // ACCEPT_POLL_INTERVAL until Drop sets the shutdown flag.
                tracing::warn!(error = %e, "internal_server: accept failed; retrying");
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

/// Answer an over-cap connection from the accept thread itself. Short
/// timeouts and no body: a client that refuses to read must not stall
/// the accept loop.
fn reject_overloaded(stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let _ = write_status(stream, 503, "Service Unavailable", b"");
}

fn handle_connection(stream: TcpStream, routes: &Routes, token: &str, port: u16) -> io::Result<()> {
    // Windows inherits the listener's non-blocking mode on accepted
    // sockets; force blocking so the read timeouts below are what
    // actually bound this connection.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(stream);

    // Read the request line with a 32 KiB cap to prevent OOM from a
    // pathological request path (loopback-only, but defence-in-depth).
    let mut request_line = Vec::<u8>::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break; // EOF
        }
        let cap_remaining = MAX_REQUEST_LINE_BYTES - request_line.len();
        let limit = available.len().min(cap_remaining);
        let newline_pos = available[..limit].iter().position(|&b| b == b'\n');
        match newline_pos {
            Some(pos) => {
                request_line.extend_from_slice(&available[..=pos]);
                reader.consume(pos + 1);
                break;
            }
            None => {
                request_line.extend_from_slice(&available[..limit]);
                reader.consume(limit);
                if limit == 0 || cap_remaining <= limit {
                    // Hit the 32 KiB cap without seeing a newline.
                    return write_status(reader.into_inner(), 414, "URI Too Long", b"");
                }
            }
        }
    }

    // Read the headers. `Host` is the only one we keep (rebinding
    // defence below); the rest are dropped.
    //
    // The MAX_HEADER_BYTES cap is applied by wrapping the reader in a
    // `take` *before* reading, not by measuring completed lines: a
    // client that sends `X: ` followed by 64 MiB of `a` and no newline
    // would otherwise grow the line buffer without bound (OOM) because
    // the check only ran once a full line had been buffered.
    let mut host_header: Option<String> = None;
    let mut header_overflow = false;
    {
        let mut limited = reader.by_ref().take(MAX_HEADER_BYTES as u64);
        loop {
            let mut line = String::new();
            let n = limited.read_line(&mut line)?;
            if n == 0 {
                // Ok(0) is EOF *or* the cap being exhausted mid-line.
                header_overflow = limited.limit() == 0;
                break;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("host")
                && host_header.is_none()
            {
                host_header = Some(value.trim().to_ascii_lowercase());
            }
        }
    }

    let stream = reader.into_inner();
    if header_overflow {
        return write_status(stream, 413, "Payload Too Large", b"");
    }
    let request_line = match String::from_utf8(request_line) {
        Ok(s) => s,
        Err(_) => {
            return write_status(stream, 400, "Bad Request", b"non-UTF-8 request line");
        }
    };
    let parsed = match parse_request_line(&request_line) {
        Some(p) => p,
        None => return write_status(stream, 400, "Bad Request", b""),
    };

    if parsed.method != "GET" {
        return write_status(stream, 405, "Method Not Allowed", b"");
    }

    // DNS-rebinding defence: a browser always sends `Host`, and a page
    // on `http://attacker.example` that has rebound its name to
    // 127.0.0.1 sends `Host: attacker.example`. Only our own
    // loopback authority is accepted.
    if let Some(host) = host_header.as_deref()
        && !host_is_ours(host, port)
    {
        return write_status(stream, 403, "Forbidden", b"bad Host header");
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

/// Is `host` (already lowercased, `Host` header value) an authority
/// that names *this* server?
///
/// Accepts the loopback literals and `localhost`, each with our own
/// port — `127.0.0.1:1234`, `localhost:1234`, `[::1]:1234`. Anything
/// else, including a bare hostname with no port, is rejected: real
/// clients reach us through the URL [`InternalServer::url_for`] built,
/// which always carries the port.
fn host_is_ours(host: &str, port: u16) -> bool {
    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    if host_port.parse::<u16>() != Ok(port) {
        return false;
    }
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
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
    // `Referrer-Policy: no-referrer` matters because the auth token
    // lives in the URL path (see `url_for`): without it, any external
    // link or subresource an internal page ever gains would hand the
    // token to a third party in the `Referer` header.
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
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
    getrandom::fill(&mut buf).expect("OS CSPRNG unavailable");
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

    /// Send `req` verbatim to `addr` and return the whole response.
    fn raw_exchange(addr: SocketAddr, req: &str) -> Vec<u8> {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(req.as_bytes()).expect("send request");
        stream.flush().unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read response");
        buf
    }

    /// Perform a raw `GET <path>` with the given `Host` header value.
    fn raw_get_with_host(addr: SocketAddr, path: &str, host: &str) -> (u16, Vec<u8>) {
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        parse_response(raw_exchange(addr, &req))
    }

    /// Perform a raw `GET <path>` against `addr`. Returns
    /// (status_code, body_bytes). Verbose so tests don't depend on a
    /// real HTTP client.
    fn raw_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
        raw_get_with_host(addr, path, &addr.to_string())
    }

    fn parse_response(buf: Vec<u8>) -> (u16, Vec<u8>) {
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
        let sep = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(buf.len() - 4);
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
        let routes = Routes::new().html("/new", count_handler(Arc::clone(&counter), "<h1>hi</h1>"));
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
        let routes = Routes::new().html("/new", count_handler(Arc::clone(&counter), "<h1>hi</h1>"));
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

        let host = server.addr().to_string();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let buf = raw_exchange(server.addr(), &req);
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
        let routes = Routes::new().html("/new", count_handler(Arc::clone(&counter), "<h1>q</h1>"));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new?foo=bar&baz=qux", server.token());
        let (code, body) = raw_get(server.addr(), &path);
        assert_eq!(code, 200);
        assert_eq!(body, b"<h1>q</h1>");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rejects_oversized_request_line() {
        // A request-line path longer than 32 KiB must be rejected with 414.
        let routes = Routes::new();
        let server = InternalServer::start(routes).expect("start");
        let token = server.token();
        let target = format!("/{}/a", token);
        // Total path must exceed 32 KiB. Account for "/<token>/" prefix.
        let pad = (32 * 1024 + 1usize).saturating_sub(target.len());
        let oversized = format!("{}/{}", target, "a".repeat(pad));

        let mut stream =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        let host = server.addr();
        let req = format!("GET {oversized} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        // Send the request. The server may close before the write
        // completes — that's fine, the cap was enforced.
        let _ = stream.write_all(req.as_bytes());
        // Give the server a moment to send the 414 response before
        // the RST packet arrives.
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => {
                // EOF = server closed without sending data; acceptable.
            }
            Ok(n) => {
                let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
                assert!(
                    resp.starts_with("HTTP/1.1 414"),
                    "oversized path should get 414, got: {resp}"
                );
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                // Server reset after sending 414; the cap was enforced.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn rejects_unbounded_header_line() {
        // Regression (H10): the header drain used `read_line` straight
        // off the reader, so a header with no newline grew the line
        // buffer without limit — `X: ` + 64 MiB of `a` was buffered in
        // full. The cap now applies *while* reading, so the server
        // answers 413 after 16 KiB and never buffers the rest.
        let routes = Routes::new().html("/new", Arc::new(|| b"unused".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new", server.token());
        let host = server.addr();

        let mut stream =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nX: ").as_bytes())
            .unwrap();
        // 1 MiB of a single header line, never newline-terminated.
        let chunk = vec![b'a'; 64 * 1024];
        let mut sent = 0usize;
        while sent < 1024 * 1024 {
            if stream.write_all(&chunk).is_err() {
                // Server closed on us after the 413 — that's the point.
                break;
            }
            sent += chunk.len();
        }
        let _ = stream.flush();

        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => panic!("server closed without answering the oversized header"),
            Ok(n) => {
                let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
                assert!(
                    resp.starts_with("HTTP/1.1 413"),
                    "oversized header must get 413, got: {resp}"
                );
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                // Server answered and reset before we drained it.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn caps_concurrent_connections_with_503() {
        // Regression (M11): every accepted connection used to spawn an
        // unbounded OS thread that idled for the full 2 s read timeout,
        // so `for (…) fetch('http://127.0.0.1:PORT/')` from a page could
        // spawn thousands. Over the cap we now answer 503 and close.
        let routes = Routes::new().html("/new", Arc::new(|| b"unused".to_vec()));
        let server = InternalServer::start(routes).expect("start");

        // Hold MAX_INFLIGHT_CONNECTIONS sockets open sending nothing —
        // each one pins a connection slot until its read timeout.
        let mut held = Vec::new();
        for _ in 0..MAX_INFLIGHT_CONNECTIONS {
            held.push(
                TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).expect("hold"),
            );
        }

        // Give the accept loop a moment to pick all of them up. This
        // has to stay well inside the 2 s per-connection read timeout,
        // after which the held slots start freeing themselves.
        thread::sleep(Duration::from_millis(200));

        let mut probe =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).expect("probe");
        probe
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buf = Vec::new();
        let _ = probe.read_to_end(&mut buf);
        assert!(
            buf.starts_with(b"HTTP/1.1 503"),
            "connection past the cap of {MAX_INFLIGHT_CONNECTIONS} must be refused with 503, got: {:?}",
            String::from_utf8_lossy(&buf[..buf.len().min(64)])
        );
        drop(held);
    }

    #[test]
    fn drop_returns_promptly_with_a_connection_in_flight() {
        // Regression (M12 / L28): shutdown used to depend on a
        // self-connect to break a *blocking* accept, and joined
        // unconditionally — a failed connect parked the accept thread
        // forever. The loop is now non-blocking and polls the flag, so
        // Drop returns within roughly one poll interval even while a
        // silent client occupies a connection thread.
        let server = InternalServer::start(Routes::new()).expect("start");
        let _silent = TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(100));

        let start = std::time::Instant::now();
        drop(server);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "Drop must not block on connection threads or a self-connect; took {elapsed:?}"
        );
    }

    #[test]
    fn response_carries_referrer_policy_no_referrer() {
        // Regression (L35): the auth token is in the URL path, so any
        // future external link on an internal page would leak it via
        // `Referer` without this header.
        let routes = Routes::new().html("/new", Arc::new(|| b"<h1>hi</h1>".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let host = server.addr();
        let path = format!("/{}/new", server.token());
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        let resp = String::from_utf8(raw_exchange(server.addr(), &req)).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
        assert!(
            resp.contains("Referrer-Policy: no-referrer\r\n"),
            "missing Referrer-Policy in: {resp}"
        );
        // …on error responses too.
        let bad = format!("GET /nope HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        let resp = String::from_utf8(raw_exchange(server.addr(), &bad)).unwrap();
        assert!(resp.starts_with("HTTP/1.1 403"));
        assert!(resp.contains("Referrer-Policy: no-referrer\r\n"));
    }

    #[test]
    fn rejects_foreign_host_header() {
        // DNS-rebinding defence (L35): a page served from
        // attacker.example whose name has been rebound to 127.0.0.1
        // still sends its own Host.
        let routes = Routes::new().html("/new", Arc::new(|| b"secret".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new", server.token());
        let port = server.addr().port();

        for host in [
            format!("attacker.example:{port}"),
            "attacker.example".to_string(),
            // Right name, wrong port → not our authority.
            format!("127.0.0.1:{}", port.wrapping_add(1)),
            // No port at all: our own URLs always carry one.
            "127.0.0.1".to_string(),
            "localhost".to_string(),
        ] {
            let (code, _) = raw_get_with_host(server.addr(), &path, &host);
            assert_eq!(code, 403, "Host: {host} must be rejected");
        }
    }

    #[test]
    fn accepts_loopback_host_headers() {
        let routes = Routes::new().html("/new", Arc::new(|| b"ok".to_vec()));
        let server = InternalServer::start(routes).expect("start");
        let path = format!("/{}/new", server.token());
        let port = server.addr().port();

        for host in [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            // Header values are case-insensitive for the hostname.
            format!("LOCALHOST:{port}"),
            format!("[::1]:{port}"),
        ] {
            let (code, body) = raw_get_with_host(server.addr(), &path, &host);
            assert_eq!(code, 200, "Host: {host} must be accepted");
            assert_eq!(body, b"ok");
        }
    }

    #[test]
    fn host_is_ours_matches_only_our_authority() {
        assert!(host_is_ours("127.0.0.1:8080", 8080));
        assert!(host_is_ours("localhost:8080", 8080));
        assert!(host_is_ours("[::1]:8080", 8080));
        assert!(!host_is_ours("127.0.0.1:8081", 8080));
        assert!(!host_is_ours("127.0.0.1", 8080));
        assert!(!host_is_ours("evil.example:8080", 8080));
        assert!(!host_is_ours("", 8080));
        assert!(!host_is_ours("127.0.0.1:", 8080));
        assert!(!host_is_ours("127.0.0.1:notaport", 8080));
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
