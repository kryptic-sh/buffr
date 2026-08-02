// Harness support, loaded by every e2e page.
//
// Reports the viewport rect of every element carrying an id, so the runner
// can aim a real cursor at it without hard-coding buffr's chrome height.
// Communication is plain `console.log`: buffr logs page console lines
// verbatim under the `buffr_core::console` target, so the runner reads them
// straight out of the browser's own log. Nothing here touches buffr APIs.
(function () {
    'use strict';

    function report() {
        var all = document.querySelectorAll('[id]');
        for (var i = 0; i < all.length; i++) {
            var el = all[i];
            var r = el.getBoundingClientRect();
            if (r.width <= 0 || r.height <= 0) { continue; }
            console.log('E2E-RECT ' + el.id + ' ' +
                Math.round(r.left) + ' ' + Math.round(r.top) + ' ' +
                Math.round(r.width) + ' ' + Math.round(r.height));
        }
        console.log('E2E-RECTS-DONE');
    }

    // Calibration: the runner clicks a known screen point and reads the
    // client coords back, which yields the viewport origin without the
    // runner knowing anything about the tab strip or input bar.
    document.addEventListener('click', function (ev) {
        console.log('E2E-CLICK ' + ev.clientX + ' ' + ev.clientY);
    }, true);

    // What actually ended up focused, for diagnosing a failure. Reported
    // from the deepest node the platform will admit to, so a shadow-root
    // input is distinguishable from its host.
    document.addEventListener('focusin', function (ev) {
        var t = ev.target;
        var deep = (ev.composedPath && ev.composedPath()[0]) || t;
        console.log('E2E-FOCUS host=' + (t && t.tagName) +
            ' deep=' + (deep && deep.tagName) +
            ' type=' + ((deep && deep.type) || '-'));
    }, true);

    // Focus trigger. A page either defines window.__e2eTrigger for a
    // sequence of its own (open a dialog, spawn a field, swap a node) or
    // marks one element [data-e2e-target] to be focused directly. This is
    // how the suite exercises focus without synthetic input: every path
    // below is one a real site actually uses to focus a field.
    function trigger() {
        try {
            if (typeof window.__e2eTrigger === 'function') {
                window.__e2eTrigger();
                return;
            }
            var t = document.querySelector('[data-e2e-target]');
            if (t && typeof t.focus === 'function') { t.focus(); }
        } catch (e) {
            console.log('E2E-TRIGGER-ERROR ' + e);
        }
    }
    window.addEventListener('load', function () { setTimeout(trigger, 400); });

    if (document.readyState === 'complete') { report(); }
    else { window.addEventListener('load', report); }
    // Re-report after late DOM work (dialogs, spawned fields).
    window.addEventListener('load', function () { setTimeout(report, 1200); });
})();
