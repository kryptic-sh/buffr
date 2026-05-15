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

private:
    std::string m_url;
    std::string m_title;
    std::vector<std::string> m_back_history;
    std::vector<std::string> m_forward_history;
    uint32_t m_width;
    uint32_t m_height;
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

} // namespace ladybird
} // namespace buffr
