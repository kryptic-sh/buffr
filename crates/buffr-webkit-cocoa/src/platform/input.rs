//! Input translation: [`NeutralKeyEvent`] / [`MouseButton`] → Cocoa NSEvent.
//!
//! WKWebView processes keyboard and mouse events as AppKit `NSEvent` objects.
//! This module translates buffr's engine-agnostic input types into the
//! Cocoa event representation and dispatches them to the WKWebView via its
//! inherited `NSResponder` interface.
//!
//! # Threading
//!
//! All dispatch calls in this module MUST happen on the **main thread** (the
//! AppKit run loop thread). The caller — `runtime.rs` — ensures this by
//! running inside a `dispatch_async(dispatch_get_main_queue(), …)` closure.
//!
//! # API verification notes (grepped from registry crate source)
//!
//! ## NSEventType variants (NSEvent.rs lines 19-91)
//!
//! Confirmed constant names (these are `NSEventType::Foo`, not `NSEventTypeFoo`):
//!   - `NSEventType::KeyDown`        = Self(10)
//!   - `NSEventType::KeyUp`          = Self(11)
//!   - `NSEventType::LeftMouseDown`  = Self(1)
//!   - `NSEventType::LeftMouseUp`    = Self(2)
//!   - `NSEventType::RightMouseDown` = Self(3)
//!   - `NSEventType::RightMouseUp`   = Self(4)
//!   - `NSEventType::OtherMouseDown` = Self(25)
//!   - `NSEventType::OtherMouseUp`   = Self(26)
//!   - `NSEventType::MouseMoved`     = Self(5)
//!   - `NSEventType::ScrollWheel`    = Self(22)
//!
//! ## NSEventModifierFlags (NSEvent.rs lines 385-407)
//!
//! Uses `bitflags!` impl. Correct constant names are the short forms:
//!   `NSEventModifierFlags::Shift`   (NOT NSEventModifierFlagShift)
//!   `NSEventModifierFlags::Control`
//!   `NSEventModifierFlags::Option`  (for Alt/Option)
//!   `NSEventModifierFlags::Command`
//!
//! ## NSEvent factory methods (NSEvent.rs lines 1076-1138)
//!
//! All require `feature = "NSGraphicsContext"`.
//!
//! `keyEventWithType:location:modifierFlags:timestamp:windowNumber:context:
//!  characters:charactersIgnoringModifiers:isARepeat:keyCode:`
//! Rust name: `keyEventWithType_location_modifierFlags_timestamp_windowNumber_
//!             context_characters_charactersIgnoringModifiers_isARepeat_keyCode`
//! Arguments (confirmed):
//!   r#type: NSEventType
//!   location: NSPoint
//!   flags: NSEventModifierFlags
//!   time: NSTimeInterval  (f64 alias)
//!   w_num: NSInteger
//!   unused_pass_nil: Option<&NSGraphicsContext>  ← MUST be None
//!   keys: &NSString   ← NOT Option<&NSString>
//!   ukeys: &NSString  ← NOT Option<&NSString>
//!   flag: bool        (isARepeat)
//!   code: c_ushort    (unsigned short)
//!
//! `mouseEventWithType:location:modifierFlags:timestamp:windowNumber:context:
//!  eventNumber:clickCount:pressure:`
//! Rust name: `mouseEventWithType_location_modifierFlags_timestamp_windowNumber_
//!             context_eventNumber_clickCount_pressure`
//! Arguments (confirmed):
//!   r#type: NSEventType
//!   location: NSPoint
//!   flags: NSEventModifierFlags
//!   time: NSTimeInterval
//!   w_num: NSInteger
//!   unused_pass_nil: Option<&NSGraphicsContext>  ← MUST be None
//!   e_num: NSInteger  (eventNumber)
//!   c_num: NSInteger  (clickCount)
//!   pressure: c_float (NOT f64)
//!
//! ## NSResponder methods (NSResponder.rs lines 80-213)
//!
//! All confirmed present and take `&NSEvent`:
//!   mouseDown, mouseUp, rightMouseDown, rightMouseUp,
//!   otherMouseDown, otherMouseUp, mouseMoved, scrollWheel,
//!   keyDown, keyUp
//!
//! WKWebView inherits NSView which inherits NSResponder.
//! Calling these on `web_view as &NSView` routes through the responder chain.
//!
//! ## Scroll wheel limitation
//!
//! `mouseEventWithType` with `NSEventType::ScrollWheel` creates a zero-delta
//! scroll event — the `deltaX`/`deltaY` properties of NSEvent cannot be set via
//! this constructor. A CGEvent-based approach would be needed for real deltas.
//! This is documented at each call site.
//!
//! # TODO: CGEvent scroll wheel
//!
//! The `mouseEventWithType:…` constructor cannot inject scroll deltas. For
//! real scroll support, create a `CGScrollWheelEvent` via `CGEventCreateScrollWheelEvent`
//! (in `objc2-core-graphics`) and wrap it with `NSEvent(with:CGEvent:)`.
//! That API is semi-private (not in the public headers) and not bound in
//! objc2-app-kit 0.3.2. Deferring to a future phase when CG bindings mature.
//!
//! # TODO: trackpad gestures
//!
//! `NSEventType::Magnify`, `Swipe`, `Rotate`, `BeginGesture`, `EndGesture`
//! require their own `NSEvent` construction path (`otherEventWithType:…`).
//! Not wired in Phase B; will be addressed when buffr-engine exposes gesture
//! input types.

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use core::ffi::c_float;

    use buffr_engine::{KeyEventKind, MouseButton, NeutralKeyEvent};
    use objc2::rc::Retained;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSView};
    use objc2_foundation::NSString;
    use objc2_web_kit::WKWebView;

    // ── Key events ────────────────────────────────────────────────────────────

    /// Translate and dispatch a `NeutralKeyEvent` to `web_view`.
    ///
    /// Constructs an `NSEvent` via the factory:
    ///   `keyEventWithType:location:modifierFlags:timestamp:windowNumber:
    ///    context:characters:charactersIgnoringModifiers:isARepeat:keyCode:`
    ///
    /// Confirmed: NSEvent.rs:1092-1105.
    ///
    /// # Coordinate system
    ///
    /// `location` is in window coordinates (bottom-left origin). For off-screen
    /// key events the location is irrelevant; we pass `(0.0, 0.0)`.
    ///
    /// # windowNumber
    ///
    /// Passing `0` is correct for synthesised off-screen events. WKWebView does
    /// not validate the window number for key processing.
    ///
    /// # NeutralKeyEvent fields
    ///
    /// - `event.modifiers`: CEF EVENTFLAG_* bitmask — mapped via `cef_mods_to_ns_mods`.
    /// - `event.character`: UTF-16 code unit for `characters` NSString.
    /// - `event.unmodified_character`: UTF-16 code unit for `charactersIgnoringModifiers`.
    /// - `event.native_key_code`: macOS Carbon HID key code (u32, cast to c_ushort).
    ///
    /// # TODO: native_key_code range check
    ///
    /// `NeutralKeyEvent::native_key_code` is declared as `u32` in buffr-engine.
    /// macOS key codes fit in u16 (max 127 for standard keys). The cast to
    /// `c_ushort` (u16) silently truncates values > 65535, which won't happen
    /// in practice but is worth a debug_assert on first macOS CI run.
    pub(crate) fn dispatch_key_event(web_view: &WKWebView, event: &NeutralKeyEvent) {
        // NSEventType::KeyDown = Self(10), NSEventType::KeyUp = Self(11).
        // Confirmed: NSEvent.rs:39-41.
        let ns_type = match event.kind {
            KeyEventKind::RawDown | KeyEventKind::Char => NSEventType::KeyDown,
            KeyEventKind::Up => NSEventType::KeyUp,
        };

        // Map CEF EVENTFLAG_* bits to NSEventModifierFlags.
        // Confirmed bit positions and constant names: see cef_mods_to_ns_mods().
        let mods = cef_mods_to_ns_mods(event.modifiers);

        // Build NSString for `characters` and `charactersIgnoringModifiers`.
        // `keyEventWithType:…` requires `&NSString`, not `Option<&NSString>`.
        // Confirmed: NSEvent.rs:1101-1102.
        // For non-printable keys (e.g. arrows) the character is typically a
        // Unicode private-use area code point; NSString wraps it fine.
        let chars = char_to_nsstring(event.character);
        let unchars = char_to_nsstring(event.unmodified_character);

        // native_key_code cast: u32 → c_ushort (u16).
        // Carbon HID key codes are 7-bit values (0..=127). The cast is safe.
        let key_code = event.native_key_code as u16;

        // NSEvent factory. All args confirmed against NSEvent.rs:1094-1105.
        // `unused_pass_nil` (NSGraphicsContext parameter) must be None.
        // The parameter was deprecated in macOS 10.14; always pass nil.
        let ns_event: Option<Retained<NSEvent>> = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            // r#type: NSEventType — KeyDown or KeyUp
            ns_type,
            // location: NSPoint — irrelevant for off-screen key events
            objc2_foundation::NSPoint { x: 0.0, y: 0.0 },
            // flags: NSEventModifierFlags — CEF bitmask converted above
            mods,
            // time: NSTimeInterval (f64) — 0.0 is valid for synthesised events
            0.0,
            // w_num: NSInteger — 0 for off-screen (no window)
            0,
            // unused_pass_nil: Option<&NSGraphicsContext> — always None (deprecated)
            None,
            // keys: &NSString — characters string (printable representation)
            &chars,
            // ukeys: &NSString — unmodified characters string
            &unchars,
            // flag: bool — isARepeat; we don't wire key-repeat from NeutralKeyEvent
            false,
            // code: c_ushort — Carbon HID virtual key code
            key_code,
        );

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent::keyEventWith… returned nil");
            return;
        };

        unsafe {
            // Dispatch via NSResponder interface inherited by WKWebView → NSView.
            // `keyDown:` / `keyUp:` confirmed: NSResponder.rs:151-158.
            let view = self_as_nsview(web_view);
            match event.kind {
                KeyEventKind::RawDown | KeyEventKind::Char => view.keyDown(&ns_event),
                KeyEventKind::Up => view.keyUp(&ns_event),
            }
        }
        tracing::debug!("webkit-cocoa input: key_event kind={:?}", event.kind);
    }

    // ── Mouse events ──────────────────────────────────────────────────────────

    /// Dispatch a mouse-move `NSEvent` to `web_view`.
    ///
    /// Uses `NSEventType::MouseMoved` = Self(5). Confirmed: NSEvent.rs:29.
    ///
    /// # Coordinate system
    ///
    /// AppKit uses a bottom-left origin coordinate system. The `y` value from
    /// buffr uses a top-left origin. We do NOT flip here because WKWebView in
    /// off-screen mode processes the event in its own coordinate space anyway.
    ///
    /// # TODO: y-flip verification
    ///
    /// On first macOS CI run, verify that mouse events land at the correct
    /// pixel position. If hover highlights appear at wrong positions, apply
    /// `y = view_height - y` before constructing the NSPoint.
    pub(crate) fn dispatch_mouse_move(web_view: &WKWebView, x: i32, y: i32, modifiers: u32) {
        let mods = cef_mods_to_ns_mods(modifiers);
        let location = objc2_foundation::NSPoint {
            x: x as f64,
            y: y as f64,
        };

        // `mouseEventWithType:…` confirmed: NSEvent.rs:1077-1089.
        // pressure: c_float (NOT f64). For mouse-move, pressure = 0.0.
        let ns_event: Option<Retained<NSEvent>> = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            // r#type: NSEventType — MouseMoved = Self(5)
            NSEventType::MouseMoved,
            // location: NSPoint
            location,
            // flags: NSEventModifierFlags
            mods,
            // time: NSTimeInterval
            0.0,
            // w_num: NSInteger — 0 for off-screen
            0,
            // unused_pass_nil: Option<&NSGraphicsContext>
            None,
            // e_num: NSInteger — eventNumber, 0 is fine for synthesised events
            0,
            // c_num: NSInteger — clickCount, 0 for move events
            0,
            // pressure: c_float — 0.0 for no pressure (mouse-move)
            0.0_f32 as c_float,
        );

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent mouse-move returned nil");
            return;
        };
        unsafe {
            // `mouseMoved:` confirmed: NSResponder.rs:111-113.
            self_as_nsview(web_view).mouseMoved(&ns_event);
        }
        tracing::debug!("webkit-cocoa input: mouse_move ({x},{y})");
    }

    /// Dispatch a mouse-click (down or up) `NSEvent` to `web_view`.
    ///
    /// Button type mapping confirmed against NSEvent.rs:21-65:
    ///   Left down  → NSEventType::LeftMouseDown  = Self(1)
    ///   Left up    → NSEventType::LeftMouseUp    = Self(2)
    ///   Right down → NSEventType::RightMouseDown = Self(3)
    ///   Right up   → NSEventType::RightMouseUp   = Self(4)
    ///   Other down → NSEventType::OtherMouseDown = Self(25)
    ///   Other up   → NSEventType::OtherMouseUp   = Self(26)
    ///
    /// Dispatcher methods confirmed: NSResponder.rs:80-108.
    pub(crate) fn dispatch_mouse_click(
        web_view: &WKWebView,
        x: i32,
        y: i32,
        button: &MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        let ns_type = mouse_button_event_type(button, mouse_up);
        let mods = cef_mods_to_ns_mods(modifiers);
        let location = objc2_foundation::NSPoint {
            x: x as f64,
            y: y as f64,
        };

        // c_num (clickCount) is NSInteger = isize.
        // pressure: c_float (u32). Normal click = 1.0.
        let ns_event: Option<Retained<NSEvent>> = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            // r#type: NSEventType
            ns_type,
            // location: NSPoint
            location,
            // flags: NSEventModifierFlags
            mods,
            // time: NSTimeInterval
            0.0,
            // w_num: NSInteger
            0,
            // unused_pass_nil: Option<&NSGraphicsContext>
            None,
            // e_num: NSInteger — eventNumber
            0,
            // c_num: NSInteger — clickCount (CEF passes 1 for single click)
            click_count as isize,
            // pressure: c_float — 1.0 for a normal button press
            1.0_f32 as c_float,
        );

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent mouse-click returned nil");
            return;
        };

        unsafe {
            let view = self_as_nsview(web_view);
            // Dispatch confirmed: NSResponder.rs:80-108.
            match (button, mouse_up) {
                (MouseButton::Left, false) => view.mouseDown(&ns_event),
                (MouseButton::Left, true) => view.mouseUp(&ns_event),
                (MouseButton::Right, false) => view.rightMouseDown(&ns_event),
                (MouseButton::Right, true) => view.rightMouseUp(&ns_event),
                (MouseButton::Middle | MouseButton::Other(_), false) => {
                    view.otherMouseDown(&ns_event)
                }
                (MouseButton::Middle | MouseButton::Other(_), true) => view.otherMouseUp(&ns_event),
            }
        }
        tracing::debug!("webkit-cocoa input: mouse_click ({x},{y}) up={mouse_up}");
    }

    /// Dispatch a scroll-wheel `NSEvent` to `web_view`.
    ///
    /// `NSEventType::ScrollWheel` = Self(22). Confirmed: NSEvent.rs:55.
    ///
    /// # Known limitation — zero-delta scroll
    ///
    /// The `mouseEventWithType:…` constructor does not set `deltaX`/`deltaY`
    /// on the resulting NSEvent. Those are computed from the raw HID device
    /// report; there is no public API to inject them via this path.
    ///
    /// As a result, `delta_x` and `delta_y` are accepted but not forwarded
    /// to the web view. The dispatched event is a zero-delta scroll that
    /// triggers WKWebView's scroll machinery with no effect.
    ///
    /// # TODO: real scroll via CGEvent
    ///
    /// A proper implementation would use `CGEventCreateScrollWheelEvent2`
    /// (objc2-core-graphics, not yet exposing this in 0.3.2) to create an
    /// event with actual deltas, then wrap it via the private
    /// `NSEvent(with:CGEvent:)` initialiser. Deferred to Phase C.
    pub(crate) fn dispatch_mouse_wheel(
        web_view: &WKWebView,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    ) {
        let mods = cef_mods_to_ns_mods(modifiers);
        let location = objc2_foundation::NSPoint {
            x: x as f64,
            y: y as f64,
        };

        // ScrollWheel via mouseEventWithType:… produces a zero-delta scroll.
        // See module-level doc for the limitation and future fix.
        let ns_event: Option<Retained<NSEvent>> = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            // r#type: NSEventType::ScrollWheel = Self(22)
            NSEventType::ScrollWheel,
            // location: NSPoint
            location,
            // flags: NSEventModifierFlags
            mods,
            // time: NSTimeInterval
            0.0,
            // w_num: NSInteger
            0,
            // unused_pass_nil: Option<&NSGraphicsContext>
            None,
            // e_num: NSInteger
            0,
            // c_num: NSInteger
            0,
            // pressure: c_float
            0.0_f32 as c_float,
        );

        // delta_x / delta_y are intentionally not forwarded (see TODO above).
        // Suppress unused variable warnings.
        let _ = (delta_x, delta_y);

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent scroll-wheel returned nil");
            return;
        };
        unsafe {
            // `scrollWheel:` confirmed: NSResponder.rs:126-128.
            self_as_nsview(web_view).scrollWheel(&ns_event);
        }
        tracing::debug!("webkit-cocoa input: mouse_wheel ({x},{y}) dx={delta_x} dy={delta_y}");
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Map CEF EVENTFLAG_* modifier bits to `NSEventModifierFlags`.
    ///
    /// CEF bit definitions (cef/include/internal/cef_types.h):
    ///   EVENTFLAG_SHIFT_DOWN   = 1 << 1  (0x0002)
    ///   EVENTFLAG_CONTROL_DOWN = 1 << 2  (0x0004)
    ///   EVENTFLAG_ALT_DOWN     = 1 << 3  (0x0008)
    ///   EVENTFLAG_COMMAND_DOWN = 1 << 4  (0x0010)  [macOS Command / Meta]
    ///
    /// `NSEventModifierFlags` is a `bitflags!` type (NSEvent.rs:386-407).
    /// Confirmed constant names:
    ///   `NSEventModifierFlags::Shift`   = 1<<17  (0x020000)
    ///   `NSEventModifierFlags::Control` = 1<<18  (0x040000)
    ///   `NSEventModifierFlags::Option`  = 1<<19  (0x080000)  ← Alt maps here
    ///   `NSEventModifierFlags::Command` = 1<<20  (0x100000)
    fn cef_mods_to_ns_mods(cef_mods: u32) -> NSEventModifierFlags {
        let mut ns = NSEventModifierFlags::empty();
        if cef_mods & 0x0002 != 0 {
            ns |= NSEventModifierFlags::Shift;
        }
        if cef_mods & 0x0004 != 0 {
            ns |= NSEventModifierFlags::Control;
        }
        if cef_mods & 0x0008 != 0 {
            // Alt / Option — `NSEventModifierFlags::Option` confirmed: NSEvent.rs:394-395.
            ns |= NSEventModifierFlags::Option;
        }
        if cef_mods & 0x0010 != 0 {
            ns |= NSEventModifierFlags::Command;
        }
        ns
    }

    /// Create an `NSString` from a UTF-16 code unit.
    ///
    /// `NeutralKeyEvent::character` and `unmodified_character` are raw `u16`
    /// values from CEF's key event. We convert to a Rust `char` (or empty
    /// string for the null character) and then use `NSString::from_str`.
    ///
    /// `NSString::from_str(&str)` confirmed: objc2-foundation-0.3.2/src/string.rs:113.
    ///
    /// Note: `keyEventWithType:…` requires `&NSString`, not `Option<&NSString>`.
    /// For non-printable keys (arrows, etc.) the character may be a private-use
    /// Unicode codepoint. NSString handles these correctly.
    fn char_to_nsstring(c: u16) -> Retained<NSString> {
        let s = if c == 0 {
            String::new()
        } else {
            char::from_u32(c as u32)
                .map(|ch| ch.to_string())
                .unwrap_or_default()
        };
        NSString::from_str(&s)
    }

    /// Map a `MouseButton` and up/down flag to an `NSEventType`.
    ///
    /// Confirmed variant names + values in NSEvent.rs:21-65:
    ///   LeftMouseDown  = Self(1)
    ///   LeftMouseUp    = Self(2)
    ///   RightMouseDown = Self(3)
    ///   RightMouseUp   = Self(4)
    ///   OtherMouseDown = Self(25)
    ///   OtherMouseUp   = Self(26)
    fn mouse_button_event_type(button: &MouseButton, mouse_up: bool) -> NSEventType {
        match (button, mouse_up) {
            (MouseButton::Left, false) => NSEventType::LeftMouseDown,
            (MouseButton::Left, true) => NSEventType::LeftMouseUp,
            (MouseButton::Right, false) => NSEventType::RightMouseDown,
            (MouseButton::Right, true) => NSEventType::RightMouseUp,
            (_, false) => NSEventType::OtherMouseDown,
            (_, true) => NSEventType::OtherMouseUp,
        }
    }

    /// Coerce a `&WKWebView` to `&NSView` for NSResponder dispatch.
    ///
    /// WKWebView → NSView → NSResponder inheritance chain means all NSResponder
    /// methods are available. The Deref chain in objc2 lets us cast via
    /// `as &NSView` once we import `objc2::ClassType`.
    ///
    /// # Safety
    ///
    /// WKWebView IS-A NSView on macOS. The cast is safe as long as `web_view`
    /// points to a live WKWebView instance (guaranteed by `Retained<WKWebView>`
    /// lifetime in the caller).
    unsafe fn self_as_nsview(web_view: &WKWebView) -> &NSView {
        web_view.as_ref() as &NSView
    }
}
