// TODO(phase-c): Replace this entire shim with real Ladybird embedding.
//
// Phase C steps:
//  1. Build LibWeb + LibJS + LibGfx from the Ladybird source tree at a pinned commit.
//  2. Implement a real `WebContent` wrapper that drives LibWeb's out-of-process
//     `WebContentServer` (or a direct in-process embedding if/when the API stabilises).
//  3. Remove the stub pixel fill and wire OSR readback from LibGfx's painting surface.
//  4. Wire real load events so `webcontent_title` and `webcontent_url` reflect live state.
//
// The FFI surface below is intentionally designed to mirror the shape that a real
// Ladybird WebContent process embedding would expose, so Phase C is a drop-in swap.

#pragma once

#include <cstdint>
#include <memory>
#include <string>
#include <vector>
#include "rust/cxx.h"

namespace buffr {
namespace ladybird {

/// Opaque browser content unit.  Phase B: in-process stub with no real LibWeb.
/// Phase C: replace internals with a real Ladybird WebContent client.
class WebContent {
public:
    WebContent(rust::Str initial_url, uint32_t width, uint32_t height);
    ~WebContent() = default;

    // Non-copyable, non-movable (UniquePtr ownership).
    WebContent(const WebContent&) = delete;
    WebContent& operator=(const WebContent&) = delete;
    WebContent(WebContent&&) = delete;
    WebContent& operator=(WebContent&&) = delete;

    // Lifecycle
    void navigate(rust::Str url);
    void reload();
    void stop();
    void go_back();
    void go_forward();

    // Accessors
    rust::String url() const;
    rust::String title() const;
    bool can_go_back() const;
    bool can_go_forward() const;

    // Viewport / OSR
    void resize(uint32_t width, uint32_t height);
    bool read_pixels(rust::Slice<uint8_t> out) const;

    // Input
    void send_key(uint32_t key_code, bool is_press, uint32_t modifiers);
    void send_mouse_move(int32_t x, int32_t y);
    void send_mouse_button(uint32_t button, int32_t x, int32_t y, bool is_press);
    void send_scroll(int32_t x, int32_t y, double dx, double dy);

    // Zoom
    //
    // Phase B: stores the requested zoom level in `m_zoom` (no render effect).
    // Phase C: maps to LibWeb `Page::set_zoom_level` via WebContent IPC.
    void set_zoom(double zoom);
    double zoom() const;

    // IME composition
    //
    // Phase B stub: `set_composition` stores the latest preedit in
    //   `m_ime_composition`; `commit` and `cancel` clear it.
    // Phase C: forward to LibWeb WebContentServer via IPC.
    //   Expected message names (TBD at Ladybird API pin time):
    //   `Messages::WebContentClient::SetComposition`,
    //   `Messages::WebContentClient::CommitComposition`,
    //   `Messages::WebContentClient::CancelComposition`.
    void ime_set_composition(rust::Str text, uint32_t sel_start, uint32_t sel_end);
    void ime_commit(rust::Str text);
    void ime_cancel();

    // JS evaluation
    //
    // Phase B stub: logs the call and no-ops (LibJS not linked in Phase B).
    // Phase C: forward via LibWeb's WebContentServer IPC, e.g.
    //   `WebContentServer::execute_script(script)` (exact API TBD at pin time).
    //
    // TODO(phase-c): wire to real LibJS dispatch.
    void eval_js(rust::Str code);
    void eval_main_frame_js(rust::Str code, rust::Str url);

    // Edit IPC helpers
    //
    // Phase B stub: log and no-op.
    // Phase C: forward to LibWeb focused-element JS IPC.
    void edit_attach(const std::string& field_id);
    void edit_cycle(bool forward);
    void edit_detach(const std::string& field_id);
    void edit_focus(const std::string& field_id);

    // Audio / video activity
    //
    // Phase B stub: returns false.
    // Phase C: query real Ladybird media-activity state.
    bool any_audio_active() const;
    bool any_video_active() const;

    // Sleep / wake
    //
    // Phase B stub: log and no-op.
    // Phase C: Ladybird WebContent visibility / suspend IPC.
    void set_sleep(bool sleep);

    // Downloads
    //
    // Phase B stub: log and no-op.
    // Phase C: trigger the Ladybird download manager.
    void start_download(const std::string& url);

    // Loading state
    //
    // Phase B stub: returns false.
    // Phase C: reflects real Ladybird page-load state.
    bool is_loading() const;

private:
    std::string m_url;
    std::string m_title;
    std::vector<std::string> m_back_history;
    std::vector<std::string> m_forward_history;
    uint32_t m_width;
    uint32_t m_height;
    /// Current page zoom level. Clamped to [0.25, 5.0] on the Rust side.
    double m_zoom = 1.0;
    /// Latest IME preedit string (Phase B stub — no render effect).
    std::string m_ime_composition;
};

// Free-function FFI entry points — called from the cxx bridge.

::std::unique_ptr<WebContent> webcontent_new(rust::Str initial_url, uint32_t width, uint32_t height);
void webcontent_navigate(WebContent& wc, rust::Str url);
void webcontent_reload(WebContent& wc);
void webcontent_stop(WebContent& wc);
void webcontent_go_back(WebContent& wc);
void webcontent_go_forward(WebContent& wc);

rust::String webcontent_url(const WebContent& wc);
rust::String webcontent_title(const WebContent& wc);
bool webcontent_can_go_back(const WebContent& wc);
bool webcontent_can_go_forward(const WebContent& wc);

void webcontent_resize(WebContent& wc, uint32_t width, uint32_t height);
bool webcontent_read_pixels(const WebContent& wc, rust::Slice<uint8_t> out);

void webcontent_send_key(WebContent& wc, uint32_t key_code, bool is_press, uint32_t modifiers);
void webcontent_send_mouse_move(WebContent& wc, int32_t x, int32_t y);
void webcontent_send_mouse_button(WebContent& wc, uint32_t button, int32_t x, int32_t y, bool is_press);
void webcontent_send_scroll(WebContent& wc, int32_t x, int32_t y, double dx, double dy);

// Zoom
void webcontent_set_zoom(WebContent& wc, double zoom);
double webcontent_zoom(const WebContent& wc);

// IME composition
void webcontent_ime_set_composition(WebContent& wc, rust::Str text, uint32_t sel_start, uint32_t sel_end);
void webcontent_ime_commit(WebContent& wc, rust::Str text);
void webcontent_ime_cancel(WebContent& wc);

// JS evaluation
void webcontent_eval_js(WebContent& wc, rust::Str code);
void webcontent_eval_main_frame_js(WebContent& wc, rust::Str code, rust::Str url);

// Edit IPC helpers
void webcontent_edit_attach(WebContent& wc, const std::string& field_id);
void webcontent_edit_cycle(WebContent& wc, bool forward);
void webcontent_edit_detach(WebContent& wc, const std::string& field_id);
void webcontent_edit_focus(WebContent& wc, const std::string& field_id);

// Audio / video activity
bool webcontent_any_audio_active(const WebContent& wc);
bool webcontent_any_video_active(const WebContent& wc);

// Sleep / wake
void webcontent_set_sleep(WebContent& wc, bool sleep);

// Downloads
void webcontent_start_download(WebContent& wc, const std::string& url);

// Loading state
bool webcontent_is_loading(const WebContent& wc);

} // namespace ladybird
} // namespace buffr
