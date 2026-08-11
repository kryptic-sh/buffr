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
use buffr_core::private_net::{host_resolves_public, is_non_public_host};

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

            // M13: run the fast string gate here — `create` is on CEF's IO
            // thread and must not block on DNS. The DNS-resolving check runs
            // on the fetch worker (see `fetch_and_render`). A rejected
            // target still gets a handler so the user sees the reason
            // instead of a bare CEF error overlay — the handler just starts
            // with the error page pre-rendered and never fetches.
            let rejection = validate_target(&underlying, initiator.as_deref(), false).err();
            if let Some(reason) = rejection.as_deref() {
                tracing::warn!(
                    url = %underlying,
                    initiator = initiator.as_deref().unwrap_or("<none>"),
                    %reason,
                    "buffr-src: refusing to fetch"
                );
            }

            let initiator_host = initiator.as_deref().and_then(http_host);
            Some(BuffrSrcResourceHandler::new(
                Arc::new(Mutex::new(
                    rejection.map(|r| error_page(&underlying, &r).into_bytes()),
                )),
                underlying,
                Arc::new(AtomicUsize::new(0)),
                initiator_host,
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
///
/// With `resolve = false` only the string guard runs — no DNS — so this is
/// the fast gate for CEF's IO thread (`create`). With `resolve = true` the
/// host is resolved and every address classified; this authoritative form
/// runs on the fetch worker thread before the network is touched.
fn validate_target(url: &str, initiator: Option<&str>, resolve: bool) -> Result<(), String> {
    if url.is_empty() {
        return Err("no URL to fetch (buffr-src: prefix with empty suffix)".to_string());
    }
    let Some(host) = http_host(url) else {
        return Err("only http:// and https:// URLs can be viewed as source".to_string());
    };
    let non_public = is_non_public_host(&host) || (resolve && !host_resolves_public(&host));
    if !non_public {
        return Ok(());
    }
    let initiator_host = initiator_host_of(initiator);
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

/// Normalize an initiator reference to a bare lower-cased host.
///
/// The fast gate (`create`) passes a full `http(s)://` frame URL, from
/// which [`http_host`] extracts the host. The worker (`fetch_and_render`)
/// passes the host `create` already extracted — a bare string with no
/// scheme — so `http_host` alone would return `None` there and the
/// same-host exception could never fire on the authoritative path. Accept
/// both shapes; reject anything that still carries a scheme after
/// `http_host` failed (defensive — a malformed initiator must not be able
/// to claim a host).
fn initiator_host_of(initiator: Option<&str>) -> Option<String> {
    initiator.and_then(|i| {
        http_host(i).or_else(|| {
            if i.contains("://") {
                None
            } else {
                Some(i.to_ascii_lowercase())
            }
        })
    })
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
        // Host of the page that triggered the navigation; handed to the
        // worker so the authoritative check can allow a same-host private
        // destination.
        initiator_host: Option<String>,
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
            let initiator_host = self.initiator_host.clone();

            let spawned = std::thread::Builder::new()
                .name("buffr-src-fetch".to_string())
                .spawn(move || {
                    // Held for the whole fetch; released on every exit path.
                    let _permit = permit;
                    let html = fetch_and_render(&url, initiator_host.as_deref());
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
fn fetch_and_render(url: &str, initiator_host: Option<&str>) -> String {
    // `create`'s check is string-only — it runs on CEF's IO thread, which
    // must not block on DNS. This worker-thread call is the authoritative
    // private-network gate, DNS resolution included, and runs before the
    // fetch touches the network.
    if let Err(reason) = validate_target(url, initiator_host, true) {
        return error_page(url, &reason);
    }

    let result = (|| -> Result<Vec<u8>, String> {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_recv_response(Some(std::time::Duration::from_secs(10)))
            // §16-2: never follow redirects — `validate_target` gates the
            // URL it is given, and every hop after a 3xx would be fetched
            // with no re-validation, so a public origin could 302 into a
            // loopback/RFC1918 address. `max_redirects(0)` returns the 3xx
            // as-is, which the 200..300 check below turns into an error page.
            .max_redirects(0)
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
    fn validate_rejects_non_http_schemes() {
        assert!(validate_target("file:///etc/passwd", None, false).is_err());
        assert!(validate_target("data:text/html,<b>hi", None, false).is_err());
        assert!(validate_target("", None, false).is_err());
    }

    #[test]
    fn validate_rejects_ssrf_from_public_page() {
        // The exact payload from the finding.
        let err = validate_target(
            "http://127.0.0.1:8080/admin",
            Some("https://evil.example/"),
            false,
        )
        .unwrap_err();
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(
            validate_target(
                "http://169.254.169.254/latest/meta-data/",
                Some("https://evil.example/"),
                false
            )
            .is_err()
        );
        // No initiator at all is treated as untrusted.
        assert!(validate_target("http://192.168.1.1/", None, false).is_err());
        // The integer form of the cloud-metadata endpoint: previously
        // slipped past the guard as "public" (getaddrinfo resolves it to
        // 169.254.169.254).
        assert!(
            validate_target(
                "http://2852039166/latest/meta-data/",
                Some("https://evil.example/"),
                false
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
                Some("http://127.0.0.1:41235/tok/new"),
                false
            )
            .is_ok()
        );
        // Different port on the same host is fine; different host is not.
        assert!(
            validate_target(
                "http://127.0.0.1:9/x",
                Some("http://127.0.0.1:41235/y"),
                false
            )
            .is_ok()
        );
        assert!(
            validate_target(
                "http://127.0.0.1:8080/x",
                Some("http://127.0.0.1:8080/"),
                false
            )
            .is_ok()
        );
        assert!(
            validate_target("http://10.0.0.5/x", Some("http://127.0.0.1:41235/y"), false).is_err()
        );
    }

    #[test]
    fn validate_same_host_bare_initiator() {
        // The worker path: `create` extracts the initiator host and passes
        // it as a bare string (no scheme). The same-host exception must
        // still fire there, or view-source of a `buffr://` internal page
        // always lands on the error page.
        assert!(
            validate_target("http://127.0.0.1:41235/tok/new", Some("127.0.0.1"), true).is_ok(),
            "bare-host initiator on the same host must pass the resolved gate"
        );
        // A different bare host is still refused.
        assert!(
            validate_target("http://127.0.0.1:41235/tok/new", Some("127.0.0.2"), true).is_err()
        );
        // A full-URL initiator keeps working on the same gate.
        assert!(
            validate_target(
                "http://127.0.0.1:41235/tok/new",
                Some("http://127.0.0.1:41235/tok/new"),
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_allows_ordinary_public_pages() {
        assert!(
            validate_target(
                "https://93.184.216.34/page",
                Some("https://other.example/"),
                false
            )
            .is_ok()
        );
        assert!(validate_target("http://93.184.216.34/page", None, false).is_ok());

        // The resolve=true (worker) path, offline-safe via IP literals.
        assert!(
            validate_target("http://93.184.216.34/page", None, true).is_ok(),
            "public literal passes the resolved gate"
        );
        assert!(
            validate_target(
                "http://127.0.0.1:8080/admin",
                Some("https://evil.example/"),
                true
            )
            .is_err(),
            "private literal fails the resolved gate"
        );
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

    // ── §16-2: no redirect following ─────────────────────────────────────

    /// Serve one canned HTTP response on an ephemeral loopback port.
    struct OneShotServer {
        addr: std::net::SocketAddr,
        thread: std::thread::JoinHandle<()>,
    }

    impl OneShotServer {
        fn spawn(response: String) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let thread = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    use std::io::Write;
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self { addr, thread }
        }
    }

    #[test]
    fn fetch_does_not_follow_redirects() {
        // The gate clears only the URL it is given; a redirect hop must
        // never be fetched. A 3xx surfaces as an error page, not as the
        // redirect target's body.
        //
        // The target server is expected to receive NO connection — the
        // redirect must not be followed — so its thread parks in accept()
        // and is never joined.
        let target = OneShotServer::spawn(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
             Content-Length: 21\r\n\r\nREDIRECT_TARGET_BODY"
                .into(),
        );
        let redirect = OneShotServer::spawn(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/target\r\n\
             Content-Length: 0\r\n\r\n",
            target.addr
        ));

        // Same-host initiator (bare form, as `create` passes to the worker)
        // lets the loopback fetch through the private-network gate so the
        // redirect behavior itself is what is exercised.
        let page = fetch_and_render(
            &format!("http://{}/redir", redirect.addr),
            Some("127.0.0.1"),
        );
        assert!(
            page.contains("HTTP 302"),
            "a redirect must surface as an error page, got: {page}"
        );
        assert!(
            !page.contains("REDIRECT_TARGET_BODY"),
            "the redirect target must never be fetched"
        );
        // Join only the redirect server (it served one request). The target
        // server's accept() stays parked for the rest of the test process;
        // never join it.
        let _ = redirect.thread.join();
    }
}
