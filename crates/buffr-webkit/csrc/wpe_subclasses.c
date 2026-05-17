/*
 * wpe_subclasses.c — GObject subclasses for buffr-webkit's WPE platform path.
 *
 * Defines four final GObject subclasses (BuffrDisplay/View/Toplevel/Screen)
 * using upstream WebKit's WPE platform classes as parents. The C side is the
 * boilerplate registration that bindgen can't faithfully reproduce (the
 * *Class struct layouts are needed for setting vmethod function pointers).
 *
 * Also defines BuffrDisplayWayland (#152): a WPEDisplay subclass that reuses
 * the host application's Wayland connection instead of opening its own. This
 * solves the cross-client wl_subsurface problem that blocked Phase 3 with
 * stock WPEDisplayWayland.
 *
 * Per-frame work happens in Rust: `buffr_view_render_buffer` (this file's
 * BuffrView vmethod) forwards each delivered WPEBuffer* to the Rust callback
 * `buffr_rust_render_buffer`, which decodes pixels into the shared OsrFrame.
 *
 * Build: compiled by build.rs via the cc crate, linked into buffr-webkit
 * alongside the bindgen-generated FFI bindings.
 */

#include <wpe/wpe-platform.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>

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
    BuffrDisplay *self = BUFFR_DISPLAY(display);
    WPEToplevel *tl = WPE_TOPLEVEL(g_object_new(BUFFR_TYPE_TOPLEVEL,
                                                 "display", display,
                                                 "max-views", max_views,
                                                 NULL));
    /* WPE's default toplevel is 1024x768; mark it sized to the host
     * viewport so WebKit paints at the dims buffr-app expects. */
    if (self->viewport_w > 0 && self->viewport_h > 0)
        wpe_toplevel_resize(tl, self->viewport_w, self->viewport_h);
    return tl;
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

/* ── BuffrDisplayWayland (#152) ─────────────────────────────────────────── */
/*
 * WPEDisplay subclass that reuses an existing Wayland connection supplied by
 * buffr-app instead of opening its own.  This eliminates the cross-client
 * wl_subsurface problem that blocked Phase 3 with stock WPEDisplayWayland.
 *
 * The display borrows all Wayland object pointers — they are owned by winit
 * for the lifetime of the host process.  EGL is initialised here via
 * eglGetPlatformDisplay(EGL_PLATFORM_WAYLAND_KHR, wl_display, NULL).
 *
 * create_view stubs out to a BuffrView (the existing OSR view) WITHOUT
 * attaching a ViewCtx so WebKit gets a valid WPEView object but the OSR
 * render-buffer ingest path never fires.  #153 will replace this stub with
 * a proper BuffrViewWayland that composites into a wl_subsurface.
 */

#define BUFFR_TYPE_DISPLAY_WAYLAND (buffr_display_wayland_get_type())
G_DECLARE_FINAL_TYPE(BuffrDisplayWayland, buffr_display_wayland, BUFFR, DISPLAY_WAYLAND, WPEDisplay)

struct _BuffrDisplayWayland {
    WPEDisplay parent_instance;

    /* Wayland objects — borrowed from buffr-app / winit. Valid for the
     * lifetime of the host process; we never own or free these. */
    struct wl_display      *wl_display;
    struct wl_compositor   *wl_compositor;
    struct wl_subcompositor *wl_subcompositor;
    struct wl_surface      *parent_surface;

    /* EGL display created here from the host wl_display.  Valid as long as
     * eglInitialize succeeded; NULL on failure (falls back to OSR in Rust). */
    EGLDisplay egl_display;

    /* Viewport / screen configuration. */
    int    viewport_w;
    int    viewport_h;
    double scale;
    int    refresh_hz;

    /* Lazily-created single screen. */
    BuffrScreen *screen;

    /* Stash for the most-recently-created WPEView so Rust can retrieve it
     * after webkit_web_view_new (mirrors BuffrDisplay). */
    GMutex  last_view_mutex;
    WPEView *last_created_view;
};

G_DEFINE_FINAL_TYPE(BuffrDisplayWayland, buffr_display_wayland, WPE_TYPE_DISPLAY)

/* ── Stash accessor ─────────────────────────────────────────────────────── */

WPEView *buffr_display_wayland_take_last_view(BuffrDisplayWayland *self) {
    g_mutex_lock(&self->last_view_mutex);
    WPEView *v = self->last_created_view;
    self->last_created_view = NULL;
    g_mutex_unlock(&self->last_view_mutex);
    return v;
}

/* ── vmethods ───────────────────────────────────────────────────────────── */

