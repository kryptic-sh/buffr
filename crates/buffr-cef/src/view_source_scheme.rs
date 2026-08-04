//! Custom `buffr-src:` scheme served by a [`SchemeHandlerFactory`].
//!
//! Round 2 of issue #30: stop rewriting `buffr-src:` → `view-source:` at the
//! navigation boundary. Instead, register `buffr-src` as a real CEF custom
//! scheme whose handler fetches the underlying URL on a worker thread and
//! renders it with [`buffr_view_source::render`] (bonsai syntax highlighting).
//!
//! # Usage
//!
//! 1. Call [`register_buffr_src_scheme`] from `on_register_custom_schemes`
//!    **before** `cef::initialize`.
//! 2. Call [`register_buffr_src_handler_factory`] once **after**
//!    `cef::initialize` succeeds.

use cef::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::host::BUFFR_SRC_PREFIX;
use crate::html::html_escape;

/// Register the `buffr-src` scheme with CEF.
///
/// Must be called from `ImplApp::on_register_custom_schemes` **before**
/// `cef::initialize`. Mirrors the flags used for `buffr://` in `new_tab.rs`.
pub fn register_buffr_src_scheme(registrar: &mut cef::SchemeRegistrar) {
    let scheme = CefString::from("buffr-src");
    // SchemeOptions::get_raw() returns u32 on Linux but i32 on Windows
    // (cef-sys bindings reflect the underlying C int width). Allow the
    // platform-dependent cast — on Windows clippy sees i32 → i32 as
    // redundant; on Linux the cast is real.
    //
    // M13: `CORS_ENABLED | FETCH_ENABLED` are deliberately NOT set. With
    // them, ordinary web content could `fetch('buffr-src:http://…')` and
    // `fetch_and_render` would perform the request from the *browser*
    // process — outside Chromium's network stack, so same-origin policy,
    // CSP and private-network-access checks are all bypassed. View-source
    // only ever needs a browser-initiated top-level navigation, which
    // `STANDARD | SECURE` already allows.
    #[allow(clippy::unnecessary_cast)]
    let opts = (SchemeOptions::STANDARD.get_raw() | SchemeOptions::SECURE.get_raw()) as i32;
    registrar.add_custom_scheme(Some(&scheme), opts);
}

/// Register the scheme handler factory for `buffr-src:`.
///
/// Must be called **after** `cef::initialize` returns successfully.
pub fn register_buffr_src_handler_factory() {
    let scheme = CefString::from("buffr-src");
    let mut factory = BuffrSrcSchemeHandlerFactory::new();
    cef::register_scheme_handler_factory(Some(&scheme), None, Some(&mut factory));
}

// ---------------------------------------------------------------------------
// URL helper
// ---------------------------------------------------------------------------

