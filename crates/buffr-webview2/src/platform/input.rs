//! Neutral input event translation for the WebView2 backend.
//!
//! # Phase B status
//!
//! WebView2 composition-hosted input synthesis uses two COM APIs:
//!
//! - **Mouse** — `ICoreWebView2CompositionController::SendMouseInput` with a
//!   `COREWEBVIEW2_MOUSE_EVENT_KIND` discriminant and a `POINT` coordinate.
//! - **Keyboard** — `ICoreWebView2CompositionController::SendKeyboardInput` with
//!   a `COREWEBVIEW2_PHYSICAL_KEY_STATUS` / Win32 `KEYBDINPUT` pair.
//!
//! Both APIs accept Windows Virtual-Key codes directly, which is why
//! `NeutralKeyEvent::windows_key_code` carries VK_* values — zero translation
//! needed for keyboard events.
//!
//! Phase B: build the translated structs and log them. Actual dispatch to the
//! `ICoreWebView2CompositionController` requires the STA thread (the controller
//! is apartment-threaded). Phase C will wire up the dispatch via the worker
//! command queue.
//!
//! # TODO markers
//!
//! - TODO(input-key): dispatch `Wv2KeyEvent` to the STA thread via
//!   `Command::SendKeyboard` and call
//!   `ICoreWebView2CompositionController::SendKeyboardInput`.
//! - TODO(input-mouse): dispatch `Wv2MouseEvent` to the STA thread via
//!   `Command::SendMouse` and call
//!   `ICoreWebView2CompositionController::SendMouseInput`.

use buffr_engine::{KeyEventKind, MouseButton, NeutralKeyEvent};

// ── Translated types ──────────────────────────────────────────────────────────

/// Translated keyboard event ready for `SendKeyboardInput`.
///
/// `vk` is the Windows Virtual-Key code (VK_*).
/// `flags` maps to `KEYBDINPUT.dwFlags` (KEYEVENTF_KEYUP for key-up, else 0).
pub(crate) struct Wv2KeyEvent {
    /// Windows Virtual-Key code. Phase C: passes to SendKeyboardInput.
    #[allow(dead_code)]
    pub vk: i32,
    /// Hardware scan code. Phase C: passes to SendKeyboardInput as uScanCode.
    #[allow(dead_code)]
    pub scan_code: u16,
    /// KEYBDINPUT.dwFlags (KEYEVENTF_KEYUP on release, else 0).
    #[allow(dead_code)]
    pub flags: u32,
    /// Human-readable description used in debug logging.
    pub description: String,
}

/// Translated mouse event ready for `SendMouseInput`.
pub(crate) struct Wv2MouseEvent {
    pub x: i32,
    pub y: i32,
    /// Mouse event kind. Phase C: maps to COREWEBVIEW2_MOUSE_EVENT_KIND.
    #[allow(dead_code)]
    pub kind: Wv2MouseKind,
}

#[allow(dead_code)]
pub(crate) enum Wv2MouseKind {
    /// COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE
    Move,
    /// COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT/RIGHT/MIDDLE_BUTTON_DOWN/UP
    ButtonDown(Wv2MouseButton),
    ButtonUp(Wv2MouseButton),
    /// COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE
    Leave,
    /// COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL
    Wheel {
        delta: i32,
    },
    /// Horizontal wheel — COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL
    HWheel {
        delta: i32,
    },
}

#[allow(dead_code)]
pub(crate) enum Wv2MouseButton {
    Left,
    Middle,
    Right,
}

// KEYEVENTF_KEYUP — set in dwFlags to signal key-release.
const KEYEVENTF_KEYUP: u32 = 0x0002;

// ── Neutral wrapper sent over the worker command channel ──────────────────────

/// A translated WebView2 input event, ready to be routed to the STA worker.
///
/// The worker receives this on the STA thread — the correct COM apartment for
/// `ICoreWebView2CompositionController` calls.
///
/// Phase B (blocked by #106): logs at `debug` level.
/// TODO(#106): dispatch via `ICoreWebView2CompositionController::SendMouseInput`
/// / `SendKeyboardInput` once COM init lands.
pub(crate) enum WebView2InputEvent {
    Key(Wv2KeyEvent),
    Mouse(Wv2MouseEvent),
}

// ── Translators ───────────────────────────────────────────────────────────────

/// Translate a `NeutralKeyEvent` to a `Wv2KeyEvent`.
///
/// `NeutralKeyEvent::windows_key_code` is already a VK_* code so the
/// translation is nearly 1:1. `native_key_code` carries the hardware scan
/// code on Windows (DMI/HID scan) — used as the scan code for extended keys.
pub(crate) fn neutral_key_to_wv2(event: &NeutralKeyEvent) -> WebView2InputEvent {
    let flags = match event.kind {
        KeyEventKind::Up => KEYEVENTF_KEYUP,
        _ => 0,
    };
    WebView2InputEvent::Key(Wv2KeyEvent {
        vk: event.windows_key_code,
        scan_code: event.native_key_code as u16,
        flags,
        description: format!(
            "key vk={} char={} kind={:?}",
            event.windows_key_code, event.character, event.kind
        ),
    })
}

/// Translate a mouse-move into a `WebView2InputEvent::Mouse`.
pub(crate) fn neutral_move_to_wv2(x: i32, y: i32) -> WebView2InputEvent {
    WebView2InputEvent::Mouse(Wv2MouseEvent {
        x,
        y,
        kind: Wv2MouseKind::Move,
    })
}

/// Translate a mouse-click into a `WebView2InputEvent::Mouse`.
pub(crate) fn neutral_click_to_wv2(
    x: i32,
    y: i32,
    button: MouseButton,
    mouse_up: bool,
) -> WebView2InputEvent {
    let btn = match button {
        MouseButton::Left => Wv2MouseButton::Left,
        MouseButton::Middle => Wv2MouseButton::Middle,
        MouseButton::Right | MouseButton::Other(_) => Wv2MouseButton::Right,
    };
    WebView2InputEvent::Mouse(Wv2MouseEvent {
        x,
        y,
        kind: if mouse_up {
            Wv2MouseKind::ButtonUp(btn)
        } else {
            Wv2MouseKind::ButtonDown(btn)
        },
    })
}

/// Translate a mouse-leave into a `WebView2InputEvent::Mouse`.
pub(crate) fn neutral_leave_to_wv2() -> WebView2InputEvent {
    WebView2InputEvent::Mouse(Wv2MouseEvent {
        x: 0,
        y: 0,
        kind: Wv2MouseKind::Leave,
    })
}

/// Translate a mouse-wheel event into a `WebView2InputEvent::Mouse`.
///
/// Windows `WHEEL_DELTA` is 120 per notch. `delta_y` from `NeutralKeyEvent`
/// is already in "lines" (positive = up). We multiply by 120 to get the
/// Win32 wheel-delta unit.
pub(crate) fn neutral_scroll_to_wv2(
    x: i32,
    y: i32,
    delta_x: i32,
    delta_y: i32,
) -> WebView2InputEvent {
    // Primary scroll axis: use vertical if non-zero, else horizontal.
    if delta_y != 0 {
        WebView2InputEvent::Mouse(Wv2MouseEvent {
            x,
            y,
            kind: Wv2MouseKind::Wheel {
                delta: delta_y * 120,
            },
        })
    } else {
        WebView2InputEvent::Mouse(Wv2MouseEvent {
            x,
            y,
            kind: Wv2MouseKind::HWheel {
                delta: delta_x * 120,
            },
        })
    }
}