static gboolean buffr_display_wayland_connect_vfunc(WPEDisplay *display, GError **error) {
    /* We don't own the wl_display — it is already connected by the host.
     * Nothing to do; always succeed. */
    (void)display;
    (void)error;
    return TRUE;
}

static gpointer buffr_display_wayland_get_egl_display_vfunc(WPEDisplay *display, GError **error) {
    (void)error;
    BuffrDisplayWayland *self = BUFFR_DISPLAY_WAYLAND(display);
    return self->egl_display;
}

static WPEView *buffr_display_wayland_create_view_vfunc(WPEDisplay *display) {
    /* Stub for #153: create a BuffrView (the existing OSR view GObject) so
     * WebKit gets a valid WPEView and construction succeeds.  We do NOT attach
     * a ViewCtx here — the OSR render-buffer ingest path will never fire for
     * this path.  #153 replaces this with BuffrViewWayland proper. */
    g_debug("buffr_display_wayland: create_view (stub — #153 pending)");
    WPEView *v = WPE_VIEW(g_object_new(BUFFR_TYPE_VIEW, "display", display, NULL));

    BuffrDisplayWayland *self = BUFFR_DISPLAY_WAYLAND(display);
    g_mutex_lock(&self->last_view_mutex);
    self->last_created_view = v;
    g_mutex_unlock(&self->last_view_mutex);

    return v;
}

static WPEToplevel *buffr_display_wayland_create_toplevel_vfunc(WPEDisplay *display, guint max_views) {
    BuffrDisplayWayland *self = BUFFR_DISPLAY_WAYLAND(display);
    WPEToplevel *tl = WPE_TOPLEVEL(g_object_new(BUFFR_TYPE_TOPLEVEL,
                                                 "display", display,
                                                 "max-views", max_views,
                                                 NULL));
    if (self->viewport_w > 0 && self->viewport_h > 0)
        wpe_toplevel_resize(tl, self->viewport_w, self->viewport_h);
    return tl;
}

static guint buffr_display_wayland_get_n_screens_vfunc(WPEDisplay *display) {
    (void)display;
    return 1;
}

static WPEScreen *buffr_display_wayland_get_screen_vfunc(WPEDisplay *display, guint index) {
    if (index != 0)
        return NULL;
    BuffrDisplayWayland *self = BUFFR_DISPLAY_WAYLAND(display);
    if (!self->screen) {
        /* Mirror the screen setup from BuffrDisplay: non-zero id to avoid the
         * WTFCrash in WebKit's ScreenManager (id=0 is the HashMap empty
         * sentinel), and non-zero physical size so the DPI math doesn't hit
         * +inf / WTFCrash in ScreenManager::collectScreenProperties. */
        self->screen = g_object_new(BUFFR_TYPE_SCREEN, "id", (guint32)1, NULL);
        wpe_screen_set_size(WPE_SCREEN(self->screen), self->viewport_w, self->viewport_h);
        wpe_screen_set_scale(WPE_SCREEN(self->screen), self->scale);
        wpe_screen_set_refresh_rate(WPE_SCREEN(self->screen), self->refresh_hz * 1000);
        int phys_w_mm = (int)(self->viewport_w * 25.4 / 96.0);
        int phys_h_mm = (int)(self->viewport_h * 25.4 / 96.0);
        if (phys_w_mm <= 0) phys_w_mm = 300;
        if (phys_h_mm <= 0) phys_h_mm = 200;
        wpe_screen_set_physical_size(WPE_SCREEN(self->screen), phys_w_mm, phys_h_mm);
    }
    return WPE_SCREEN(self->screen);
}

/* ── GObject lifecycle ──────────────────────────────────────────────────── */

static void buffr_display_wayland_init(BuffrDisplayWayland *self) {
    self->wl_display        = NULL;
    self->wl_compositor     = NULL;
    self->wl_subcompositor  = NULL;
    self->parent_surface    = NULL;
    self->egl_display       = EGL_NO_DISPLAY;
    self->viewport_w        = 1280;
    self->viewport_h        = 720;
    self->scale             = 1.0;
    self->refresh_hz        = 60;
    self->screen            = NULL;
    self->last_created_view = NULL;
    g_mutex_init(&self->last_view_mutex);
}

static void buffr_display_wayland_dispose(GObject *object) {
    BuffrDisplayWayland *self = BUFFR_DISPLAY_WAYLAND(object);
    g_clear_object(&self->screen);
    g_mutex_clear(&self->last_view_mutex);
    /* Note: we do NOT call eglTerminate here — the EGLDisplay belongs to the
     * host wl_display connection that outlives us. */
    G_OBJECT_CLASS(buffr_display_wayland_parent_class)->dispose(object);
}

