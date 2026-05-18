// Exit insert/edit mode: dispatch a synthetic Escape keydown+keyup
// on the active element (so pages like Google close their autocomplete
// dropdowns) then blur the element.
//
// Triggered by both the engine-resolved `ExitInsertMode` action and
// the apps-layer Esc fast path inside `edit_mode_handle_key`.
(function () {
    var el = document.activeElement;
    if (!el) return;
    var k = {
        key: 'Escape',
        code: 'Escape',
        keyCode: 27,
        which: 27,
        bubbles: true,
        cancelable: true,
    };
    el.dispatchEvent(new KeyboardEvent('keydown', k));
    el.dispatchEvent(new KeyboardEvent('keyup', k));
    el.blur();
})();
