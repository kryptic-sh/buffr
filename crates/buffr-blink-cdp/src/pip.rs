//! Picture-in-Picture support for the blink-cdp backend (Phase 8g, #90).
//!
//! # Approach
//!
//! The HTML `requestPictureInPicture()` API is fully available in headless
//! Chromium. Unlike the CEF backend (which cannot surface PiP — see issue #31),
//! blink-cdp can invoke PiP by evaluating a small IIFE via `Runtime.evaluate`
//! on the active session.
//!
//! # Video selection heuristic
//!
//! When multiple `<video>` elements are present, the IIFE picks:
//!   1. The currently playing video (not paused, not ended).
//!   2. A video with audio (not muted).
//!   3. The first video element in document order.
//!
//! This heuristic covers the common cases (single-video pages, video feeds
//! where one item is playing). For precise control on multi-video pages the
//! user can click the video first and then invoke PiP.
//!
//! # Toggle semantics
//!
//! If the chosen video is already the `document.pictureInPictureElement` the
//! IIFE calls `exitPictureInPicture()` instead, so a single keybind toggles.

// ── JS helper ─────────────────────────────────────────────────────────────────

/// Returns the IIFE that toggles Picture-in-Picture on the most relevant video.
///
/// The expression is suitable for direct use as the `expression` field of a
/// `Runtime.evaluate` CDP command with `returnByValue: true`.
///
/// The IIFE returns a plain object:
/// - `{ ok: false, reason: 'no-video' }` — no `<video>` elements found.
/// - `{ ok: true, action: 'enter' }` — `requestPictureInPicture()` called.
/// - `{ ok: true, action: 'exit' }` — `exitPictureInPicture()` called.
///
/// Failures from the async PiP API are caught and forwarded to `console.warn`
/// so they appear in DevTools without crashing the page context.
pub fn pip_toggle_js() -> &'static str {
    r#"(function() {
        // Find the most relevant video to PiP. Preference order:
        //   1. currently playing video
        //   2. video with audio
        //   3. first video element
        const videos = Array.from(document.querySelectorAll('video'));
        if (videos.length === 0) return { ok: false, reason: 'no-video' };

        const playing = videos.find(v => !v.paused && !v.ended);
        const target = playing || videos.find(v => !v.muted) || videos[0];

        // Toggle: if already in PiP, exit; otherwise request.
        if (document.pictureInPictureElement === target) {
            document.exitPictureInPicture()
                .catch(e => console.warn('[buffr] exitPiP failed', e));
            return { ok: true, action: 'exit' };
        }

        target.requestPictureInPicture()
            .catch(e => console.warn('[buffr] requestPiP failed', e));
        return { ok: true, action: 'enter' };
    })()"#
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_toggle_js_contains_required_calls() {
        let js = pip_toggle_js();
        assert!(
            js.contains("requestPictureInPicture"),
            "JS must call requestPictureInPicture"
        );
        assert!(
            js.contains("exitPictureInPicture"),
            "JS must call exitPictureInPicture"
        );
        assert!(
            js.contains("pictureInPictureElement"),
            "JS must check pictureInPictureElement for toggle detection"
        );
    }

    #[test]
    fn pip_toggle_js_handles_no_video() {
        let js = pip_toggle_js();
        assert!(
            js.contains("no-video"),
            "JS must short-circuit with 'no-video' when no <video> elements exist"
        );
    }
}
