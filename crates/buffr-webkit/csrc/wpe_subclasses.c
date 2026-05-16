/*
 * wpe_subclasses.c — GObject subclasses for buffr-webkit's WPE platform path.
 *
 * Defines four final GObject subclasses (BuffrDisplay/View/Toplevel/Screen)
 * using upstream WebKit's WPE platform classes as parents. The C side is the
 * boilerplate registration that bindgen can't faithfully reproduce (the
 * *Class struct layouts are needed for setting vmethod function pointers).
 *
 * Per-frame work happens in Rust: `buffr_view_render_buffer` (this file's
 * BuffrView vmethod) forwards each delivered WPEBuffer* to the Rust callback
 * `buffr_rust_render_buffer`, which decodes pixels into the shared OsrFrame.
 *
 * Build: compiled by build.rs via the cc crate, linked into buffr-webkit
 * alongside the bindgen-generated FFI bindings.
 */

#include <wpe/wpe-platform.h>

/* Forward declaration of the Rust-side render callback. Implemented in
 * src/platform/wpe_subclass.rs with #[no_mangle] pub extern "C". */
extern void buffr_rust_render_buffer(WPEView *view, WPEBuffer *buffer);

/* ── BuffrScreen ────────────────────────────────────────────────────── */

#define BUFFR_TYPE_SCREEN (buffr_screen_get_type())
G_DECLARE_FINAL_TYPE(BuffrScreen, buffr_screen, BUFFR, SCREEN, WPEScreen)

struct _BuffrScreen {
    WPEScreen parent_instance;
};

G_DEFINE_FINAL_TYPE(BuffrScreen, buffr_screen, WPE_TYPE_SCREEN)

static void buffr_screen_init(BuffrScreen *self) { (void)self; }
static void buffr_screen_class_init(BuffrScreenClass *klass) { (void)klass; }

/* ── BuffrToplevel ──────────────────────────────────────────────────── */

#define BUFFR_TYPE_TOPLEVEL (buffr_toplevel_get_type())
G_DECLARE_FINAL_TYPE(BuffrToplevel, buffr_toplevel, BUFFR, TOPLEVEL, WPEToplevel)

struct _BuffrToplevel {
    WPEToplevel parent_instance;
};

G_DEFINE_FINAL_TYPE(BuffrToplevel, buffr_toplevel, WPE_TYPE_TOPLEVEL)

static void buffr_toplevel_constructed(GObject *object) {
    G_OBJECT_CLASS(buffr_toplevel_parent_class)->constructed(object);
    /* Active right away so WebKit doesn't wait on a configure event. */
    wpe_toplevel_state_changed(WPE_TOPLEVEL(object), WPE_TOPLEVEL_STATE_ACTIVE);
}

static gboolean buffr_toplevel_propagate_size(WPEToplevel *tl, WPEView *v, gpointer user_data) {
    (void)user_data;
    int w, h;
    wpe_toplevel_get_size(tl, &w, &h);
    wpe_view_resized(v, w, h);
    return FALSE;
}

static gboolean buffr_toplevel_resize_vfunc(WPEToplevel *toplevel, int width, int height) {
    wpe_toplevel_resized(toplevel, width, height);
    /* Propagate to every view bound to this toplevel. */
    wpe_toplevel_foreach_view(toplevel, buffr_toplevel_propagate_size, NULL);
    return TRUE;
}

static gboolean buffr_toplevel_set_fullscreen_vfunc(WPEToplevel *toplevel, gboolean fullscreen) {
    WPEToplevelState state = wpe_toplevel_get_state(toplevel);
    state = fullscreen
        ? (WPEToplevelState)(state | WPE_TOPLEVEL_STATE_FULLSCREEN)
        : (WPEToplevelState)(state & ~WPE_TOPLEVEL_STATE_FULLSCREEN);
    wpe_toplevel_state_changed(toplevel, state);
    return TRUE;
}

static void buffr_toplevel_init(BuffrToplevel *self) { (void)self; }
static void buffr_toplevel_class_init(BuffrToplevelClass *klass) {
    GObjectClass *object_class = G_OBJECT_CLASS(klass);
    object_class->constructed = buffr_toplevel_constructed;

    WPEToplevelClass *toplevel_class = WPE_TOPLEVEL_CLASS(klass);
    toplevel_class->resize = buffr_toplevel_resize_vfunc;
    toplevel_class->set_fullscreen = buffr_toplevel_set_fullscreen_vfunc;
}

/* ── BuffrView ──────────────────────────────────────────────────────── */

#define BUFFR_TYPE_VIEW (buffr_view_get_type())
G_DECLARE_FINAL_TYPE(BuffrView, buffr_view, BUFFR, VIEW, WPEView)

struct _BuffrView {
    WPEView parent_instance;
};

G_DEFINE_FINAL_TYPE(BuffrView, buffr_view, WPE_TYPE_VIEW)

