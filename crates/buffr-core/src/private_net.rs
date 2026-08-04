//! Shared guard for destinations a browser-process fetch must never touch.
//!
//! Moved here from `buffr-cef`'s view-source handler (A1) so the same
//! fail-closed rule backs every browser-process network read — view-source
//! and "Copy Image" alike — without drifting into a second copy.

/// `true` when `host` names a loopback, link-local, unique-local or
/// RFC1918 destination. Conservative: an unparseable literal that *looks*
/// numeric is treated as non-public.
pub fn is_non_public_host(host: &str) -> bool {
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
pub(crate) fn is_non_public_v4(addr: std::net::Ipv4Addr) -> bool {
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
pub(crate) fn looks_numeric(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | 'x' | 'X'))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
