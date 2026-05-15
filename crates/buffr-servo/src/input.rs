//! Input event translation: buffr-engine neutral types → `servo::InputEvent`.
//!
//! Servo re-exports input event types at the crate root via
//! `pub use embedder_traits::*`, so all types (including `WebViewPoint`,
//! `InputEvent`, `MouseButton`, etc.) are available as `servo::Foo`.
//!
//! The `ServoInputEvent` wrapper carries a debug description (for logging)
//! and an `Option<InputEvent>` for variants that translate cleanly.
//! Variants that can't be mapped (no known `Key`) are logged and skipped.

use buffr_engine::{KeyEventKind, MouseButton as NeutralButton, NeutralKeyEvent};
use keyboard_types;
use servo::{
    Code, CompositionEvent, CompositionState, ImeEvent, InputEvent, Key, KeyState, Location,
    Modifiers, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent,
    MouseLeftViewportEvent, MouseMoveEvent, NamedKey, WebViewPoint, WheelDelta, WheelEvent,
    WheelMode,
};

// Servo re-exports `keyboard_types::KeyboardEvent` under the servo namespace
// via `pub use keyboard_types::{..., NamedKey}` — the `KeyboardEvent` wrapper
// is `embedder_traits::input_events::KeyboardEvent`, exposed as servo::KeyboardEvent.
use servo::KeyboardEvent as ServoKeyboardEvent;

// webrender_api::units types are re-exported by servo:
// `pub use webrender_api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize, ...}`
use servo::DevicePoint;

// ── Public wrapper type ───────────────────────────────────────────────────────

/// Wraps a translated `servo::InputEvent` plus a log description.
///
/// If translation is not possible for a given input variant, `inner` is `None`
/// and the worker logs and skips the event.
#[derive(Debug, Clone)]
pub struct ServoInputEvent {
    /// Human-readable description for logging.
    pub description: String,
    /// The real Servo input event, if translation succeeded.
    pub(crate) inner: Option<InputEvent>,
}

impl ServoInputEvent {
    fn new(description: impl Into<String>, inner: InputEvent) -> Self {
        Self {
            description: description.into(),
            inner: Some(inner),
        }
    }

    fn untranslatable(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            inner: None,
        }
    }
}

// ── Coordinate helpers ────────────────────────────────────────────────────────

fn to_webview_point(x: i32, y: i32) -> WebViewPoint {
    // WebViewPoint::Device wraps a DevicePoint (euclid Point2D<f32, DevicePixel>)
    WebViewPoint::Device(DevicePoint::new(x as f32, y as f32))
}

// ── Windows VK → keyboard_types::Key (v0.8.x shape) ──────────────────────────

fn vk_to_key(vk: i32, character: u16) -> Option<Key> {
    // Printable character available → Key::Character
    if character != 0
        && let Some(c) = char::from_u32(character as u32)
    {
        return Some(Key::Character(c.to_string()));
    }
    // Named keys
    let named: NamedKey = match vk {
        0x08 => NamedKey::Backspace,
        0x09 => NamedKey::Tab,
        0x0D => NamedKey::Enter,
        0x1B => NamedKey::Escape,
        0x21 => NamedKey::PageUp,
        0x22 => NamedKey::PageDown,
        0x23 => NamedKey::End,
        0x24 => NamedKey::Home,
        0x25 => NamedKey::ArrowLeft,
        0x26 => NamedKey::ArrowUp,
        0x27 => NamedKey::ArrowRight,
        0x28 => NamedKey::ArrowDown,
        0x2E => NamedKey::Delete,
        0x10 => NamedKey::Shift,
        0x11 => NamedKey::Control,
        0x12 => NamedKey::Alt,
        0x5B | 0x5C => NamedKey::Meta,
        0x20 => return Some(Key::Character(" ".to_string())),
        0x70 => NamedKey::F1,
        0x71 => NamedKey::F2,
        0x72 => NamedKey::F3,
        0x73 => NamedKey::F4,
        0x74 => NamedKey::F5,
        0x75 => NamedKey::F6,
        0x76 => NamedKey::F7,
        0x77 => NamedKey::F8,
        0x78 => NamedKey::F9,
        0x79 => NamedKey::F10,
        0x7A => NamedKey::F11,
        0x7B => NamedKey::F12,
        _ => return None,
    };
    Some(Key::Named(named))
}

