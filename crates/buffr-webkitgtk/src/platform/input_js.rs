//! JS event injection snippets for headless WebKitGTK input dispatch.
//!
//! # Safety / injection note
//!
//! All values injected into JS template strings are integers (`i32`, `u16`,
//! `f64`, button index). There is no user-supplied string data in any snippet,
//! so no JS escaping is required. If future callers add string fields (e.g. a
//! URL typed by the user) those **must** be escaped before interpolation.
//!
//! # Limitations
//!
//! DOM event dispatch via `dispatchEvent()` fires JS handlers and follows
//! bubbling / capture rules but does **not** trigger the browser's native
//! default actions (e.g. Tab focus traversal, form submit on Enter). This is
//! the standard trade-off for headless automation and covers the 80 % case.

use super::input::{GtkKeyEvent, GtkMouseEvent, GtkWheelEvent};

// ── Key map ───────────────────────────────────────────────────────────────────

/// Mapping from a Windows VK code to the JS `KeyboardEvent` fields.
struct JsKey {
    /// JS `key` field (printable string or named key, e.g. `"a"`, `"Enter"`).
    key: &'static str,
    /// JS `code` field (physical key identity, e.g. `"KeyA"`, `"Enter"`).
    code: &'static str,
    /// JS `keyCode` / `which` integer.
    key_code: u32,
}

