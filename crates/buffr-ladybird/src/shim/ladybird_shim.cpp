// TODO(phase-c): Replace this stub implementation with real Ladybird embedding.
//
// This file is a zero-dependency C++17 stub that satisfies the cxx bridge FFI
// surface so the Rust crate builds and smoke-tests without any Ladybird sources.
//
// Phase C replacement checklist:
//  - Build LibWeb + LibGfx + LibJS from a pinned Ladybird commit.
//  - Implement `WebContent` around `WebContentClient` (out-of-process) or
//    a direct LibWeb `Page` (in-process, experimental).
//  - Replace `read_pixels` stub with LibGfx bitmap readback.
//  - Wire `navigate` / `go_back` / `go_forward` to real WebContent IPC messages.
//  - Replace `m_title` stub with a real `on_title_change` callback from LibWeb.

#include "ladybird_shim.h"

#include <cstring>  // std::memset, std::memcpy

namespace buffr {
namespace ladybird {

// ── BGRA stub colour (dark grey, fully opaque) ──────────────────────────────
// Phase C: replaced by LibGfx surface readback.
static constexpr uint8_t STUB_B = 0x10;
static constexpr uint8_t STUB_G = 0x10;
static constexpr uint8_t STUB_R = 0x10;
static constexpr uint8_t STUB_A = 0xFF;

// ── WebContent ───────────────────────────────────────────────────────────────

WebContent::WebContent(rust::Str initial_url, uint32_t width, uint32_t height)
    : m_url(initial_url.begin(), initial_url.end())
    , m_title("Ladybird (stub)")
    , m_width(width)
    , m_height(height)
{
    // TODO(phase-c): launch WebContentServer process and establish IPC socket.
}

void WebContent::navigate(rust::Str url)
{
    // Push current URL onto back-history before navigating.
    m_back_history.push_back(m_url);
    m_forward_history.clear();
    m_url = std::string(url.begin(), url.end());
    m_title = "Ladybird (stub)";
    // TODO(phase-c): send LoadURL IPC message to WebContentServer.
}

void WebContent::reload()
{
    // No-op in the stub — URL stays the same, title unchanged.
    // TODO(phase-c): send ReloadPage IPC message.
}

void WebContent::stop()
{
    // TODO(phase-c): send StopLoading IPC message.
}

void WebContent::go_back()
{
    if (m_back_history.empty()) return;
    m_forward_history.push_back(m_url);
    m_url = m_back_history.back();
    m_back_history.pop_back();
    // TODO(phase-c): send GoBack IPC message.
}

void WebContent::go_forward()
{
    if (m_forward_history.empty()) return;
    m_back_history.push_back(m_url);
    m_url = m_forward_history.back();
    m_forward_history.pop_back();
    // TODO(phase-c): send GoForward IPC message.
}

rust::String WebContent::url() const
{
    return rust::String(m_url);
}

rust::String WebContent::title() const
{
    return rust::String(m_title);
}

bool WebContent::can_go_back() const
{
    return !m_back_history.empty();
}

bool WebContent::can_go_forward() const
{
    return !m_forward_history.empty();
}

void WebContent::resize(uint32_t width, uint32_t height)
{
    m_width = width;
    m_height = height;
    // TODO(phase-c): send SetViewportSize IPC message to WebContentServer.
}

bool WebContent::read_pixels(rust::Slice<uint8_t> out) const
{
    // Phase B: fill with a solid dark-grey BGRA pattern.
    // Phase C: copy readback bytes from LibGfx bitmap into `out`.
    const size_t expected = static_cast<size_t>(m_width) * m_height * 4u;
    if (out.size() < expected) return false;

    uint8_t* dst = out.data();
    for (size_t i = 0; i < expected; i += 4) {
        dst[i + 0] = STUB_B;
        dst[i + 1] = STUB_G;
        dst[i + 2] = STUB_R;
        dst[i + 3] = STUB_A;
    }
    return true;
}

void WebContent::send_key(uint32_t key_code, bool is_press, uint32_t modifiers)
{
    // TODO(phase-c): send KeyEvent IPC message with Ladybird KeyCode mapping.
    (void)key_code; (void)is_press; (void)modifiers;
}

void WebContent::send_mouse_move(int32_t x, int32_t y)
{
    // TODO(phase-c): send MouseMove IPC message.
    (void)x; (void)y;
}

void WebContent::send_mouse_button(uint32_t button, int32_t x, int32_t y, bool is_press)
{
    // TODO(phase-c): send MouseDown/MouseUp IPC message.
    (void)button; (void)x; (void)y; (void)is_press;
}

void WebContent::send_scroll(int32_t x, int32_t y, double dx, double dy)
{
    // TODO(phase-c): send MouseWheel IPC message.
    (void)x; (void)y; (void)dx; (void)dy;
}

void WebContent::set_zoom(double zoom)
{
    // Phase B: store the zoom level in m_zoom. No render effect.
    // Phase C: send SetZoomLevel IPC message to WebContentServer (maps to
    //          LibWeb `Page::set_zoom_level`).
    m_zoom = zoom;
}

double WebContent::zoom() const
{
    return m_zoom;
}

// ── Free-function entry points (called from cxx bridge) ─────────────────────

::std::unique_ptr<WebContent> webcontent_new(rust::Str initial_url, uint32_t width, uint32_t height)
{
    return ::std::make_unique<WebContent>(initial_url, width, height);
}

void webcontent_navigate(WebContent& wc, rust::Str url)  { wc.navigate(url); }
void webcontent_reload(WebContent& wc)                    { wc.reload(); }
void webcontent_stop(WebContent& wc)                      { wc.stop(); }
void webcontent_go_back(WebContent& wc)                   { wc.go_back(); }
void webcontent_go_forward(WebContent& wc)                { wc.go_forward(); }

rust::String webcontent_url(const WebContent& wc)         { return wc.url(); }
rust::String webcontent_title(const WebContent& wc)       { return wc.title(); }
bool webcontent_can_go_back(const WebContent& wc)         { return wc.can_go_back(); }
bool webcontent_can_go_forward(const WebContent& wc)      { return wc.can_go_forward(); }

void webcontent_resize(WebContent& wc, uint32_t width, uint32_t height)
{
    wc.resize(width, height);
}

bool webcontent_read_pixels(const WebContent& wc, rust::Slice<uint8_t> out)
{
    return wc.read_pixels(out);
}

void webcontent_send_key(WebContent& wc, uint32_t key_code, bool is_press, uint32_t modifiers)
{
    wc.send_key(key_code, is_press, modifiers);
}

void webcontent_send_mouse_move(WebContent& wc, int32_t x, int32_t y)
{
    wc.send_mouse_move(x, y);
}

void webcontent_send_mouse_button(WebContent& wc, uint32_t button, int32_t x, int32_t y, bool is_press)
{
    wc.send_mouse_button(button, x, y, is_press);
}

void webcontent_send_scroll(WebContent& wc, int32_t x, int32_t y, double dx, double dy)
{
    wc.send_scroll(x, y, dx, dy);
}

void webcontent_set_zoom(WebContent& wc, double zoom) { wc.set_zoom(zoom); }
double webcontent_zoom(const WebContent& wc)          { return wc.zoom(); }

} // namespace ladybird
} // namespace buffr
