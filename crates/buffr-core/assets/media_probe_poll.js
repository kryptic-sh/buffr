// buffr media-activity probe — poll script.
//
// Executed every ~2 s by `BrowserHost::run_media_probe`. Recomputes
// `window.__buffr_media_active` and `window.__buffr_video_active` from all
// five signal sources and writes boolean results. The companion init script
// (`media_probe_init.js`) must have run first to install the patched
// constructors; if it hasn't, the three patched-constructor signals simply
// read as false.
//
// This file is a TEMPLATE: `media_probe::build_poll_script` substitutes
// `%%SENTINEL%%` with the sentinel + the per-page-load nonce before the
// script is evaluated. Evaluating it verbatim emits lines Rust rejects.
//
// Signal sources and how they map to each flag:
//
//   Signal                                       | media_active | video_active
//   ─────────────────────────────────────────────┼──────────────┼─────────────
//   1. mediaSession.playbackState === 'playing'  | YES          | YES (treated
//      (YouTube, Spotify, etc.)                  |              | as video by
//                                                |              | default)
//   2. fullscreenElement instanceof              | YES          | YES
//      HTMLVideoElement                          |              |
//   3. Playing <video> element tracked by        | YES          | YES
//      __buffr_media_state.playingMedia          |              |
//   3. Playing <audio> element tracked by        | YES          | NO
//      __buffr_media_state.playingMedia          |              |
//   4. RTCPeerConnection with video sender/recv  | YES          | YES
//   4. RTCPeerConnection with audio-only         | YES          | NO
//   5. WakeLockSentinel with released === false  | YES          | YES (page
//      (navigator.wakeLock.request('screen'))    |              | explicitly
//                                                |              | wants screen)

(function () {
    'use strict';

    var active = false;
    var videoActive = false;

    // ── Signal 1: mediaSession ─────────────────────────────────────────────
    // Treat mediaSession as a video signal: Chrome uses mediaSession for
    // both audio and video, but issue #22 says: treat as video by default so
    // music doesn't keep the screen on (users who want that can set
    // idle_inhibit.inhibit_audio_only = true in config).
    try {
        if (navigator.mediaSession && navigator.mediaSession.playbackState === 'playing') {
            active = true;
            videoActive = true;
        }
    } catch (_e) {}

    // ── Signal 2: fullscreen video ─────────────────────────────────────────
    try {
        if (document.fullscreenElement instanceof HTMLVideoElement) {
            active = true;
            videoActive = true;
        }
    } catch (_e) {}

    // ── Signal 3: playing media elements (patched-ctor) ───────────────────
    // The WeakSet has no iterator; instead we scan all <video> and <audio>
    // elements on the page and check paused + tracked-membership.
    try {
        var state = window.__buffr_media_state;
        if (state && state.playingMedia) {
            var els = document.querySelectorAll('video, audio');
            for (var i = 0; i < els.length; i++) {
                var el = els[i];
                if (!el.paused && state.playingMedia.has(el)) {
                    active = true;
                    if (el.tagName === 'VIDEO') {
                        videoActive = true;
                    }
                }
            }
        }
    } catch (_e) {}

    // ── Signal 4: RTCPeerConnection (patched-ctor) ─────────────────────────
    // Video is active if any track of kind 'video' is present on a live
    // connection; audio-only connections contribute to media_active only.
    try {
        var state = window.__buffr_media_state;
        if (state && state.peerConnections && state.peerConnections.size > 0) {
            state.peerConnections.forEach(function (pc) {
                if (pc.connectionState !== 'closed' &&
                    pc.connectionState !== 'failed' &&
                    pc.iceConnectionState !== 'closed') {
                    active = true;
                    // Check whether the connection carries any video track.
                    try {
                        var tracks = pc.getSenders().concat(pc.getReceivers());
                        for (var j = 0; j < tracks.length; j++) {
                            if (tracks[j].track && tracks[j].track.kind === 'video') {
                                videoActive = true;
                                break;
                            }
                        }
                    } catch (_e2) {}
                }
            });
        }
    } catch (_e) {}

    // ── Signal 5: Screen Wake Lock (patched-ctor) ──────────────────────────
    // A page that explicitly requests a screen wake lock wants the screen on —
    // treat as both media and video active.
    try {
        var state = window.__buffr_media_state;
        if (state && state.wakeLocks && state.wakeLocks.size > 0) {
            state.wakeLocks.forEach(function (sentinel) {
                if (!sentinel.released) {
                    active = true;
                    videoActive = true;
                }
            });
        }
    } catch (_e) {}

    window.__buffr_media_active = active;
    window.__buffr_video_active = videoActive;

    // ── IPC: emit sentinel-prefixed JSON when state changes ──────────────
    // The Rust DisplayHandler::on_console_message scrapes the sentinel and
    // updates BrowserHost's video_active atomic. Same console-log shape as
    // edit.js / hint.js. Cache the last emitted payload so we only fire a
    // line on transitions — silent pages would otherwise spam every ~2 s.
    //
    // `%%SENTINEL%%` is substituted by `media_probe::build_poll_script` with
    // `__buffr_media__:` + the per-page-load nonce + `:`. The Rust side only
    // accepts lines carrying the nonce it currently holds for this browser,
    // which is what stops an ad iframe from pinning the idle inhibitor on.
    // Keep it a local `var`: never publish it on `window`.
    try {
        var SENTINEL = '%%SENTINEL%%';
        var prev = window.__buffr_media_last_emit;
        if (!prev || prev.media !== active || prev.video !== videoActive) {
            window.__buffr_media_last_emit = { media: active, video: videoActive };
            console.log(SENTINEL + JSON.stringify({
                media: active,
                video: videoActive
            }));
        }
    } catch (_e) {}
})();