/// Map a Windows VK code to the JS key fields, if known.
///
/// Returns `None` for unrecognised codes. Callers must skip the JS dispatch and
/// emit a `tracing::debug!` instead so that malformed snippets are never
/// injected.
///
/// Coverage: printable ASCII 32–90 (space, 0-9, A-Z), common function keys
/// (F1-F12), navigation keys (arrows, Home, End, PgUp, PgDn), editing keys
/// (Backspace, Tab, Enter, Escape, Delete, Insert).
pub(crate) fn vk_to_js_key(vk: i32) -> Option<(&'static str, &'static str, u32)> {
    // Printable ASCII range handled generically in the table below.
    let entry: Option<JsKey> = match vk {
        // ── Control / editing ────────────────────────────────────────────────
        8 => Some(JsKey {
            key: "Backspace",
            code: "Backspace",
            key_code: 8,
        }),
        9 => Some(JsKey {
            key: "Tab",
            code: "Tab",
            key_code: 9,
        }),
        13 => Some(JsKey {
            key: "Enter",
            code: "Enter",
            key_code: 13,
        }),
        16 => Some(JsKey {
            key: "Shift",
            code: "ShiftLeft",
            key_code: 16,
        }),
        17 => Some(JsKey {
            key: "Control",
            code: "ControlLeft",
            key_code: 17,
        }),
        18 => Some(JsKey {
            key: "Alt",
            code: "AltLeft",
            key_code: 18,
        }),
        19 => Some(JsKey {
            key: "Pause",
            code: "Pause",
            key_code: 19,
        }),
        20 => Some(JsKey {
            key: "CapsLock",
            code: "CapsLock",
            key_code: 20,
        }),
        27 => Some(JsKey {
            key: "Escape",
            code: "Escape",
            key_code: 27,
        }),
        32 => Some(JsKey {
            key: " ",
            code: "Space",
            key_code: 32,
        }),
        33 => Some(JsKey {
            key: "PageUp",
            code: "PageUp",
            key_code: 33,
        }),
        34 => Some(JsKey {
            key: "PageDown",
            code: "PageDown",
            key_code: 34,
        }),
        35 => Some(JsKey {
            key: "End",
            code: "End",
            key_code: 35,
        }),
        36 => Some(JsKey {
            key: "Home",
            code: "Home",
            key_code: 36,
        }),
        37 => Some(JsKey {
            key: "ArrowLeft",
            code: "ArrowLeft",
            key_code: 37,
        }),
        38 => Some(JsKey {
            key: "ArrowUp",
            code: "ArrowUp",
            key_code: 38,
        }),
        39 => Some(JsKey {
            key: "ArrowRight",
            code: "ArrowRight",
            key_code: 39,
        }),
        40 => Some(JsKey {
            key: "ArrowDown",
            code: "ArrowDown",
            key_code: 40,
        }),
        45 => Some(JsKey {
            key: "Insert",
            code: "Insert",
            key_code: 45,
        }),
        46 => Some(JsKey {
            key: "Delete",
            code: "Delete",
            key_code: 46,
        }),
        // ── Digits 0-9 (VK 48-57) ───────────────────────────────────────────
        48 => Some(JsKey {
            key: "0",
            code: "Digit0",
            key_code: 48,
        }),
        49 => Some(JsKey {
            key: "1",
            code: "Digit1",
            key_code: 49,
        }),
        50 => Some(JsKey {
            key: "2",
            code: "Digit2",
            key_code: 50,
        }),
        51 => Some(JsKey {
            key: "3",
            code: "Digit3",
            key_code: 51,
        }),
        52 => Some(JsKey {
            key: "4",
            code: "Digit4",
            key_code: 52,
        }),
        53 => Some(JsKey {
            key: "5",
            code: "Digit5",
            key_code: 53,
        }),
        54 => Some(JsKey {
            key: "6",
            code: "Digit6",
            key_code: 54,
        }),
        55 => Some(JsKey {
            key: "7",
            code: "Digit7",
            key_code: 55,
        }),
        56 => Some(JsKey {
            key: "8",
            code: "Digit8",
            key_code: 56,
        }),
        57 => Some(JsKey {
            key: "9",
            code: "Digit9",
            key_code: 57,
        }),
        // ── Letters A-Z (VK 65-90 → lowercase key, KeyA-KeyZ code) ──────────
        65 => Some(JsKey {
            key: "a",
            code: "KeyA",
            key_code: 65,
        }),
        66 => Some(JsKey {
            key: "b",
            code: "KeyB",
            key_code: 66,
        }),
        67 => Some(JsKey {
            key: "c",
            code: "KeyC",
            key_code: 67,
        }),
        68 => Some(JsKey {
            key: "d",
            code: "KeyD",
            key_code: 68,
        }),
        69 => Some(JsKey {
            key: "e",
            code: "KeyE",
            key_code: 69,
        }),
        70 => Some(JsKey {
            key: "f",
            code: "KeyF",
            key_code: 70,
        }),
        71 => Some(JsKey {
            key: "g",
            code: "KeyG",
            key_code: 71,
        }),
        72 => Some(JsKey {
            key: "h",
            code: "KeyH",
            key_code: 72,
        }),
        73 => Some(JsKey {
            key: "i",
            code: "KeyI",
            key_code: 73,
        }),
        74 => Some(JsKey {
            key: "j",
            code: "KeyJ",
            key_code: 74,
        }),
        75 => Some(JsKey {
            key: "k",
            code: "KeyK",
            key_code: 75,
        }),
        76 => Some(JsKey {
            key: "l",
            code: "KeyL",
            key_code: 76,
        }),
        77 => Some(JsKey {
            key: "m",
            code: "KeyM",
            key_code: 77,
        }),
        78 => Some(JsKey {
            key: "n",
            code: "KeyN",
            key_code: 78,
        }),
        79 => Some(JsKey {
            key: "o",
            code: "KeyO",
            key_code: 79,
        }),
        80 => Some(JsKey {
            key: "p",
            code: "KeyP",
            key_code: 80,
        }),
        81 => Some(JsKey {
            key: "q",
            code: "KeyQ",
            key_code: 81,
        }),
        82 => Some(JsKey {
            key: "r",
            code: "KeyR",
            key_code: 82,
        }),
        83 => Some(JsKey {
            key: "s",
            code: "KeyS",
            key_code: 83,
        }),
        84 => Some(JsKey {
            key: "t",
            code: "KeyT",
            key_code: 84,
        }),
        85 => Some(JsKey {
            key: "u",
            code: "KeyU",
            key_code: 85,
        }),
        86 => Some(JsKey {
            key: "v",
            code: "KeyV",
            key_code: 86,
        }),
        87 => Some(JsKey {
            key: "w",
            code: "KeyW",
            key_code: 87,
        }),
        88 => Some(JsKey {
            key: "x",
            code: "KeyX",
            key_code: 88,
        }),
        89 => Some(JsKey {
            key: "y",
            code: "KeyY",
            key_code: 89,
        }),
        90 => Some(JsKey {
            key: "z",
            code: "KeyZ",
            key_code: 90,
        }),
        // ── Function keys F1-F12 (VK 112-123) ───────────────────────────────
        112 => Some(JsKey {
            key: "F1",
            code: "F1",
            key_code: 112,
        }),
        113 => Some(JsKey {
            key: "F2",
            code: "F2",
            key_code: 113,
        }),
        114 => Some(JsKey {
            key: "F3",
            code: "F3",
            key_code: 114,
        }),
        115 => Some(JsKey {
            key: "F4",
            code: "F4",
            key_code: 115,
        }),
        116 => Some(JsKey {
            key: "F5",
            code: "F5",
            key_code: 116,
        }),
        117 => Some(JsKey {
            key: "F6",
            code: "F6",
            key_code: 117,
        }),
        118 => Some(JsKey {
            key: "F7",
            code: "F7",
            key_code: 118,
        }),
        119 => Some(JsKey {
            key: "F8",
            code: "F8",
            key_code: 119,
        }),
        120 => Some(JsKey {
            key: "F9",
            code: "F9",
            key_code: 120,
        }),
        121 => Some(JsKey {
            key: "F10",
            code: "F10",
            key_code: 121,
        }),
        122 => Some(JsKey {
            key: "F11",
            code: "F11",
            key_code: 122,
        }),
        123 => Some(JsKey {
            key: "F12",
            code: "F12",
            key_code: 123,
        }),
        _ => None,
    };
    entry.map(|k| (k.key, k.code, k.key_code))
}

