//! Per-page-load nonces for the renderer → browser console-log IPC.
//!
//! # Why
//!
//! buffr's `hint` / `edit` / `media_probe` subsystems ship JS that talks
//! back to the browser process by writing `console.log("<sentinel>" + …)`
//! lines, which [`crate::console_sentinel`] scrapes out of
//! `DisplayHandler::on_console_message`. That callback carries **no frame
//! argument**, so before this module existed *any* frame — including a
//! third-party ad iframe — could emit one of the three fixed, publicly
//! documented sentinel prefixes and drive buffr's internals: overwrite the
//! live hint session (turning the user's next hint keystroke into a click on
//! an attacker-chosen element), pin the platform idle inhibitor on so the
//! screen never locks, or push attacker-chosen text into the
//! yank-to-clipboard path.
//!
//! # How
//!
//! Each injected script carries a fresh random token spliced in at build
//! time; the wire format becomes `<sentinel><nonce>:<json>` and the Rust
//! parsers reject anything whose nonce is not the one currently minted for
//! that browser. Nonces are only ever spliced into scripts injected into a
//! **main frame**, so subframes never learn one — which is exactly the
//! cross-frame forgery case above.
//!
//! # Threat model — what this does and does not buy
//!
//! The injected script runs *in the page*, so the nonce is not a secret
//! from a determined same-document attacker: a page that replaces
//! `console.log` before injection sees every line we emit, nonce included,
//! and can then forge for its own top frame. What rotation buys is:
//!
//! - subframes / cross-origin iframes can never forge (they are never
//!   injected into, so they never see a nonce);
//! - a forged event is confined to the page load that leaked the nonce —
//!   [`ConsoleNonces::rotate_page`] runs on every main-frame load;
//! - hint sessions get their own token ([`ConsoleNonces::rotate_hint`]), so
//!   a nonce leaked during one hint session is dead by the next one.
//!
//! The complete fix is a real IPC channel (a renderer-process handler
//! sending `cef_process_message_t`) instead of console scraping; until then
//! this is defence in depth, not a boundary.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

/// Nonce length in hex characters (128 bits of entropy).
///
/// Fixed-width matters: the wire format is `<sentinel><nonce>:<json>`, and
/// the parser strips the nonce by prefix comparison rather than by scanning
/// for the separator.
pub const CONSOLE_NONCE_LEN: usize = 32;

/// Separator between the nonce and the JSON payload on the wire.
pub const CONSOLE_NONCE_SEPARATOR: char = ':';

