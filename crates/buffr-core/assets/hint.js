// buffr hint mode -- DOM-injected overlay.
//
// This script is injected via `cef::Frame::execute_java_script`. The Rust
// caller substitutes three placeholders before calling `execute_java_script`:
//
//   __ALPHABET__   -- JS string literal, the configured label alphabet.
//   __LABELS__     -- JS array literal, [["a"], ["s"], ...] (one entry per
//                    enumerated element, in document order).
//   __SELECTORS__  -- JS string literal, CSS selector list.
//
// IPC (renderer -> browser) uses `console.log` with a sentinel prefix:
// `__buffr_hint__:` + JSON. The buffr `DisplayHandler::on_console_message`
// scrapes those lines. This avoids `cef_process_message_t` which would
// require a renderer-side `RenderProcessHandler` + V8 extension.
//
// The script is wrapped in an IIFE so each `enter_hint_mode` invocation
// gets a fresh closure. All host-callable entries are stashed on
// `window.__buffrHint*` and overwritten on re-entry.

(function () {
    'use strict';

    var ALPHABET = '__ALPHABET__';
    var LABELS = __LABELS__;
    var SELECTORS = '__SELECTORS__';
    var SENTINEL = '__buffr_hint__:';
    var STYLE_ID = 'buffr-hint-style';
    var CLASS = 'buffr-hint-overlay';

    // ---- cleanup any prior session ---------------------------------------
    if (window.__buffrHintCancel) {
        try { window.__buffrHintCancel(); } catch (e) {}
    }

    // ---- style ----------------------------------------------------------
    if (!document.getElementById(STYLE_ID)) {
        var s = document.createElement('style');
        s.id = STYLE_ID;
        s.textContent =
            '.' + CLASS + '{' +
            'position:fixed;' +
            'z-index:2147483647;' +
            'background:#FFD83A;' +
            'color:#000;' +
            'border:1px solid #C8AA10;' +
            'border-radius:3px;' +
            'padding:0 3px;' +
            'font:bold 11px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",monospace;' +
            'pointer-events:none;' +
            'box-shadow:0 1px 2px rgba(0,0,0,0.4);' +
            'text-transform:lowercase;' +
            '}' +
            '.' + CLASS + ' .buffr-hint-typed-prefix{' +
            'opacity:0.45;' +
            'text-decoration:line-through;' +
            '}' +
            '.' + CLASS + '.buffr-hint-hidden{display:none !important;}';
        (document.head || document.documentElement).appendChild(s);
    }

    // ---- enumerate clickable, in-viewport elements ----------------------
    function isVisible(el) {
        var rect = el.getBoundingClientRect();
        if (rect.width <= 1 || rect.height <= 1) return null;
        var vw = window.innerWidth || document.documentElement.clientWidth;
        var vh = window.innerHeight || document.documentElement.clientHeight;
        if (rect.bottom < 0 || rect.right < 0) return null;
        if (rect.top > vh || rect.left > vw) return null;
        var style = window.getComputedStyle(el);
        if (style.visibility === 'hidden' || style.display === 'none') return null;
        if (parseFloat(style.opacity || '1') < 0.1) return null;
        return rect;
    }

    function classify(el) {
        var tag = el.tagName ? el.tagName.toLowerCase() : '';
        if (tag === 'a') return 'link';
        if (tag === 'button') return 'button';
        if (tag === 'input') {
            var t = (el.type || '').toLowerCase();
            if (t === 'submit' || t === 'button' || t === 'reset') return 'button';
            return 'input';
        }
        if (tag === 'select' || tag === 'textarea') return 'input';
        if (el.getAttribute && el.getAttribute('role') === 'button') return 'button';
        return 'other';
    }

    var raw;
    try {
        raw = document.querySelectorAll(SELECTORS);
    } catch (err) {
        console.log(SENTINEL + JSON.stringify({ kind: 'error', message: 'querySelectorAll failed: ' + String(err) }));
        return;
    }

    var hints = [];
    var max = LABELS.length;
    for (var i = 0; i < raw.length && hints.length < max; i++) {
        var el = raw[i];
        var rect = isVisible(el);
        if (!rect) continue;
        hints.push({ el: el, rect: rect, kind: classify(el) });
    }

    // ---- assign labels and inject overlays -------------------------------
    var overlays = [];
    var report = [];
    for (var j = 0; j < hints.length; j++) {
        var h = hints[j];
        var label = LABELS[j];
        var id = j;
        h.el.setAttribute('data-buffr-hint-target-id', String(id));

        var div = document.createElement('div');
        div.className = CLASS;
        div.setAttribute('data-buffr-hint-id', String(id));
        div.dataset.buffrHintLabel = label;
        div.textContent = label;
        // Position in viewport coordinates (we use position:fixed).
        div.style.left = Math.max(0, Math.floor(h.rect.left)) + 'px';
        div.style.top = Math.max(0, Math.floor(h.rect.top)) + 'px';
        document.body.appendChild(div);
        overlays.push(div);

        report.push({
            id: id,
            label: label,
            kind: h.kind,
            x: Math.floor(h.rect.left),
            y: Math.floor(h.rect.top),
            w: Math.floor(h.rect.width),
            h: Math.floor(h.rect.height)
        });
    }

    // ---- host-callable API ----------------------------------------------
    window.__buffrHintFilter = function (typed) {
        for (var k = 0; k < overlays.length; k++) {
            var ov = overlays[k];
            var lab = ov.dataset.buffrHintLabel || '';
            if (!typed) {
                ov.classList.remove('buffr-hint-hidden');
                ov.textContent = lab;
            } else if (lab.indexOf(typed) === 0) {
                ov.classList.remove('buffr-hint-hidden');
                // Strike-through the typed prefix so the user sees how
                // far they've narrowed the label without losing the
                // remaining hint chars they still need to press.
                var prefix = document.createElement('span');
                prefix.className = 'buffr-hint-typed-prefix';
                prefix.textContent = lab.slice(0, typed.length);
                var rest = document.createTextNode(lab.slice(typed.length));
                ov.textContent = '';
                ov.appendChild(prefix);
                ov.appendChild(rest);
            } else {
                ov.classList.add('buffr-hint-hidden');
            }
        }
    };

    window.__buffrHintCommit = function (elementId) {
        var sel = '[data-buffr-hint-target-id="' + String(elementId) + '"]';
        var target = document.querySelector(sel);
        if (!target) return;
        try {
            if (typeof target.focus === 'function') target.focus();
            if (typeof target.click === 'function') target.click();
        } catch (e) {
            console.log(SENTINEL + JSON.stringify({ kind: 'error', message: 'click failed: ' + String(e) }));
        }
        if (window.__buffrHintCancel) window.__buffrHintCancel();
    };

    window.__buffrHintCancel = function () {
        for (var m = 0; m < overlays.length; m++) {
            var o = overlays[m];
            if (o && o.parentNode) o.parentNode.removeChild(o);
        }
        overlays.length = 0;
        var stragglers = document.querySelectorAll('[data-buffr-hint-target-id]');
        for (var n = 0; n < stragglers.length; n++) {
            stragglers[n].removeAttribute('data-buffr-hint-target-id');
        }
        window.__buffrHintFilter = null;
        window.__buffrHintCommit = null;
        window.__buffrHintCancel = null;
    };

    // ---- announce ready --------------------------------------------------
    console.log(SENTINEL + JSON.stringify({ kind: 'ready', hints: report, alphabet: ALPHABET }));
})();