// ── JS snippet builders ───────────────────────────────────────────────────────

/// Build a JS snippet that dispatches a `KeyboardEvent` on the focused element.
///
/// Returns `None` when the VK code is not in the known map — callers log at
/// debug level and skip the dispatch.
///
/// `event_type` is one of `"keydown"`, `"keypress"`, `"keyup"`.
///
/// All interpolated values are integers or static strings; no escaping needed.
pub(crate) fn key_js(event: &GtkKeyEvent, event_type: &str) -> Option<String> {
    let (key_str, code_str, key_code) = vk_to_js_key(event.windows_key_code)?;
    Some(format!(
        "(document.activeElement || document.body).dispatchEvent(\
            new KeyboardEvent({type_q}, {{\
                key: {key_q}, \
                code: {code_q}, \
                keyCode: {key_code}, \
                which: {key_code}, \
                bubbles: true, \
                cancelable: true\
            }})\
        );",
        type_q = js_str(event_type),
        key_q = js_str(key_str),
        code_q = js_str(code_str),
        key_code = key_code,
    ))
}

/// Build a JS snippet that dispatches a `mousemove` event at `(x, y)`.
pub(crate) fn mouse_move_js(event: &GtkMouseEvent) -> String {
    let x = event.x as i64;
    let y = event.y as i64;
    format!(
        "(function(){{ \
            var el = document.elementFromPoint({x}, {y}) || document.body; \
            el.dispatchEvent(new MouseEvent('mousemove', \
                {{clientX: {x}, clientY: {y}, bubbles: true, cancelable: true}})); \
        }})();",
        x = x,
        y = y,
    )
}

/// Build a JS snippet that dispatches `mousedown` + `mouseup` + `click` at
/// `(x, y)` for the given button index (0 = left, 1 = middle, 2 = right).
pub(crate) fn mouse_click_js(event: &GtkMouseEvent, button: u32, mouse_up: bool) -> String {
    let x = event.x as i64;
    let y = event.y as i64;
    if mouse_up {
        format!(
            "(function(){{ \
                var el = document.elementFromPoint({x}, {y}) || document.body; \
                el.dispatchEvent(new MouseEvent('mouseup', \
                    {{clientX: {x}, clientY: {y}, button: {button}, bubbles: true, cancelable: true}})); \
                if ({button} === 0) {{ \
                    el.dispatchEvent(new MouseEvent('click', \
                        {{clientX: {x}, clientY: {y}, button: 0, bubbles: true, cancelable: true}})); \
                }} \
            }})();",
            x = x,
            y = y,
            button = button,
        )
    } else {
        format!(
            "(function(){{ \
                var el = document.elementFromPoint({x}, {y}) || document.body; \
                el.dispatchEvent(new MouseEvent('mousedown', \
                    {{clientX: {x}, clientY: {y}, button: {button}, bubbles: true, cancelable: true}})); \
            }})();",
            x = x,
            y = y,
            button = button,
        )
    }
}

/// Build a JS snippet that fires `mouseleave` on the currently active element.
pub(crate) fn mouse_leave_js() -> String {
    "(document.activeElement || document.body).dispatchEvent(\
        new MouseEvent('mouseleave', {bubbles: false, cancelable: false})\
    );"
    .to_owned()
}