/// Mint a fresh 128-bit nonce, lower-case hex.
///
/// Entropy comes from the OS CSPRNG via `getrandom`. If the kernel refuses
/// (which in practice means the process is in a broken sandbox), we log at
/// `error` and fall back to a splitmix64 stream seeded from the clock, a
/// stack address and a counter. That fallback is *not* cryptographically
/// strong and is only there so that a failure to read entropy degrades the
/// hardening rather than bricking hint / edit / media-probe entirely.
pub fn new_console_nonce() -> String {
    let mut bytes = [0u8; CONSOLE_NONCE_LEN / 2];
    if let Err(err) = getrandom::fill(&mut bytes) {
        tracing::error!(
            error = %err,
            "console nonce: OS RNG unavailable — falling back to a weak nonce"
        );
        fallback_entropy(&mut bytes);
    }
    let mut out = String::with_capacity(CONSOLE_NONCE_LEN);
    for b in bytes {
        // Infallible: writing to a String never fails.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Last-resort entropy when `getrandom` fails. See [`new_console_nonce`].
fn fallback_entropy(bytes: &mut [u8; CONSOLE_NONCE_LEN / 2]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let ctr = COUNTER.fetch_add(1, Ordering::Relaxed);
    let addr = bytes.as_ptr() as usize as u64;
    let mut state = nanos ^ ctr.rotate_left(32) ^ addr.rotate_left(17);

    for chunk in bytes.chunks_mut(8) {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        for (dst, src) in chunk.iter_mut().zip(z.to_le_bytes()) {
            *dst = src;
        }
    }
}

/// The two live nonces for one browser.
///
/// `page` covers the scripts that live for the whole document (`edit.js`,
/// the media probe poll). `hint` is separate so a hint session can rotate
/// without invalidating the already-wired `edit.js` listeners.
#[derive(Debug, Clone)]
struct TabNonces {
    page: String,
    hint: String,
}

impl TabNonces {
    fn fresh() -> Self {
        Self {
            page: new_console_nonce(),
            hint: new_console_nonce(),
        }
    }
}

/// Per-browser console-IPC nonce table, shared between the script
/// injectors (load handler / host) and the console scraper (display
/// handler).
///
/// Cheap to clone — the map is behind an `Arc<Mutex<…>>`.
///
/// All accessors fail *closed*: if the mutex is poisoned they hand back a
/// freshly minted nonce that was never stored, so verification cannot
/// succeed by accident.
#[derive(Debug, Clone, Default)]
pub struct ConsoleNonces {
    inner: Arc<Mutex<HashMap<i32, TabNonces>>>,
}

impl ConsoleNonces {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a brand-new page **and** hint nonce for `browser_id` and return
    /// the page nonce.
    ///
    /// Call this once per main-frame load, immediately before injecting the
    /// document-lifetime scripts. Rotating the hint nonce too is deliberate:
    /// a hint session never survives a navigation.
    pub fn rotate_page(&self, browser_id: i32) -> String {
        let fresh = TabNonces::fresh();
        let page = fresh.page.clone();
        if let Ok(mut map) = self.inner.lock() {
            map.insert(browser_id, fresh);
            page
        } else {
            new_console_nonce()
        }
    }

    /// Mint a new hint nonce for `browser_id`, leaving the page nonce alone.
    ///
    /// Call this immediately before injecting `hint.js`.
    pub fn rotate_hint(&self, browser_id: i32) -> String {
        let hint = new_console_nonce();
        if let Ok(mut map) = self.inner.lock() {
            let entry = map.entry(browser_id).or_insert_with(TabNonces::fresh);
            entry.hint.clone_from(&hint);
            hint
        } else {
            new_console_nonce()
        }
    }

    /// Current page nonce for `browser_id`, minting a fresh entry if the
    /// browser is unknown.
    ///
    /// Minting-on-miss is safe: an unknown browser has never been injected
    /// into, so the fresh value matches nothing and any sentinel line it
    /// produces is rejected.
    pub fn page(&self, browser_id: i32) -> String {
        self.get_or_mint(browser_id, |n| n.page.clone())
    }

    /// Current hint nonce for `browser_id`. See [`Self::page`].
    pub fn hint(&self, browser_id: i32) -> String {
        self.get_or_mint(browser_id, |n| n.hint.clone())
    }

    fn get_or_mint(&self, browser_id: i32, pick: impl FnOnce(&TabNonces) -> String) -> String {
        match self.inner.lock() {
            Ok(mut map) => pick(map.entry(browser_id).or_insert_with(TabNonces::fresh)),
            Err(_) => new_console_nonce(),
        }
    }

    /// Drop the entry for a closed browser so the table doesn't grow with
    /// the tab churn of a long session.
    pub fn forget(&self, browser_id: i32) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&browser_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn nonce_is_fixed_width_lowercase_hex() {
        let n = new_console_nonce();
        assert_eq!(n.len(), CONSOLE_NONCE_LEN);
        assert!(
            n.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "nonce must be lower-case ASCII hex: {n}"
        );
    }

    #[test]
    fn nonces_do_not_repeat() {
        let set: HashSet<String> = (0..256).map(|_| new_console_nonce()).collect();
        assert_eq!(set.len(), 256, "duplicate nonce out of 256 draws");
    }

    #[test]
    fn fallback_entropy_is_not_constant() {
        let mut a = [0u8; CONSOLE_NONCE_LEN / 2];
        let mut b = [0u8; CONSOLE_NONCE_LEN / 2];
        fallback_entropy(&mut a);
        fallback_entropy(&mut b);
        assert_ne!(a, b);
        assert_ne!(a, [0u8; CONSOLE_NONCE_LEN / 2]);
    }

    #[test]
    fn page_nonce_is_stable_until_rotated() {
        let nonces = ConsoleNonces::new();
        let first = nonces.rotate_page(1);
        assert_eq!(nonces.page(1), first);
        assert_eq!(nonces.page(1), first);
        let second = nonces.rotate_page(1);
        assert_ne!(first, second, "nonce must change across page loads");
        assert_eq!(nonces.page(1), second);
    }

    #[test]
    fn page_and_hint_nonces_are_independent() {
        let nonces = ConsoleNonces::new();
        let page = nonces.rotate_page(7);
        let hint_a = nonces.hint(7);
        assert_ne!(page, hint_a);

        let hint_b = nonces.rotate_hint(7);
        assert_ne!(hint_a, hint_b, "hint nonce must change per session");
        assert_eq!(nonces.page(7), page, "rotating hint must not touch page");
    }

    #[test]
    fn page_load_rotates_the_hint_nonce_too() {
        let nonces = ConsoleNonces::new();
        nonces.rotate_page(3);
        let hint_before = nonces.hint(3);
        nonces.rotate_page(3);
        assert_ne!(hint_before, nonces.hint(3));
    }

    #[test]
    fn browsers_do_not_share_nonces() {
        let nonces = ConsoleNonces::new();
        assert_ne!(nonces.rotate_page(1), nonces.rotate_page(2));
    }

    #[test]
    fn unknown_browser_mints_rather_than_matching() {
        let nonces = ConsoleNonces::new();
        let a = nonces.page(42);
        assert_eq!(a.len(), CONSOLE_NONCE_LEN);
        // The minted entry persists rather than re-rolling per lookup.
        assert_eq!(nonces.page(42), a, "unknown-browser nonce must be stable");
    }

    #[test]
    fn forget_drops_the_entry() {
        let nonces = ConsoleNonces::new();
        let first = nonces.rotate_page(9);
        nonces.forget(9);
        assert_ne!(nonces.page(9), first, "forgotten browser must mint afresh");
    }
}