/// Strip the `buffr-src:` prefix from a CEF-routed URL to obtain the
/// underlying URL that should be fetched.
///
/// - `"buffr-src:https://example.com"` → `"https://example.com"`
/// - `"buffr-src:"` → `""` (empty underlying URL)
/// - Anything that does **not** start with the prefix is returned as-is
///   (defensive: CEF only routes registered-scheme URLs to this factory).
pub(crate) fn underlying_url(buffr_src_url: &str) -> &str {
    buffr_src_url
        .strip_prefix(BUFFR_SRC_PREFIX)
        .unwrap_or(buffr_src_url)
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

wrap_scheme_handler_factory! {
    pub struct BuffrSrcSchemeHandlerFactory {}

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut cef::Browser>,
            frame: Option<&mut cef::Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut cef::Request>,
        ) -> Option<cef::ResourceHandler> {
            // Extract the `buffr-src:` URL from the incoming request.
            let buffr_src_url = request
                .map(|r| CefStringUtf16::from(&r.url()).to_string())
                .unwrap_or_default();

            let underlying = underlying_url(&buffr_src_url).to_owned();

            // The URL of the page that triggered this load. For a
            // browser-initiated top-level navigation this is the page the
            // user was on when they hit "view source", which is exactly
            // the origin allowed to reach its own private-network host.
            let initiator = frame.map(|f| CefStringUtf16::from(&f.url()).to_string());

            // M13: validate before the handler is ever constructed. A
            // rejected target still gets a handler so the user sees the
            // reason instead of a bare CEF error overlay — the handler just
            // starts with the error page pre-rendered and never fetches.
            let rejection = validate_target(&underlying, initiator.as_deref()).err();
            if let Some(reason) = rejection.as_deref() {
                tracing::warn!(
                    url = %underlying,
                    initiator = initiator.as_deref().unwrap_or("<none>"),
                    %reason,
                    "buffr-src: refusing to fetch"
                );
            }

            Some(BuffrSrcResourceHandler::new(
                Arc::new(Mutex::new(
                    rejection.map(|r| error_page(&underlying, &r).into_bytes()),
                )),
                underlying,
                Arc::new(AtomicUsize::new(0)),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Target validation (M13)
// ---------------------------------------------------------------------------

/// Reject anything `buffr-src:` must never fetch.
///
/// Two rules:
///
/// 1. The underlying URL must be `http` or `https`. Everything else
///    (`file:`, `data:`, `ftp:`, another `buffr-src:`, …) is refused —
///    `fetch_and_render` runs in the **browser** process, so a `file:` URL
///    would be a direct local-disk read with no Chromium mediation.
/// 2. Non-public destinations — loopback, link-local (169.254/16 incl. the
///    cloud metadata endpoint), and RFC1918 — are refused **unless** the
///    page that triggered the navigation is already on that same host.
///    That keeps "view source of a `buffr://` internal page" working (the
///    internal server is on 127.0.0.1) while blocking a public page from
///    pivoting into the local network.
fn validate_target(url: &str, initiator: Option<&str>) -> Result<(), String> {
    if url.is_empty() {
        return Err("no URL to fetch (buffr-src: prefix with empty suffix)".to_string());
    }
    let Some(host) = http_host(url) else {
        return Err("only http:// and https:// URLs can be viewed as source".to_string());
    };
    if !is_non_public_host(&host) {
        return Ok(());
    }
    let initiator_host = initiator.and_then(http_host);
    if initiator_host.as_deref() == Some(host.as_str()) {
        return Ok(());
    }
    Err(format!(
        "refusing to fetch private-network host `{host}` from a page on \
         `{}` — view-source of a local address is only allowed from that \
         same host",
        initiator_host.as_deref().unwrap_or("<unknown origin>")
    ))
}

/// Extract the lower-cased host of an `http`/`https` URL.
///
/// Returns `None` for any other scheme or a malformed authority. Strips
/// `userinfo@`, the `:port` suffix, and `[...]` around IPv6 literals — a
/// deliberately small parser so this crate does not take a `url` dep for
/// one call site.
fn http_host(url: &str) -> Option<String> {
    let rest = {
        let lower = url.get(..8).unwrap_or_default().to_ascii_lowercase();
        if lower.starts_with("http://") {
            &url[7..]
        } else if lower.starts_with("https://") {
            &url[8..]
        } else {
            return None;
        }
    };
    // Authority runs to the first '/', '?' or '#'.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Drop any userinfo (everything up to the LAST '@' — a password may
    // itself contain '@').
    let hostport = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    if hostport.is_empty() {
        return None;
    }
    // IPv6 literal: `[::1]:8080`.
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default()
    } else {
        hostport.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// `true` when `host` names a loopback, link-local, unique-local or
/// RFC1918 destination. Conservative: an unparseable literal that *looks*
/// numeric is treated as non-public.
fn is_non_public_host(host: &str) -> bool {
    // Hostname forms.
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    // IPv6 literals (already unbracketed by `http_host`).
    if host.contains(':') {
        let h = host.split('%').next().unwrap_or(host); // strip zone id
        if h == "::1" || h == "::" {
            return true;
        }
        // fe80::/10 link-local, fc00::/7 unique-local.
        if h.starts_with("fe8")
            || h.starts_with("fe9")
            || h.starts_with("fea")
            || h.starts_with("feb")
            || h.starts_with("fc")
            || h.starts_with("fd")
        {
            return true;
        }
        // Anything else must be a well-formed IPv6 literal before it is
        // trusted as public — glibc's getaddrinfo otherwise reinterprets
        // other numeric forms (e.g. `::ffff:7f00:1` is 127.0.0.1 in v6
        // clothing).
        if let Ok(addr) = h.parse::<std::net::Ipv6Addr>() {
            return match addr.to_ipv4_mapped() {
                // IPv4-mapped: classify by the embedded v4 address.
                Some(v4) => is_non_public_v4(v4),
                // A non-mapped, non-private v6 literal is public.
                None => false,
            };
        }
        // Unparseable "IPv6" that still looks numeric: fail closed. Note
        // `looks_numeric` deliberately excludes ':', so an invalid
        // colon-literal falls to the public side where the fetch fails
        // anyway — harmless.
        return looks_numeric(h);
    }
    // Canonical dotted quad (Rust's parser is strict: it rejects octal,
    // hex, integer and shorthand forms that glibc would resolve).
    if let Ok(addr) = host.parse::<std::net::Ipv4Addr>() {
        return is_non_public_v4(addr);
    }
    // Not canonical, but getaddrinfo could still resolve it numerically
    // (2852039166, 0177.0.0.1, 127.1, 0x7f.0.0.1, …) — fail closed.
    looks_numeric(host)
}

/// `true` when `host` is an IPv4 address in a non-public range.
fn is_non_public_v4(addr: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = addr.octets();
    a == 0                                    // 0.0.0.0/8 "this network"
        || a == 127                           // loopback
        || a == 10                            // RFC1918
        || (a == 172 && (16..=31).contains(&b))// RFC1918
        || (a == 192 && b == 168)             // RFC1918
        || (a == 169 && b == 254)             // link-local + cloud metadata
        || (a == 100 && (64..=127).contains(&b)) // CGNAT
        || a >= 224 // multicast + reserved
}

/// `true` when `s` is a bare numeric-looking literal — every character is a
/// hex digit, `.`, `x` or `X` — i.e. something glibc's `getaddrinfo` could
/// resolve as a number. A DNS name (letters beyond hex, dashes, …) returns
/// `false` and stays "public"; DNS rebinding is out of scope for this guard.
fn looks_numeric(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | 'x' | 'X'))
}

// ---------------------------------------------------------------------------
// Handler state
// ---------------------------------------------------------------------------

/// Shared rendered body. `None` until the worker thread finishes.
type BodySlot = Arc<Mutex<Option<Vec<u8>>>>;

// ---------------------------------------------------------------------------
// Resource handler
// ---------------------------------------------------------------------------

wrap_resource_handler! {
    pub struct BuffrSrcResourceHandler {
        // Rendered HTML body. Populated by the worker thread.
        body: BodySlot,
        // Underlying URL to fetch (everything after `buffr-src:`).
        underlying_url: String,
        // Read cursor into `body`.
        cursor: Arc<AtomicUsize>,
    }

    impl ResourceHandler {
        // CEF calls `open` first. Spawn a worker thread to fetch + render,
        // then call `callback.cont()` when bytes are ready.
        // Returning false (0) from `open` makes CEF wait for callback.cont().
        fn open(
            &self,
            _request: Option<&mut cef::Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            callback: Option<&mut cef::Callback>,
        ) -> ::std::os::raw::c_int {
            // Rebound as `mut` so the `handle_request` out-param can be
            // written on whichever exit path we end up taking.
            let mut handle_request = handle_request;

            // If `create` already rejected the target (M13) the body is
            // pre-filled with the error page — serve it synchronously and
            // never touch the network.
            let already_resolved = self.body.lock().map(|g| g.is_some()).unwrap_or(false);

            // CEF callback must be called from another thread to continue
            // the resource load once the body is ready.
            //
            // Safety: cef::Callback is Send per cef-rs's design; we ship it
            // across the thread boundary via the closure.
            //
            // L33: `callback` is `None` when CEF hands us a null Callback.
            // Spawning the worker then would leave the load pending forever
            // with nothing to call `cont()` — the tab spins until the user
            // kills it. Fall through to the synchronous path instead.
            let callback_arc: Option<cef::Callback> = callback.map(|c| {
                // `c` is `&mut cef::Callback`; we need an owned copy.
                // The CEF wrapper objects are ref-counted, so this clone
                // is safe and keeps the callback alive on the worker.
                c.clone()
            });

            // Synchronous completion: `handle_request = 1` + return 1 tells
            // CEF to read whatever is in `self.body` right now.
            let mut serve_now = |slf: &Self, reason: Option<&str>| {
                if let Some(reason) = reason
                    && let Ok(mut slot) = slf.body.lock()
                {
                    *slot = Some(error_page(&slf.underlying_url, reason).into_bytes());
                }
                if let Some(hr) = handle_request.as_deref_mut() {
                    *hr = 1;
                }
                1
            };

            let Some(callback_arc) = callback_arc.filter(|_| !already_resolved) else {
                let reason = if already_resolved {
                    None
                } else {
                    tracing::warn!(
                        url = %self.underlying_url,
                        "buffr-src: no resource callback from CEF — serving error page"
                    );
                    Some("internal error: CEF supplied no resource callback")
                };
                return serve_now(self, reason);
            };

            // M14: bound the worker-thread fan-out. Without this an
            // attacker-controlled request loop spawns one OS thread per
            // request, each parked on a 10 s connect + 10 s recv timeout.
            let Some(permit) = FetchPermit::acquire() else {
                tracing::warn!(
                    url = %self.underlying_url,
                    cap = MAX_INFLIGHT_FETCHES,
                    "buffr-src: fetch pool saturated — refusing request"
                );
                return serve_now(
                    self,
                    Some("too many concurrent view-source fetches — try again"),
                );
            };

            let body_slot = Arc::clone(&self.body);
            let url = self.underlying_url.clone();

            let spawned = std::thread::Builder::new()
                .name("buffr-src-fetch".to_string())
                .spawn(move || {
                    // Held for the whole fetch; released on every exit path.
                    let _permit = permit;
                    let html = fetch_and_render(&url);
                    let bytes = html.into_bytes();
                    if let Ok(mut slot) = body_slot.lock() {
                        *slot = Some(bytes);
                    }
                    // Tell CEF the response is ready.
                    callback_arc.cont();
                });

            if let Err(err) = spawned {
                // Thread spawn failed (fd/thread exhaustion). Complete
                // synchronously rather than leaving CEF waiting forever.
                tracing::warn!(error = %err, "buffr-src: worker spawn failed");
                return serve_now(self, Some("worker spawn failed"));
            }

            // Signal that we will handle this request but NOT synchronously:
            // CEF waits for `callback.cont()` before proceeding.
            if let Some(hr) = handle_request {
                *hr = 0;
            }
            // Return 0 (false): request is pending, wait for callback.cont().
            0
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let body_len = self
                .body
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|b| b.len()))
                .unwrap_or(0);

            if let Some(r) = response {
                r.set_status(200);
                let mime = CefString::from("text/html; charset=utf-8");
                r.set_mime_type(Some(&mime));
            }
            if let Some(len) = response_length {
                *len = body_len as i64;
            }
        }

        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut cef::ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let guard = match self.body.lock() {
                Ok(g) => g,
                Err(_) => {
                    if let Some(br) = bytes_read {
                        *br = 0;
                    }
                    return 0;
                }
            };

            let bytes = match guard.as_ref() {
                Some(b) => b,
                None => {
                    if let Some(br) = bytes_read {
                        *br = 0;
                    }
                    return 0;
                }
            };

            let len = bytes.len();
            let pos = self.cursor.load(Ordering::SeqCst);

            if pos >= len || bytes_to_read <= 0 {
                if let Some(br) = bytes_read {
                    *br = 0;
                }
                // EOF
                return 0;
            }

            let remaining = len - pos;
            let to_copy = remaining.min(bytes_to_read as usize);

            // Safety: CEF guarantees `data_out` is a valid writable buffer of
            // at least `bytes_to_read` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr().add(pos), data_out, to_copy);
            }

            self.cursor.store(pos + to_copy, Ordering::SeqCst);

            if let Some(br) = bytes_read {
                *br = to_copy as i32;
            }

            1
        }
    }
}