static gboolean buffr_view_render_buffer_vfunc(WPEView *view,
                                                WPEBuffer *buffer,
                                                const WPERectangle *damage,
                                                guint n_damage,
                                                GError **error) {
    (void)damage;
    (void)n_damage;
    (void)error;
    g_debug("buffr: render_buffer view=%p buffer=%p", view, buffer);
    /* Forward to Rust for pixel ingestion. Rust calls
     * wpe_view_buffer_rendered itself once it's done. */
    buffr_rust_render_buffer(view, buffer);
    return TRUE;
}

static void buffr_view_on_notify_toplevel(WPEView *view, GParamSpec *pspec, gpointer user_data) {
    (void)pspec;
    (void)user_data;
    WPEToplevel *toplevel = wpe_view_get_toplevel(view);
    g_debug("buffr: notify::toplevel view=%p toplevel=%p", view, toplevel);
    if (!toplevel) {
        wpe_view_unmap(view);
        return;
    }
    int w, h;
    wpe_toplevel_get_size(toplevel, &w, &h);
    g_debug("buffr: toplevel size=%dx%d", w, h);
    if (w > 0 && h > 0)
        wpe_view_resized(view, w, h);
    wpe_view_map(view);
    g_debug("buffr: view mapped=%d", wpe_view_get_mapped(view));
}

static void buffr_view_constructed(GObject *object) {
    G_OBJECT_CLASS(buffr_view_parent_class)->constructed(object);

    /* WPE WebKit expects the view to be "mapped" before it'll begin
     * delivering frames. Attach a notify::toplevel handler that maps the
     * view as soon as a toplevel is assigned. */
    g_signal_connect(object, "notify::toplevel",
                     G_CALLBACK(buffr_view_on_notify_toplevel), NULL);
}

static void buffr_view_init(BuffrView *self) { (void)self; }
static void buffr_view_class_init(BuffrViewClass *klass) {
    GObjectClass *object_class = G_OBJECT_CLASS(klass);
    object_class->constructed = buffr_view_constructed;

    WPEViewClass *view_class = WPE_VIEW_CLASS(klass);
    view_class->render_buffer = buffr_view_render_buffer_vfunc;
}

/* ── BuffrDisplay ───────────────────────────────────────────────────── */

#define BUFFR_TYPE_DISPLAY (buffr_display_get_type())
G_DECLARE_FINAL_TYPE(BuffrDisplay, buffr_display, BUFFR, DISPLAY, WPEDisplay)

struct _BuffrDisplay {
    WPEDisplay parent_instance;

    /* Configured by buffr_display_new(): EGLDisplay we hand WebKit, plus
     * the initial viewport size that drives BuffrScreen. */
    gpointer egl_display;
    int viewport_w;
    int viewport_h;
    double scale;
    int refresh_hz;

    /* Lazily-created single screen handed out by get_screen. */
    BuffrScreen *screen;

    /* Lazily-created DRM device handed out by get_drm_device. */
    WPEDRMDevice *drm_device;
};

G_DEFINE_FINAL_TYPE(BuffrDisplay, buffr_display, WPE_TYPE_DISPLAY)

/* Stash for the most recently created WPEView so Rust can retrieve it
 * after webkit_web_view_new returns (the WebView path swallows the view).
 * Guarded by a static GMutex since GLib may dispatch create_view from any
 * thread it considers thread-default. */
static GMutex buffr_last_view_mutex;
static WPEView *buffr_last_created_view = NULL;

WPEView *buffr_display_take_last_view(void) {
    g_mutex_lock(&buffr_last_view_mutex);
    WPEView *v = buffr_last_created_view;
    buffr_last_created_view = NULL;
    g_mutex_unlock(&buffr_last_view_mutex);
    return v;
}

static gboolean buffr_display_connect_vfunc(WPEDisplay *display, GError **error) {
    (void)display;
    (void)error;
    return TRUE;
}

static WPEView *buffr_display_create_view_vfunc(WPEDisplay *display) {
    WPEView *v = WPE_VIEW(g_object_new(BUFFR_TYPE_VIEW, "display", display, NULL));
    g_mutex_lock(&buffr_last_view_mutex);
    /* Hold a borrowed reference for Rust; we don't ref so that ownership
     * stays with WebKit. Rust must not unref this pointer. */
    buffr_last_created_view = v;
    g_mutex_unlock(&buffr_last_view_mutex);
    return v;
}

static WPEToplevel *buffr_display_create_toplevel_vfunc(WPEDisplay *display, guint max_views) {
    return WPE_TOPLEVEL(g_object_new(BUFFR_TYPE_TOPLEVEL,
                                      "display", display,
                                      "max-views", max_views,
                                      NULL));
}

static gpointer buffr_display_get_egl_vfunc(WPEDisplay *display, GError **error) {
    (void)error;
    return BUFFR_DISPLAY(display)->egl_display;
}

static guint buffr_display_get_n_screens_vfunc(WPEDisplay *display) {
    (void)display;
    return 1;
}