/// Build a JS snippet that dispatches a `wheel` event at `(x, y)` with the
/// given delta values (pixels, DOM_DELTA_PIXEL mode = 0).
pub(crate) fn wheel_js(event: &GtkWheelEvent) -> String {
    let x = event.x as i64;
    let y = event.y as i64;
    let dx = event.delta_x as i64;
    let dy = event.delta_y as i64;
    format!(
        "(function(){{ \
            var el = document.elementFromPoint({x}, {y}) || document.body; \
            el.dispatchEvent(new WheelEvent('wheel', {{\
                clientX: {x}, clientY: {y}, \
                deltaX: {dx}, deltaY: {dy}, \
                deltaMode: 0, \
                bubbles: true, cancelable: true\
            }})); \
        }})();",
        x = x,
        y = y,
        dx = dx,
        dy = dy,
    )
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Wrap a string in JS single-quoted notation.
///
/// All callers pass string literals that contain no single-quotes or
/// backslashes (key names like `"Enter"`, `"a"`, event types like `"keydown"`).
/// No escaping is needed; if future callers pass user-supplied strings they
/// **must** escape single-quotes and backslashes first.
fn js_str(s: &str) -> String {
    format!("'{s}'")
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::input::{GtkKeyEvent, GtkMouseEvent, GtkMouseKind, GtkWheelEvent};

    #[test]
    fn vk_a_maps_correctly() {
        let (key, code, kc) = vk_to_js_key(65).unwrap();
        assert_eq!(key, "a");
        assert_eq!(code, "KeyA");
        assert_eq!(kc, 65);
    }

    #[test]
    fn vk_enter_maps_correctly() {
        let (key, code, kc) = vk_to_js_key(13).unwrap();
        assert_eq!(key, "Enter");
        assert_eq!(code, "Enter");
        assert_eq!(kc, 13);
    }

    #[test]
    fn vk_unknown_returns_none() {
        assert!(vk_to_js_key(0).is_none());
        assert!(vk_to_js_key(200).is_none());
    }

    #[test]
    fn key_js_contains_event_type_and_keycode() {
        let ev = GtkKeyEvent {
            windows_key_code: 65,
            description: "a".into(),
        };
        let js = key_js(&ev, "keydown").unwrap();
        assert!(js.contains("'keydown'"), "missing event type: {js}");
        assert!(js.contains("keyCode: 65"), "missing keyCode: {js}");
        assert!(js.contains("'KeyA'"), "missing code: {js}");
    }

    #[test]
    fn key_js_unknown_vk_returns_none() {
        let ev = GtkKeyEvent {
            windows_key_code: 0,
            description: "unknown".into(),
        };
        assert!(key_js(&ev, "keydown").is_none());
    }

    #[test]
    fn mouse_move_js_contains_coords() {
        let ev = GtkMouseEvent {
            x: 42.0,
            y: 100.0,
            kind: GtkMouseKind::Move,
        };
        let js = mouse_move_js(&ev);
        assert!(js.contains("42"), "missing x: {js}");
        assert!(js.contains("100"), "missing y: {js}");
        assert!(js.contains("mousemove"), "missing event type: {js}");
    }

    #[test]
    fn mouse_click_js_mousedown_contains_button() {
        let ev = GtkMouseEvent {
            x: 10.0,
            y: 20.0,
            kind: GtkMouseKind::ButtonPress(crate::platform::input::GtkMouseButton::Left),
        };
        let js = mouse_click_js(&ev, 0, false);
        assert!(js.contains("mousedown"), "missing mousedown: {js}");
        assert!(js.contains("button: 0"), "missing button 0: {js}");
    }

    #[test]
    fn mouse_click_js_mouseup_fires_click_for_left() {
        let ev = GtkMouseEvent {
            x: 10.0,
            y: 20.0,
            kind: GtkMouseKind::ButtonRelease(crate::platform::input::GtkMouseButton::Left),
        };
        let js = mouse_click_js(&ev, 0, true);
        assert!(js.contains("mouseup"), "missing mouseup: {js}");
        assert!(js.contains("'click'"), "missing click: {js}");
    }

    #[test]
    fn mouse_leave_js_fires_mouseleave() {
        let js = mouse_leave_js();
        assert!(js.contains("mouseleave"), "missing mouseleave: {js}");
    }

    #[test]
    fn wheel_js_contains_deltas() {
        let ev = GtkWheelEvent {
            x: 5.0,
            y: 10.0,
            delta_x: 0.0,
            delta_y: -120.0,
        };
        let js = wheel_js(&ev);
        assert!(js.contains("wheel"), "missing 'wheel': {js}");
        assert!(js.contains("-120"), "missing deltaY: {js}");
    }
}
