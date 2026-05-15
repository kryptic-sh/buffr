//! Input translation: [`NeutralKeyEvent`] / [`MouseButton`] → Cocoa NSEvent.
//!
//! WKWebView processes keyboard and mouse events as AppKit `NSEvent` objects.
//! This module translates buffr's engine-agnostic input types into the
//! Cocoa event representation and dispatches them to the WKWebView.
//!
//! # Threading
//!
//! All dispatch calls in this module MUST happen on the **main thread** (the
//! AppKit run loop thread). The caller — `runtime.rs` — ensures this by
//! running inside a `dispatch_async(dispatch_get_main_queue(), …)` closure.
//!
//! # TODO(verify-on-mac)
//!
//! Every NSEvent construction call is tagged with a `// TODO(verify-on-mac):`
//! annotation that records:
//! - Which Objective-C selector maps to which Rust binding in `objc2-app-kit`.
//! - Which fields of `NeutralKeyEvent` carry the right semantics.
//! - Whether the `windowNumber` needs to be 0 or the actual window's number.
//!
//! A reader on macOS should be able to fix each annotation in one pass.

#[cfg(target_os = "macos")]
pub(crate) use macos::*;

#[cfg(target_os = "macos")]
mod macos {
    use buffr_engine::{KeyEventKind, MouseButton, NeutralKeyEvent};
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSEvent,     // TODO(verify-on-mac): objc2-app-kit 0.3 re-exports NSEvent
        NSEventType, // TODO(verify-on-mac): Confirm NSEventType enum variants exist
        NSView,      // TODO(verify-on-mac): WKWebView inherits NSView; calls go here
    };
    use objc2_web_kit::WKWebView;

    // ── Key events ────────────────────────────────────────────────────────────

    /// Translate and dispatch a `NeutralKeyEvent` to `web_view`.
    ///
    /// Constructs an `NSEvent` of type `NSEventTypeKeyDown` or
    /// `NSEventTypeKeyUp` and calls `-[NSView keyDown:]` / `-[NSView keyUp:]`.
    ///
    /// # NSEvent construction
    ///
    /// The relevant AppKit factory method is:
    /// ```objc
    /// + (NSEvent *)keyEventWithType:(NSEventType)type
    ///                      location:(NSPoint)location
    ///                 modifierFlags:(NSEventModifierFlags)flags
    ///                     timestamp:(NSTimeInterval)time
    ///                  windowNumber:(NSInteger)wnum
    ///                       context:(NSGraphicsContext *)ctx
    ///                    characters:(NSString *)chars
    ///   charactersIgnoringModifiers:(NSString *)unchars
    ///                     isARepeat:(BOOL)repeat
    ///                       keyCode:(unsigned short)keyCode
    /// ```
    ///
    /// In `objc2-app-kit 0.3` this selector is bound as (exact name is a guess):
    ///   `NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode`
    ///
    /// # TODO(verify-on-mac)
    ///
    /// - Confirm the Rust binding name for the above factory method.
    /// - Confirm `NSEventType::KeyDown` / `NSEventType::KeyUp` variant names.
    /// - `NeutralKeyEvent::native_key_code` is the macOS HID key code. Verify
    ///   that CEF's `native_key_code` for macOS equals `NSEvent::keyCode`.
    /// - `NeutralKeyEvent::modifiers` is CEF's EVENTFLAG_* bitmask. Map to
    ///   `NSEventModifierFlags` (Shift=0x20000, Control=0x40000, Option=0x80000,
    ///   Command=0x100000). Confirm the mapping table below is correct.
    /// - `window_number`: pass 0 (no window) for off-screen. Confirm WKWebView
    ///   accepts windowNumber=0 in off-screen mode.
    pub(crate) fn dispatch_key_event(web_view: &WKWebView, event: &NeutralKeyEvent) {
        let ns_type = match event.kind {
            KeyEventKind::RawDown | KeyEventKind::Char => NSEventType::KeyDown,
            KeyEventKind::Up => NSEventType::KeyUp,
        };

        // Translate CEF EVENTFLAG_* to NSEventModifierFlags.
        // TODO(verify-on-mac): Verify bit positions and NSEventModifierFlags constants.
        let mods = cef_mods_to_ns_mods(event.modifiers);

        // Build the character strings from the NeutralKeyEvent.
        // TODO(verify-on-mac): Verify that character/unmodified_character are
        // correct UTF-16 code units that NSString can wrap.
        let chars = char_to_nsstring(event.character);
        let unchars = char_to_nsstring(event.unmodified_character);

        // TODO(verify-on-mac): Confirm that native_key_code == macOS HID key code.
        let key_code = event.native_key_code as u16;

        // TODO(verify-on-mac): Confirm objc2-app-kit binding name for
        //   +[NSEvent keyEventWithType:location:modifierFlags:timestamp:
        //             windowNumber:context:characters:
        //             charactersIgnoringModifiers:isARepeat:keyCode:]
        // The unsafe block is required because NSEvent construction is not
        // memory-safe (it interacts with the Objective-C runtime).
        let ns_event: Option<Retained<NSEvent>> = unsafe {
            NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                ns_type,
                objc2_foundation::NSPoint { x: 0.0, y: 0.0 },
                mods,
                0.0,   // timestamp — 0 is acceptable for synthesised events
                0,     // windowNumber — 0 for off-screen
                None,  // context (deprecated since macOS 10.14; pass nil)
                Some(&chars),
                Some(&unchars),
                false, // isARepeat — TODO(verify-on-mac): wire from NeutralKeyEvent if needed
                key_code,
            )
        };

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent::keyEventWith… returned nil");
            return;
        };

        unsafe {
            // TODO(verify-on-mac): Confirm WKWebView inherits keyDown:/keyUp: from NSView
            // and that calling them directly is the correct dispatch path for OSR.
            match event.kind {
                KeyEventKind::RawDown | KeyEventKind::Char => {
                    // NSView::keyDown is a required method; WKWebView inherits it.
                    (web_view as &NSView).keyDown(&ns_event);
                }
                KeyEventKind::Up => {
                    (web_view as &NSView).keyUp(&ns_event);
                }
            }
        }
        tracing::debug!("webkit-cocoa input: key_event kind={:?}", event.kind);
    }

    // ── Mouse events ──────────────────────────────────────────────────────────

    /// Dispatch a mouse-move `NSEvent` to `web_view`.
    ///
    /// # TODO(verify-on-mac)
    ///
    /// - Confirm `NSEventType::MouseMoved` variant name.
    /// - Confirm `NSEvent::mouseEventWithType:location:modifierFlags:timestamp:
    ///   windowNumber:context:eventNumber:clickCount:pressure:` binding in
    ///   objc2-app-kit 0.3.
    /// - Verify coordinate system: AppKit uses bottom-left origin; CEF / winit
    ///   use top-left. Apply `y = height - y` transform if needed.
    pub(crate) fn dispatch_mouse_move(web_view: &WKWebView, x: i32, y: i32, modifiers: u32) {
        let mods = cef_mods_to_ns_mods(modifiers);
        let location = objc2_foundation::NSPoint {
            x: x as f64,
            y: y as f64, // TODO(verify-on-mac): may need y-flip; see doc above
        };

        let ns_event: Option<Retained<NSEvent>> = unsafe {
            // TODO(verify-on-mac): Confirm binding name for mouseEventWithType:…
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::MouseMoved,
                location,
                mods,
                0.0, // timestamp
                0,   // windowNumber
                None, // context (deprecated)
                0,   // eventNumber
                0,   // clickCount
                0.0, // pressure
            )
        };

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent mouse-move returned nil");
            return;
        };
        unsafe {
            // TODO(verify-on-mac): Confirm mouseMoved: is the right selector.
            (web_view as &NSView).mouseMoved(&ns_event);
        }
        tracing::debug!("webkit-cocoa input: mouse_move ({x},{y})");
    }

    /// Dispatch a mouse-click (down or up) `NSEvent` to `web_view`.
    ///
    /// # TODO(verify-on-mac)
    ///
    /// - Confirm NSEventType variants for mouseDown/mouseUp on left/right/other.
    /// - Verify `clickCount` plumbing; CEF passes 1 for a single click.
    pub(crate) fn dispatch_mouse_click(
        web_view: &WKWebView,
        x: i32,
        y: i32,
        button: &buffr_engine::MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        let ns_type = mouse_button_event_type(button, mouse_up);
        let mods = cef_mods_to_ns_mods(modifiers);
        let location = objc2_foundation::NSPoint {
            x: x as f64,
            y: y as f64, // TODO(verify-on-mac): y-flip may be needed
        };

        let ns_event: Option<Retained<NSEvent>> = unsafe {
            // TODO(verify-on-mac): Confirm binding name.
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                ns_type,
                location,
                mods,
                0.0,
                0,
                None,
                0,
                click_count as isize, // TODO(verify-on-mac): confirm isize vs i32 for clickCount
                1.0, // pressure: 1.0 for a normal click
            )
        };

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent mouse-click returned nil");
            return;
        };

        unsafe {
            // TODO(verify-on-mac): Confirm correct NSView selectors per button/up state.
            match (button, mouse_up) {
                (MouseButton::Left, false) => (web_view as &NSView).mouseDown(&ns_event),
                (MouseButton::Left, true) => (web_view as &NSView).mouseUp(&ns_event),
                (MouseButton::Right, false) => (web_view as &NSView).rightMouseDown(&ns_event),
                (MouseButton::Right, true) => (web_view as &NSView).rightMouseUp(&ns_event),
                (MouseButton::Middle | MouseButton::Other(_), false) => {
                    (web_view as &NSView).otherMouseDown(&ns_event)
                }
                (MouseButton::Middle | MouseButton::Other(_), true) => {
                    (web_view as &NSView).otherMouseUp(&ns_event)
                }
            }
        }
        tracing::debug!("webkit-cocoa input: mouse_click ({x},{y}) up={mouse_up}");
    }

    /// Dispatch a scroll-wheel `NSEvent` to `web_view`.
    ///
    /// # TODO(verify-on-mac)
    ///
    /// - Confirm that `NSEventType::ScrollWheel` is the correct type for
    ///   synthesised scroll events.
    /// - `deltaX` / `deltaY` on `NSEvent` are points (not pixels). Verify
    ///   that CEF's pixel deltas need scaling. Typically 1 pixel ≈ 1 point at
    ///   1.0× scale, so we pass through directly for now.
    /// - Confirm `NSView::scrollWheel:` is available on WKWebView (inherited).
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

        // TODO(verify-on-mac): scrollWheel NSEvent requires deltaX/deltaY which
        // are properties of NSEvent, not constructor arguments. The standard
        // NSEvent factory method for scroll wheels is:
        //   +[NSEvent scrollWheelEventWithLocation:modifierFlags:timestamp:
        //             windowNumber:context:deltaX:deltaY:deltaZ:]
        // However this private method may not be available in objc2-app-kit 0.3.
        // If not available, use CGEvent + NSEvent(with:CGEvent:) (also private-ish).
        // For now, construct a mouse-moved event as a placeholder and log the gap.
        //
        // This is one of the higher-risk guesses in Phase B.
        let ns_event: Option<Retained<NSEvent>> = unsafe {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::ScrollWheel,
                location,
                mods,
                0.0,
                0,
                None,
                0,
                0,
                0.0,
            )
        };

        let Some(ns_event) = ns_event else {
            tracing::warn!("webkit-cocoa input: NSEvent scroll-wheel returned nil");
            return;
        };
        // TODO(verify-on-mac): `delta_x` / `delta_y` are not injected into
        // the event via this constructor path. A CGEvent-based approach is
        // required for real scroll. This dispatches a zero-delta scroll.
        let _ = (delta_x, delta_y); // suppress unused-warning until real impl
        unsafe {
            (web_view as &NSView).scrollWheel(&ns_event);
        }
        tracing::debug!("webkit-cocoa input: mouse_wheel ({x},{y}) dx={delta_x} dy={delta_y}");
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Map CEF EVENTFLAG_* modifier bits to `NSEventModifierFlags`.
    ///
    /// CEF bit definitions (from cef/include/internal/cef_types.h):
    ///   EVENTFLAG_SHIFT_DOWN   = 1 << 1  (0x0002)
    ///   EVENTFLAG_CONTROL_DOWN = 1 << 2  (0x0004)
    ///   EVENTFLAG_ALT_DOWN     = 1 << 3  (0x0008)
    ///   EVENTFLAG_COMMAND_DOWN = 1 << 4  (0x0010)  [macOS Command / Meta]
    ///
    /// NSEventModifierFlags values (from AppKit headers):
    ///   NSEventModifierFlagShift   = 1 << 17 (0x020000)
    ///   NSEventModifierFlagControl = 1 << 18 (0x040000)
    ///   NSEventModifierFlagOption  = 1 << 19 (0x080000)
    ///   NSEventModifierFlagCommand = 1 << 20 (0x100000)
    ///
    /// TODO(verify-on-mac): Confirm objc2-app-kit 0.3 re-exports NSEventModifierFlags
    /// and these constants have the correct raw values. The objc2 binding may
    /// use a bitflags! type rather than raw u64.
    fn cef_mods_to_ns_mods(cef_mods: u32) -> objc2_app_kit::NSEventModifierFlags {
        use objc2_app_kit::NSEventModifierFlags;
        let mut ns = NSEventModifierFlags::empty();
        if cef_mods & 0x0002 != 0 {
            ns |= NSEventModifierFlags::NSEventModifierFlagShift; // TODO(verify-on-mac)
        }
        if cef_mods & 0x0004 != 0 {
            ns |= NSEventModifierFlags::NSEventModifierFlagControl; // TODO(verify-on-mac)
        }
        if cef_mods & 0x0008 != 0 {
            ns |= NSEventModifierFlags::NSEventModifierFlagOption; // TODO(verify-on-mac)
        }
        if cef_mods & 0x0010 != 0 {
            ns |= NSEventModifierFlags::NSEventModifierFlagCommand; // TODO(verify-on-mac)
        }
        ns
    }

    /// Create an NSString wrapping a single UTF-16 code unit.
    ///
    /// TODO(verify-on-mac): Confirm the NSString constructor for a raw UTF-16
    /// char. `objc2-foundation 0.3` may expose `NSString::from_str` (for &str)
    /// which is simpler. The u16 → char conversion below may produce `\0` for
    /// non-printable keys, which is correct for NSEvent.
    fn char_to_nsstring(c: u16) -> objc2_foundation::NSString {
        use objc2_foundation::NSString;
        let s = if c == 0 {
            String::new()
        } else {
            char::from_u32(c as u32)
                .map(|ch| ch.to_string())
                .unwrap_or_default()
        };
        // TODO(verify-on-mac): Confirm NSString::from_str is the right API in
        // objc2-foundation 0.3. Alternative: NSString::alloc().init_str(&s).
        NSString::from_str(&s) // TODO(verify-on-mac)
    }

    /// Map a `MouseButton` and up/down flag to an `NSEventType`.
    ///
    /// TODO(verify-on-mac): Confirm these NSEventType variant names in
    /// objc2-app-kit 0.3.
    fn mouse_button_event_type(button: &MouseButton, mouse_up: bool) -> NSEventType {
        match (button, mouse_up) {
            (MouseButton::Left, false) => NSEventType::LeftMouseDown, // TODO(verify-on-mac)
            (MouseButton::Left, true) => NSEventType::LeftMouseUp,    // TODO(verify-on-mac)
            (MouseButton::Right, false) => NSEventType::RightMouseDown, // TODO(verify-on-mac)
            (MouseButton::Right, true) => NSEventType::RightMouseUp,  // TODO(verify-on-mac)
            (_, false) => NSEventType::OtherMouseDown,                // TODO(verify-on-mac)
            (_, true) => NSEventType::OtherMouseUp,                   // TODO(verify-on-mac)
        }
    }
}