static WPEDRMDevice *buffr_display_get_drm_device_vfunc(WPEDisplay *display) {
    BuffrDisplay *self = BUFFR_DISPLAY(display);
    if (!self->drm_device) {
        /* Use the primary node + render node from /dev/dri. Mesa picks
         * sensible defaults; we hand WebKit a device descriptor so its
         * AcceleratedBackingStore can produce DMA-BUF / EGLImage buffers. */
        self->drm_device = wpe_drm_device_new("/dev/dri/card0", "/dev/dri/renderD128");
    }
    return self->drm_device;
}

static WPEScreen *buffr_display_get_screen_vfunc(WPEDisplay *display, guint index) {
    if (index != 0)
        return NULL;
    BuffrDisplay *self = BUFFR_DISPLAY(display);
    if (!self->screen) {
        /* `id` is a G_PARAM_CONSTRUCT_ONLY property on WPEScreen and reads
         * back via wpe_screen_get_id. WebKit's ScreenManager keys a
         * HashMap<uint32, ScreenData> off this id; HashTable<uint32, …>
         * uses 0 as the empty-slot sentinel, so a screen reporting id=0
         * trips a WTFCrash in HashTable.h on insert. Use a non-zero id. */
        self->screen = g_object_new(BUFFR_TYPE_SCREEN, "id", (guint32)1, NULL);
        wpe_screen_set_size(WPE_SCREEN(self->screen), self->viewport_w, self->viewport_h);
        wpe_screen_set_scale(WPE_SCREEN(self->screen), self->scale);
        wpe_screen_set_refresh_rate(WPE_SCREEN(self->screen), self->refresh_hz * 1000);
        /* WebKit's ScreenManager::collectScreenProperties divides by the
         * screen's physical diagonal-in-mm to compute DPI; if both physical
         * dimensions are zero the result is +inf and WTFCrash fires from
         * inside WebPageProxy::launchProcess. Pick a 96-DPI-equivalent size
         * derived from the pixel viewport so the DPI math lands at 96. */
        int phys_w_mm = (int)(self->viewport_w * 25.4 / 96.0);
        int phys_h_mm = (int)(self->viewport_h * 25.4 / 96.0);
        if (phys_w_mm <= 0) phys_w_mm = 300;
        if (phys_h_mm <= 0) phys_h_mm = 200;
        wpe_screen_set_physical_size(WPE_SCREEN(self->screen), phys_w_mm, phys_h_mm);
    }
    return WPE_SCREEN(self->screen);
}

static void buffr_display_init(BuffrDisplay *self) {
    self->egl_display = NULL;
    self->viewport_w = 1280;
    self->viewport_h = 720;
    self->scale = 1.0;
    self->refresh_hz = 60;
    self->screen = NULL;
    self->drm_device = NULL;
}

static void buffr_display_dispose(GObject *object) {
    BuffrDisplay *self = BUFFR_DISPLAY(object);
    g_clear_object(&self->screen);
    g_clear_pointer(&self->drm_device, wpe_drm_device_unref);
    G_OBJECT_CLASS(buffr_display_parent_class)->dispose(object);
}

static void buffr_display_class_init(BuffrDisplayClass *klass) {
    GObjectClass *object_class = G_OBJECT_CLASS(klass);
    object_class->dispose = buffr_display_dispose;

    WPEDisplayClass *display_class = WPE_DISPLAY_CLASS(klass);
    display_class->connect = buffr_display_connect_vfunc;
    display_class->create_view = buffr_display_create_view_vfunc;
    display_class->create_toplevel = buffr_display_create_toplevel_vfunc;
    display_class->get_egl_display = buffr_display_get_egl_vfunc;
    display_class->get_drm_device = buffr_display_get_drm_device_vfunc;
    /* Expose one screen w/ non-zero physical size so AcceleratedBackingStore
     * can allocate frame buffers against it. The DPI-from-physical-mm WTFCrash
     * in ScreenManager::collectScreenProperties is dodged by computing physical
     * mm from the pixel viewport at 96 DPI in buffr_display_get_screen_vfunc. */
    display_class->get_n_screens = buffr_display_get_n_screens_vfunc;
    display_class->get_screen = buffr_display_get_screen_vfunc;
}

/* Public constructor exposed to Rust. */
WPEDisplay *buffr_display_new(gpointer egl_display,
                              int viewport_w,
                              int viewport_h,
                              double scale,
                              int refresh_hz) {
    BuffrDisplay *self = g_object_new(BUFFR_TYPE_DISPLAY, NULL);
    self->egl_display = egl_display;
    self->viewport_w = viewport_w;
    self->viewport_h = viewport_h;
    self->scale = scale > 0 ? scale : 1.0;
    self->refresh_hz = refresh_hz > 0 ? refresh_hz : 60;
    return WPE_DISPLAY(self);
}

/* Re-export the toplevel/view GType getters so Rust can pass them to
 * g_object_new directly if it ever needs to (the default flow goes through
 * the display's create_toplevel/create_view vmethods). */
GType buffr_display_get_view_type(void) { return BUFFR_TYPE_VIEW; }
GType buffr_display_get_toplevel_type(void) { return BUFFR_TYPE_TOPLEVEL; }