// ---------------------------------------------------------------------------
// Worker: fetch + render
// ---------------------------------------------------------------------------

/// Maximum response body size before aborting (10 MiB, matches the renderer).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Maximum number of `buffr-src:` fetches in flight at once (M14).
///
/// View-source is a deliberate, one-at-a-time user action; a handful of
/// concurrent fetches covers reload-spam and a few pinned tabs restoring at
/// startup. Anything beyond that is a runaway loop, and each worker parks on
/// a 10 s connect + 10 s recv timeout, so an unbounded spawn is a
/// thread-exhaustion DoS.
const MAX_INFLIGHT_FETCHES: usize = 8;

/// Number of `buffr-src:` worker threads currently running.
static INFLIGHT_FETCHES: AtomicUsize = AtomicUsize::new(0);

/// RAII slot in the bounded fetch pool. Decrements [`INFLIGHT_FETCHES`] on
/// drop, so a panicking or early-returning worker cannot leak capacity.
struct FetchPermit;

impl FetchPermit {
    /// Claim a slot, or `None` when the pool is saturated.
    fn acquire() -> Option<Self> {
        let mut current = INFLIGHT_FETCHES.load(Ordering::Acquire);
        loop {
            if current >= MAX_INFLIGHT_FETCHES {
                return None;
            }
            match INFLIGHT_FETCHES.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(FetchPermit),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for FetchPermit {
    fn drop(&mut self) {
        INFLIGHT_FETCHES.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Fetch `url` via ureq and render it with `buffr_view_source::render`.
///
/// On any error (empty URL, network failure, non-2xx, body too large)
/// returns an HTML error page so the user sees *something* useful rather
/// than a CEF error overlay.
fn fetch_and_render(url: &str) -> String {
    // Belt-and-braces: `create` already validated the target, but this is
    // the function that actually touches the network so it re-checks.
    // `initiator = None` here — a same-host private destination that was
    // approved at `create` time would be rejected again, so the caller must
    // not reach this path for one (it doesn't: rejected targets never spawn
    // a worker).
    if url.is_empty() {
        return error_page(url, "no URL to fetch (buffr-src: prefix with empty suffix)");
    }
    if http_host(url).is_none() {
        return error_page(
            url,
            "only http:// and https:// URLs can be viewed as source",
        );
    }

    let result = (|| -> Result<Vec<u8>, String> {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_recv_response(Some(std::time::Duration::from_secs(10)))
            .build();
        let agent = ureq::Agent::new_with_config(config);

        let mut resp = agent
            .get(url)
            .call()
            .map_err(|e| format!("network error: {e}"))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }

        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES as u64)
            .read_to_vec()
            .map_err(|e| format!("read error: {e}"))?;

        Ok(body)
    })();

    match result {
        Ok(body) => buffr_view_source::render(url, &body),
        Err(err) => error_page(url, &err),
    }
}

/// Build a minimal HTML error page shown when fetching fails.
fn error_page(url: &str, reason: &str) -> String {
    let escaped_url = html_escape(url);
    let escaped_reason = html_escape(reason);
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>buffr-src: error</title>
<style>
html,body{{margin:0;padding:1em 1.5em;background:#1a1b26;color:#c0caf5;
font-family:"SF Mono",Menlo,Consolas,monospace;font-size:13px;line-height:1.5}}
.err{{color:#f7768e;}}
</style>
</head>
<body>
<p class="err"><strong>Failed to fetch source for <code>{escaped_url}</code>:</strong></p>
<pre>{escaped_reason}</pre>
</body>
</html>"#
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underlying_url_strips_prefix() {
        assert_eq!(
            underlying_url("buffr-src:https://example.com"),
            "https://example.com"
        );
    }

    #[test]
    fn underlying_url_empty_suffix() {
        assert_eq!(underlying_url("buffr-src:"), "");
    }

    #[test]
    fn underlying_url_no_prefix_passthrough() {
        // Defensive: should not happen in practice since CEF only routes
        // registered-scheme URLs to this factory.
        assert_eq!(underlying_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn error_page_contains_url() {
        let page = error_page("https://example.com", "timeout");
        assert!(page.contains("https://example.com"));
        assert!(page.contains("timeout"));
    }

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
    }

    // ── M13: scheme + private-network validation ──────────────────────────

    #[test]
    fn http_host_extracts_host() {
        assert_eq!(
            http_host("http://example.com/a/b"),
            Some("example.com".into())
        );
        assert_eq!(http_host("https://example.com"), Some("example.com".into()));
        assert_eq!(
            http_host("https://EXAMPLE.com:8443/x?y#z"),
            Some("example.com".into())
        );
        assert_eq!(
            http_host("http://user:p@ss@127.0.0.1:8080/admin"),
            Some("127.0.0.1".into())
        );
        assert_eq!(http_host("http://[::1]:8080/x"), Some("::1".into()));
    }

    #[test]
    fn http_host_rejects_other_schemes() {
        assert_eq!(http_host("file:///etc/passwd"), None);
        assert_eq!(http_host("data:text/html,<b>x"), None);
        assert_eq!(http_host("ftp://example.com/x"), None);
        assert_eq!(http_host("buffr-src:https://example.com"), None);
        assert_eq!(http_host(""), None);
        assert_eq!(http_host("http://"), None);
        assert_eq!(http_host("http:///path-only"), None);
    }

    #[test]
    fn non_public_hosts_are_detected() {
        for h in [
            "127.0.0.1",
            "127.1.2.3",
            "localhost",
            "foo.localhost",
            "printer.local",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
        ] {
            assert!(is_non_public_host(h), "{h} should be non-public");
        }
    }

    #[test]
    fn public_hosts_are_allowed() {
        for h in [
            "example.com",
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "172.15.0.1",
            "192.169.0.1",
            "169.253.0.1",
            "2606:4700::1111",
            "2606:4700:4700::1111",
        ] {
            assert!(!is_non_public_host(h), "{h} should be public");
        }
    }

    #[test]
    fn glibc_numeric_forms_are_non_public() {
        // glibc's getaddrinfo accepts all of these numeric forms even though
        // Rust's strict Ipv4Addr parser rejects them. Resolutions (verified
        // on glibc): 2852039166 → 169.254.169.254, 0177.0.0.1 → 127.0.0.1,
        // 127.1 → 127.0.0.1, 0x7f.0.0.1 → 127.0.0.1, 2130706433 → 127.0.0.1,
        // ::ffff:7f00:1 → 127.0.0.1.
        for h in [
            "2852039166",
            "0177.0.0.1",
            "127.1",
            "0x7f.0.0.1",
            "2130706433",
            "::ffff:7f00:1",
        ] {
            assert!(is_non_public_host(h), "{h} should be non-public");
        }
    }

    #[test]
    fn validate_rejects_non_http_schemes() {
        assert!(validate_target("file:///etc/passwd", None).is_err());
        assert!(validate_target("data:text/html,<b>hi", None).is_err());
        assert!(validate_target("", None).is_err());
    }

    #[test]
    fn validate_rejects_ssrf_from_public_page() {
        // The exact payload from the finding.
        let err = validate_target("http://127.0.0.1:8080/admin", Some("https://evil.example/"))
            .unwrap_err();
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(
            validate_target(
                "http://169.254.169.254/latest/meta-data/",
                Some("https://evil.example/")
            )
            .is_err()
        );
        // No initiator at all is treated as untrusted.
        assert!(validate_target("http://192.168.1.1/", None).is_err());
        // The integer form of the cloud-metadata endpoint: previously
        // slipped past the guard as "public" (getaddrinfo resolves it to
        // 169.254.169.254).
        assert!(
            validate_target(
                "http://2852039166/latest/meta-data/",
                Some("https://evil.example/")
            )
            .is_err()
        );
    }

    #[test]
    fn validate_allows_same_host_private_target() {
        // View-source of a `buffr://` internal page: the page is already
        // served from the loopback internal server.
        assert!(
            validate_target(
                "http://127.0.0.1:41235/tok/new",
                Some("http://127.0.0.1:41235/tok/new")
            )
            .is_ok()
        );
        // Different port on the same host is fine; different host is not.
        assert!(validate_target("http://127.0.0.1:9/x", Some("http://127.0.0.1:41235/y")).is_ok());
        assert!(validate_target("http://127.0.0.1:8080/x", Some("http://127.0.0.1:8080/")).is_ok());
        assert!(validate_target("http://10.0.0.5/x", Some("http://127.0.0.1:41235/y")).is_err());
    }

    #[test]
    fn validate_allows_ordinary_public_pages() {
        assert!(
            validate_target("https://example.com/page", Some("https://other.example/")).is_ok()
        );
        assert!(validate_target("http://example.com/page", None).is_ok());
    }

    // ── M14: bounded fetch pool ───────────────────────────────────────────

    #[test]
    fn fetch_permits_are_capped_and_released() {
        let baseline = INFLIGHT_FETCHES.load(Ordering::Acquire);
        let mut held = Vec::new();
        while let Some(p) = FetchPermit::acquire() {
            held.push(p);
            assert!(
                held.len() <= MAX_INFLIGHT_FETCHES,
                "acquire() handed out more than the cap"
            );
        }
        assert_eq!(held.len(), MAX_INFLIGHT_FETCHES - baseline);
        assert!(FetchPermit::acquire().is_none(), "pool should be saturated");
        drop(held);
        assert_eq!(INFLIGHT_FETCHES.load(Ordering::Acquire), baseline);
        assert!(
            FetchPermit::acquire().is_some(),
            "capacity must come back after the permits drop"
        );
    }
}