fn vk_to_code(vk: i32) -> Code {
    match vk {
        0x08 => Code::Backspace,
        0x09 => Code::Tab,
        0x0D => Code::Enter,
        0x1B => Code::Escape,
        0x20 => Code::Space,
        0x21 => Code::PageUp,
        0x22 => Code::PageDown,
        0x23 => Code::End,
        0x24 => Code::Home,
        0x25 => Code::ArrowLeft,
        0x26 => Code::ArrowUp,
        0x27 => Code::ArrowRight,
        0x28 => Code::ArrowDown,
        0x2E => Code::Delete,
        0x10 => Code::ShiftLeft,
        0x11 => Code::ControlLeft,
        0x12 => Code::AltLeft,
        0x5B => Code::MetaLeft,
        0x5C => Code::MetaRight,
        0x70 => Code::F1,
        0x71 => Code::F2,
        0x72 => Code::F3,
        0x73 => Code::F4,
        0x74 => Code::F5,
        0x75 => Code::F6,
        0x76 => Code::F7,
        0x77 => Code::F8,
        0x78 => Code::F9,
        0x79 => Code::F10,
        0x7A => Code::F11,
        0x7B => Code::F12,
        // Alpha A-Z
        v if (0x41..=0x5A).contains(&v) => match v {
            0x41 => Code::KeyA,
            0x42 => Code::KeyB,
            0x43 => Code::KeyC,
            0x44 => Code::KeyD,
            0x45 => Code::KeyE,
            0x46 => Code::KeyF,
            0x47 => Code::KeyG,
            0x48 => Code::KeyH,
            0x49 => Code::KeyI,
            0x4A => Code::KeyJ,
            0x4B => Code::KeyK,
            0x4C => Code::KeyL,
            0x4D => Code::KeyM,
            0x4E => Code::KeyN,
            0x4F => Code::KeyO,
            0x50 => Code::KeyP,
            0x51 => Code::KeyQ,
            0x52 => Code::KeyR,
            0x53 => Code::KeyS,
            0x54 => Code::KeyT,
            0x55 => Code::KeyU,
            0x56 => Code::KeyV,
            0x57 => Code::KeyW,
            0x58 => Code::KeyX,
            0x59 => Code::KeyY,
            0x5A => Code::KeyZ,
            _ => Code::Unidentified,
        },
        // Digits 0-9
        0x30 => Code::Digit0,
        0x31 => Code::Digit1,
        0x32 => Code::Digit2,
        0x33 => Code::Digit3,
        0x34 => Code::Digit4,
        0x35 => Code::Digit5,
        0x36 => Code::Digit6,
        0x37 => Code::Digit7,
        0x38 => Code::Digit8,
        0x39 => Code::Digit9,
        _ => Code::Unidentified,
    }
}

/// Convert CEF EVENTFLAG_* bitmask → `keyboard_types::Modifiers`.
fn cef_mods_to_modifiers(mods: u32) -> Modifiers {
    let mut m = Modifiers::empty();
    if mods & 0x02 != 0 {
        m |= Modifiers::SHIFT;
    }
    if mods & 0x04 != 0 {
        m |= Modifiers::CONTROL;
    }
    if mods & 0x08 != 0 {
        m |= Modifiers::ALT;
    }
    if mods & 0x40 != 0 {
        m |= Modifiers::META;
    }
    m
}

// ── Public translation helpers ────────────────────────────────────────────────

/// Translate a [`NeutralKeyEvent`] to a [`ServoInputEvent`].
pub fn neutral_key_to_servo(ev: &NeutralKeyEvent) -> ServoInputEvent {
    let state = match ev.kind {
        KeyEventKind::Up => KeyState::Up,
        KeyEventKind::RawDown | KeyEventKind::Char => KeyState::Down,
    };

    let Some(key) = vk_to_key(ev.windows_key_code, ev.character) else {
        let desc = format!("key:VK(0x{:02X})", ev.windows_key_code);
        tracing::debug!("servo input: no mapping for {desc}, skipping");
        return ServoInputEvent::untranslatable(desc);
    };

    let code = vk_to_code(ev.windows_key_code);
    let modifiers = cef_mods_to_modifiers(ev.modifiers);

    // `ServoKeyboardEvent::new` takes `keyboard_types::KeyboardEvent` (the raw
    // type from the keyboard-types crate, not the embedder_traits wrapper).
    let keyboard_event = keyboard_types::KeyboardEvent {
        state,
        key: key.clone(),
        code,
        location: Location::Standard,
        modifiers,
        repeat: false,
        is_composing: false,
    };

    let desc = format!("{state:?}:{key:?}");
    tracing::debug!("servo input: {desc}");
    ServoInputEvent::new(
        desc,
        InputEvent::Keyboard(ServoKeyboardEvent::new(keyboard_event)),
    )
}

