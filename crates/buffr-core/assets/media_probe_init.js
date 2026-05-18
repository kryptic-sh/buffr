// buffr media-activity probe — init script (patched-constructor phase).
//
// Injected once per main-frame load via `LoadHandler::on_load_end →
// frame.execute_java_script`. Patches three browser APIs to register
// activity in a shared namespace object that the poll script reads on
// each probe tick.
//
// Namespace: `window.__buffr_media_state`
//   .playingMedia   — WeakSet<HTMLMediaElement>  elements currently un-paused
//   .peerConnections — Set<RTCPeerConnection>     live RTC connections
//   .wakeLocks      — Set<WakeLockSentinel>       un-released wake locks
//
// Guard: `window.__buffr_media_probe_installed` prevents double-installation
// on SPA soft-navigations that re-trigger on_load_end.

(function () {
    'use strict';

    if (window.__buffr_media_probe_installed) { return; }
    window.__buffr_media_probe_installed = true;

    // Shared state namespace — init before any patch runs.
    var state = window.__buffr_media_state;
    if (!state) {
        state = {
            playingMedia: new WeakSet(),
            peerConnections: new Set(),
            wakeLocks: new Set(),
        };
        window.__buffr_media_state = state;
    }

    // ── Signal 1: HTMLMediaElement.prototype.play / pause / ended ─────────
    //
    // Tracks <video> and <audio> elements that are actively playing, including
    // silent/muted video and autoplay GIF-as-video pages that never touch
    // navigator.mediaSession. We patch `play` to register the element and
    // attach one-shot `pause` / `ended` listeners to de-register it.
    try {
        var origPlay = HTMLMediaElement.prototype.play;
        HTMLMediaElement.prototype.play = function buffr_play() {
            var el = this;
            // Register immediately — the actual play may be async but the
            // intent is clear. If the promise rejects the element stays
            // paused and the poll will see `el.paused === true`.
            state.playingMedia.add(el);

            function onDone() {
                el.removeEventListener('pause', onDone);
                el.removeEventListener('ended', onDone);
                // Leave the element in the WeakSet — the poll checks
                // `el.paused` directly so stale entries are harmless.
            }
            el.addEventListener('pause', onDone, { once: true });
            el.addEventListener('ended', onDone, { once: true });

            return origPlay.apply(el, arguments);
        };
    } catch (_e) {}

    // ── Signal 2: RTCPeerConnection constructor ────────────────────────────
    //
    // Registers every new RTCPeerConnection so the poll can check whether
    // any still has a live connectionState. We keep a plain Set (not WeakSet)
    // because we need to iterate. Closed connections are pruned by the poll.
    try {
        if (typeof RTCPeerConnection !== 'undefined') {
            var OrigRTCPeerConnection = RTCPeerConnection;
            window.RTCPeerConnection = function buffr_RTCPeerConnection() {
                var pc = new OrigRTCPeerConnection(...arguments);
                state.peerConnections.add(pc);

                // Prune from Set when fully closed so the set doesn't grow
                // unboundedly on pages that create many short-lived connections.
                pc.addEventListener('connectionstatechange', function () {
                    if (pc.connectionState === 'closed') {
                        state.peerConnections.delete(pc);
                    }
                });

                return pc;
            };
            // Copy static methods / properties (e.g. generateCertificate)
            // by chaining the patched ctor's __proto__ to the original.
            Object.setPrototypeOf(window.RTCPeerConnection, OrigRTCPeerConnection);
            // Alias the prototype property so `pc instanceof RTCPeerConnection`
            // still works — pc's prototype chain is rooted at the original
            // prototype (since `new OrigRTCPeerConnection(...)` was used), so
            // the patched ctor's `.prototype` must BE that same object, not
            // merely chain to it.
            window.RTCPeerConnection.prototype = OrigRTCPeerConnection.prototype;
        }
    } catch (_e) {}

    // ── Signal 3: navigator.wakeLock.request ──────────────────────────────
    //
    // Patches the Screen Wake Lock API so that any page calling
    // `navigator.wakeLock.request('screen')` is tracked. The sentinel
    // object's `released` property tells us when the lock has been dropped.
    try {
        if (navigator.wakeLock && typeof navigator.wakeLock.request === 'function') {
            var origWakeLockRequest = navigator.wakeLock.request.bind(navigator.wakeLock);
            navigator.wakeLock.request = function buffr_wakeLockRequest(type) {
                var promise = origWakeLockRequest(type);
                promise.then(function (sentinel) {
                    state.wakeLocks.add(sentinel);
                    // Remove from set once released to keep the set lean.
                    sentinel.addEventListener('release', function () {
                        state.wakeLocks.delete(sentinel);
                    }, { once: true });
                }).catch(function () {});
                return promise;
            };
        }
    } catch (_e) {}
})();
