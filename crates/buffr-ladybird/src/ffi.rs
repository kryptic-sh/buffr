//! cxx bridge — Rust ↔ C++ FFI surface for the Ladybird shim.
//!
//! Phase B: bridges to `src/shim/ladybird_shim.{h,cpp}` (a zero-dep stub).
//! Phase C: the C++ side is replaced with real Ladybird WebContent IPC.

#[cxx::bridge(namespace = "buffr::ladybird")]
pub(crate) mod bridge {
    // ── Opaque C++ type ───────────────────────────────────────────────────────
    unsafe extern "C++" {
        include!("buffr-ladybird/src/shim/ladybird_shim.h");

        /// Opaque Ladybird WebContent instance.
        ///
        /// Phase B: backed by a stub struct that stores URL and title in
        /// std::string fields and fills pixels with a solid dark-grey BGRA.
        /// Phase C: backed by a real Ladybird WebContentClient (out-of-process
        /// IPC to the WebContentServer).
        type WebContent;

        // ── Lifecycle ─────────────────────────────────────────────────────────

        /// Create a new WebContent instance loading `initial_url`.
        fn webcontent_new(initial_url: &str, width: u32, height: u32) -> UniquePtr<WebContent>;

        /// Navigate to `url`. Pushes current URL onto the back-history.
        fn webcontent_navigate(wc: Pin<&mut WebContent>, url: &str);

        /// Reload the current URL.
        fn webcontent_reload(wc: Pin<&mut WebContent>);

        /// Stop an in-progress load.
        fn webcontent_stop(wc: Pin<&mut WebContent>);

        /// Navigate backwards in history.
        fn webcontent_go_back(wc: Pin<&mut WebContent>);

        /// Navigate forwards in history.
        fn webcontent_go_forward(wc: Pin<&mut WebContent>);

        // ── Accessors ─────────────────────────────────────────────────────────

        /// Current main-frame URL.
        fn webcontent_url(wc: &WebContent) -> String;

        /// Document title.
        fn webcontent_title(wc: &WebContent) -> String;

        /// Whether back-navigation is possible.
        fn webcontent_can_go_back(wc: &WebContent) -> bool;

        /// Whether forward-navigation is possible.
        fn webcontent_can_go_forward(wc: &WebContent) -> bool;

        // ── Viewport / OSR ───────────────────────────────────────────────────

        /// Notify the engine that the viewport changed to `(width, height)`.
        fn webcontent_resize(wc: Pin<&mut WebContent>, width: u32, height: u32);

        /// Read BGRA pixels into `out`.
        ///
        /// `out` must be at least `width * height * 4` bytes.
        /// Returns `false` when `out` is too small or the engine has no frame.
        fn webcontent_read_pixels(wc: &WebContent, out: &mut [u8]) -> bool;

        // ── Input ─────────────────────────────────────────────────────────────

        /// Send a key press or release.
        ///
        /// `key_code`: Windows VK_* code (Ladybird uses a compatible table).
        /// `modifiers`: CEF EVENTFLAG_* bitmask (Ctrl=2, Shift=4, Alt=8).
        fn webcontent_send_key(
            wc: Pin<&mut WebContent>,
            key_code: u32,
            is_press: bool,
            modifiers: u32,
        );

        /// Send a mouse-move event at CSS-pixel coordinates `(x, y)`.
        fn webcontent_send_mouse_move(wc: Pin<&mut WebContent>, x: i32, y: i32);

        /// Send a mouse-button press or release.
        ///
        /// `button`: 0=Left, 1=Middle, 2=Right (matching Ladybird MouseButton).
        fn webcontent_send_mouse_button(
            wc: Pin<&mut WebContent>,
            button: u32,
            x: i32,
            y: i32,
            is_press: bool,
        );

        /// Send a scroll event.
        ///
        /// `(x, y)`: cursor position; `(dx, dy)`: wheel deltas in CSS pixels.
        fn webcontent_send_scroll(wc: Pin<&mut WebContent>, x: i32, y: i32, dx: f64, dy: f64);

        // ── Zoom ──────────────────────────────────────────────────────────────

        /// Set the page zoom level.
        ///
        /// Phase B: stored in `WebContent::m_zoom` with no rendering effect.
        /// Phase C: maps to LibWeb `Page::set_zoom_level` via WebContent IPC.
        fn webcontent_set_zoom(wc: Pin<&mut WebContent>, zoom: f64);

        /// Read back the current page zoom level.
        fn webcontent_zoom(wc: &WebContent) -> f64;

        // ── IME composition ───────────────────────────────────────────────────
        //
        // Phase B: C++ stub stores the latest preedit string in
        //   `WebContent::m_ime_composition` and no-ops the rest.
        // Phase C: forward to LibWeb's WebContentServer via IPC messages
        //   `Messages::SetIMEComposition`, `Messages::CommitIME`,
        //   `Messages::CancelIME` (exact names TBD by Ladybird API at pin time).

        /// Notify the engine of an in-progress IME composition update.
        ///
        /// `text`: current preedit string.
        /// `sel_start`, `sel_end`: cursor/selection byte range within `text`.
        fn webcontent_ime_set_composition(
            wc: Pin<&mut WebContent>,
            text: &str,
            sel_start: u32,
            sel_end: u32,
        );

        /// Commit the composed text, inserting it into the focused element.
        fn webcontent_ime_commit(wc: Pin<&mut WebContent>, text: &str);

        /// Cancel the current IME composition, discarding the preedit string.
        fn webcontent_ime_cancel(wc: Pin<&mut WebContent>);

        // ── JS evaluation ─────────────────────────────────────────────────────
        //
        // Phase B: C++ stub logs the call and no-ops — LibJS is not linked.
        // Phase C: forward to LibJS via LibWeb's JavaScript execution path, e.g.
        //   `WebContentServer::execute_script(script)` IPC message (exact API
        //   TBD at Ladybird pin time; LibJS has no fire-and-forget public API
        //   separate from the full eval pipeline).
        //
        // TODO(phase-c): wire to real LibJS dispatch once Ladybird embedding
        //   exposes a stable `eval_script` IPC surface.

        /// Evaluate `code` in the active main-frame JS context (fire-and-forget).
        fn webcontent_eval_js(wc: Pin<&mut WebContent>, code: &str);

        /// Evaluate `code` attributed to `url` (fire-and-forget).
        ///
        /// `url` is used as the script origin in DevTools / error stacks.
        /// Phase B stub: `url` is logged but otherwise ignored.
        fn webcontent_eval_main_frame_js(wc: Pin<&mut WebContent>, code: &str, url: &str);

        // ── Edit IPC helpers ──────────────────────────────────────────────────
        //
        // Phase B: C++ stubs log the call and no-op.
        // Phase C: forward to LibWeb's focused-element JS IPC.
        //
        // TODO(phase-c): wire to real LibWeb edit dispatch.

        /// Attach the edit-active CSS class to `field_id`.
        fn webcontent_edit_attach(wc: Pin<&mut WebContent>, field_id: &CxxString);

        /// Cycle focus to the next (`forward=true`) or previous visible input.
        fn webcontent_edit_cycle(wc: Pin<&mut WebContent>, forward: bool);

        /// Detach the edit-active CSS class from `field_id`.
        fn webcontent_edit_detach(wc: Pin<&mut WebContent>, field_id: &CxxString);

        /// Re-focus the element identified by `field_id`.
        fn webcontent_edit_focus(wc: Pin<&mut WebContent>, field_id: &CxxString);

        // ── Audio / video activity ────────────────────────────────────────────
        //
        // Phase B: C++ stubs return false.
        // Phase C: query real Ladybird media-activity state.

        /// Whether the WebContent instance has an active audio stream.
        fn webcontent_any_audio_active(wc: &WebContent) -> bool;

        /// Whether the WebContent instance has an active video stream.
        fn webcontent_any_video_active(wc: &WebContent) -> bool;

        // ── Sleep / wake ──────────────────────────────────────────────────────
        //
        // Phase B: C++ stub logs and no-ops.
        // Phase C: maps to Ladybird WebContent visibility / suspend IPC.

        /// Put the WebContent to sleep (`sleep=true`) or wake it.
        fn webcontent_set_sleep(wc: Pin<&mut WebContent>, sleep: bool);

        // ── Downloads ─────────────────────────────────────────────────────────
        //
        // Phase B: C++ stub logs the URL and no-ops.
        // Phase C: trigger the Ladybird download manager.
        //
        // TODO(phase-c): wire to real Ladybird download infra.

        /// Start a managed browser download for `url`.
        fn webcontent_start_download(wc: Pin<&mut WebContent>, url: &CxxString);

        // ── Loading state ─────────────────────────────────────────────────────
        //
        // Phase B: C++ stub returns false.
        // Phase C: reflects real Ladybird page-load state.

        /// Whether the WebContent is currently loading a document.
        fn webcontent_is_loading(wc: &WebContent) -> bool;
    }
}