/// Translate a neutral mouse-click to a [`ServoInputEvent`].
pub fn neutral_click_to_servo(
    x: i32,
    y: i32,
    button: NeutralButton,
    mouse_up: bool,
) -> ServoInputEvent {
    let btn = match button {
        NeutralButton::Left => ServoMouseButton::Left,
        NeutralButton::Middle => ServoMouseButton::Middle,
        NeutralButton::Right => ServoMouseButton::Right,
        NeutralButton::Other(n) => ServoMouseButton::Other(n as u16),
    };
    let action = if mouse_up {
        MouseButtonAction::Up
    } else {
        MouseButtonAction::Down
    };
    let point = to_webview_point(x, y);
    let desc = format!("{action:?}:{btn:?}@{x},{y}");
    tracing::debug!("servo input: {desc}");
    ServoInputEvent::new(
        desc,
        InputEvent::MouseButton(MouseButtonEvent::new(action, btn, point)),
    )
}

/// Translate a neutral mouse-move to a [`ServoInputEvent`].
pub fn neutral_move_to_servo(x: i32, y: i32) -> ServoInputEvent {
    let point = to_webview_point(x, y);
    ServoInputEvent::new(
        format!("mousemove@{x},{y}"),
        InputEvent::MouseMove(MouseMoveEvent::new(point)),
    )
}

/// Translate a neutral mouse-leave to a [`ServoInputEvent`].
pub fn neutral_leave_to_servo() -> ServoInputEvent {
    ServoInputEvent::new(
        "mouseleave",
        InputEvent::MouseLeftViewport(MouseLeftViewportEvent {
            focus_moving_to_another_iframe: false,
        }),
    )
}

/// Translate a preedit update to a [`ServoInputEvent`] via `ImeEvent::Composition`.
///
/// `servo-embedder-traits-0.1.0/input_events.rs:331`:
///   `ImeEvent::Composition(CompositionEvent)`
///
/// `keyboard_types::CompositionEvent { state: CompositionState::Update, data: text }`
/// carries the live preedit string.  Servo does not have a separate
/// preedit-cursor-range field on `CompositionEvent`; the cursor position is
/// managed by the engine internally.  `cursor` is accepted here for API symmetry
/// but is not forwarded to Servo (no field available).
pub fn ime_preedit_to_servo(text: &str, _cursor: Option<(usize, usize)>) -> ServoInputEvent {
    let composition = CompositionEvent {
        state: CompositionState::Update,
        data: text.to_owned(),
    };
    let desc = format!("ime:preedit({text:?})");
    tracing::debug!("servo input: {desc}");
    ServoInputEvent::new(desc, InputEvent::Ime(ImeEvent::Composition(composition)))
}

/// Translate a commit string to a [`ServoInputEvent`] via `ImeEvent::Composition`.
///
/// `keyboard_types::CompositionState::End` signals that the composition is
/// complete and the text should be committed.
pub fn ime_commit_to_servo(text: &str) -> ServoInputEvent {
    let composition = CompositionEvent {
        state: CompositionState::End,
        data: text.to_owned(),
    };
    let desc = format!("ime:commit({text:?})");
    tracing::debug!("servo input: {desc}");
    ServoInputEvent::new(desc, InputEvent::Ime(ImeEvent::Composition(composition)))
}

/// Cancel IME composition via `ImeEvent::Dismissed`.
///
/// `servo-embedder-traits-0.1.0/input_events.rs:333`: `ImeEvent::Dismissed`.
pub fn ime_cancel_to_servo() -> ServoInputEvent {
    tracing::debug!("servo input: ime:cancel");
    ServoInputEvent::new("ime:cancel", InputEvent::Ime(ImeEvent::Dismissed))
}

/// Translate a neutral scroll to a [`ServoInputEvent`].
pub fn neutral_scroll_to_servo(x: i32, y: i32, delta_x: i32, delta_y: i32) -> ServoInputEvent {
    let point = to_webview_point(x, y);
    let delta = WheelDelta {
        x: delta_x as f64,
        y: delta_y as f64,
        z: 0.0,
        mode: WheelMode::DeltaPixel,
    };
    let desc = format!("wheel@{x},{y} delta={delta_x},{delta_y}");
    tracing::debug!("servo input: {desc}");
    ServoInputEvent::new(desc, InputEvent::Wheel(WheelEvent::new(delta, point)))
}