static void buffr_display_wayland_class_init(BuffrDisplayWaylandClass *klass) {
    GObjectClass *object_class = G_OBJECT_CLASS(klass);
    object_class->dispose = buffr_display_wayland_dispose;

    WPEDisplayClass *display_class = WPE_DISPLAY_CLASS(klass);
    display_class->connect          = buffr_display_wayland_connect_vfunc;
    display_class->create_view      = buffr_display_wayland_create_view_vfunc;
    display_class->create_toplevel  = buffr_display_wayland_create_toplevel_vfunc;
    display_class->get_egl_display  = buffr_display_wayland_get_egl_display_vfunc;
    display_class->get_n_screens    = buffr_display_wayland_get_n_screens_vfunc;
    display_class->get_screen       = buffr_display_wayland_get_screen_vfunc;
    /* get_keymap: NULL → WebKit synthesises a keymap on demand. */
    /* get_drm_device: NULL → WebKit picks /dev/dri/renderD128 by default. */
}

/* ── Public constructor ─────────────────────────────────────────────────── */

/*
 * buffr_display_wayland_new — construct a BuffrDisplayWayland.
 *
 * Takes borrowed Wayland object pointers from the host winit window and
 * creates an EGL platform display from the host wl_display.  Returns NULL
 * when eglInitialize fails so the Rust caller can fall back to OSR.
 *
 * All pointer arguments are borrowed; the caller (winit / buffr-app) owns
 * them for the lifetime of the host process.
 */
WPEDisplay *buffr_display_wayland_new(
    void *wl_display_ptr,
    void *wl_compositor_ptr,
    void *wl_subcompositor_ptr,
    void *parent_surface_ptr,
    int viewport_w,
    int viewport_h,
    double scale,
    int refresh_hz)
{
    if (!wl_display_ptr) {
        g_warning("buffr_display_wayland_new: wl_display is NULL — refusing to construct");
        return NULL;
    }

    /* Initialise EGL using the host wl_display.  We use
     * eglGetPlatformDisplay (EGL 1.5 / EGL_EXT_platform_base) so Mesa
     * picks the Wayland platform rather than guessing from the pointer type.
     * If the implementation only supports eglGetDisplay, fall back to that. */
    EGLDisplay egl_dpy = EGL_NO_DISPLAY;
    PFNEGLGETPLATFORMDISPLAYEXTPROC eglGetPlatformDisplayEXT =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (eglGetPlatformDisplayEXT) {
        egl_dpy = eglGetPlatformDisplayEXT(EGL_PLATFORM_WAYLAND_KHR,
                                           wl_display_ptr, NULL);
    }
    if (egl_dpy == EGL_NO_DISPLAY) {
        /* Fallback: standard eglGetDisplay with the wl_display pointer cast
         * to EGLNativeDisplayType.  Works on most Mesa Wayland drivers. */
        egl_dpy = eglGetDisplay((EGLNativeDisplayType)wl_display_ptr);
    }
    if (egl_dpy == EGL_NO_DISPLAY) {
        g_warning("buffr_display_wayland_new: eglGetDisplay returned EGL_NO_DISPLAY");
        return NULL;
    }

    EGLint major = 0, minor = 0;
    if (!eglInitialize(egl_dpy, &major, &minor)) {
        g_warning("buffr_display_wayland_new: eglInitialize failed (error 0x%x)",
                  (unsigned)eglGetError());
        return NULL;
    }
    g_debug("buffr_display_wayland: EGL %d.%d initialised on wl_display=%p",
            major, minor, wl_display_ptr);

    BuffrDisplayWayland *self = g_object_new(BUFFR_TYPE_DISPLAY_WAYLAND, NULL);
    self->wl_display        = (struct wl_display *)wl_display_ptr;
    self->wl_compositor     = (struct wl_compositor *)wl_compositor_ptr;
    self->wl_subcompositor  = (struct wl_subcompositor *)wl_subcompositor_ptr;
    self->parent_surface    = (struct wl_surface *)parent_surface_ptr;
    self->egl_display       = egl_dpy;
    self->viewport_w        = viewport_w > 0  ? viewport_w  : 1280;
    self->viewport_h        = viewport_h > 0  ? viewport_h  : 720;
    self->scale             = scale > 0.0     ? scale        : 1.0;
    self->refresh_hz        = refresh_hz > 0  ? refresh_hz   : 60;

    return WPE_DISPLAY(self);
}
