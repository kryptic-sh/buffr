//! Wayland registry roundtrip to bind `wl_compositor` and `wl_subcompositor`
//! from an *existing* `wl_display` connection owned by winit (#151).
//!
//! This module exists only on Linux.  It uses raw FFI against
//! `libwayland-client.so` — no Rust wayland-client crate — so it doesn't
//! require a new direct dependency.  `libwayland-client` is already loaded by
//! winit's Wayland backend; the symbols are guaranteed present when we reach
//! this code path.
//!
//! # Safety contract
//!
//! Every `unsafe` block here follows the Wayland client API invariants:
//!
//! - `wl_display` pointer is valid for the process lifetime (winit holds it).
//! - The registry roundtrip completes synchronously; the listener callback
//!   fires on the calling thread only during `wl_display_roundtrip`.
//! - Bound globals (`wl_compositor`, `wl_subcompositor`) are valid from
//!   construction until `wl_proxy_destroy` — we hand them to the caller
//!   without destroying them; lifetime is tied to `wl_display`.

use std::ffi::{CStr, c_void};
use std::os::raw::{c_char, c_int, c_uint};

// ── minimal wl_interface / wl_message types ──────────────────────────────────
//
// We only need the struct layout so libwayland can identify the interface name
// and version when dispatching `wl_registry_bind`.  Requests/events arrays are
// empty — we never use the auto-generated dispatch path, only `wl_proxy_*` calls.

#[repr(C)]
struct WlMessage {
    name: *const c_char,
    signature: *const c_char,
    types: *const *const WlInterface,
}

#[repr(C)]
struct WlInterface {
    name: *const c_char,
    version: c_int,
    request_count: c_int,
    requests: *const WlMessage,
    event_count: c_int,
    events: *const WlMessage,
}

unsafe impl Sync for WlInterface {}
unsafe impl Send for WlInterface {}

/// Minimal `wl_interface` for `wl_compositor` (version 6 is current; we bind ≥4).
static WL_COMPOSITOR_INTERFACE: WlInterface = WlInterface {
    name: c"wl_compositor".as_ptr(),
    version: 6,
    request_count: 0,
    requests: std::ptr::null(),
    event_count: 0,
    events: std::ptr::null(),
};

/// Minimal `wl_interface` for `wl_subcompositor` (version 1 is the only version).
static WL_SUBCOMPOSITOR_INTERFACE: WlInterface = WlInterface {
    name: c"wl_subcompositor".as_ptr(),
    version: 1,
    request_count: 0,
    requests: std::ptr::null(),
    event_count: 0,
    events: std::ptr::null(),
};

/// Minimal `wl_interface` for `wl_registry` (needed to create the registry proxy).
static WL_REGISTRY_INTERFACE: WlInterface = WlInterface {
    name: c"wl_registry".as_ptr(),
    version: 1,
    request_count: 0,
    requests: std::ptr::null(),
    event_count: 0,
    events: std::ptr::null(),
};

// ── wl_argument union ─────────────────────────────────────────────────────────

#[repr(C)]
union WlArgument {
    i: i32,
    u: u32,
    #[allow(dead_code)]
    f: i32, // wl_fixed_t
    s: *const c_char,
    o: *const c_void,
    n: u32,
    #[allow(dead_code)]
    h: i32, // fd
}

// ── libwayland-client extern declarations ─────────────────────────────────────

#[link(name = "wayland-client")]
unsafe extern "C" {
    /// Dispatch all queued events and flush, returning after one round trip.
    fn wl_display_roundtrip(display: *mut c_void) -> c_int;

    /// Attach a listener struct to a proxy.
    fn wl_proxy_add_listener(
        proxy: *mut c_void,
        implementation: *mut unsafe extern "C" fn(),
        data: *mut c_void,
    ) -> c_int;

    /// Low-level request marshal that creates a new proxy with a versioned
    /// interface (used for `wl_display.get_registry` and `wl_registry.bind`).
    fn wl_proxy_marshal_array_constructor_versioned(
        proxy: *mut c_void,
        opcode: u32,
        args: *mut WlArgument,
        interface: *const WlInterface,
        version: u32,
    ) -> *mut c_void;

    /// Destroy a proxy without sending a protocol request (just drops the
    /// client-side object).
    fn wl_proxy_destroy(proxy: *mut c_void);
}

// ── wl_registry_listener ─────────────────────────────────────────────────────

/// C-compatible listener struct for `wl_registry`.
///
/// `global` fires once per advertised global when we call
/// `wl_display_roundtrip` after attaching the listener.
/// `global_remove` fires when a global disappears; we don't care.
#[repr(C)]
struct WlRegistryListener {
    global: unsafe extern "C" fn(
        data: *mut c_void,
        registry: *mut c_void,
        name: c_uint,
        interface: *const c_char,
        version: c_uint,
    ),
    global_remove: unsafe extern "C" fn(data: *mut c_void, registry: *mut c_void, name: c_uint),
}

/// Mutable state threaded through the registry listener as `user_data`.
struct BindState {
    compositor: *mut c_void,
    subcompositor: *mut c_void,
}

/// `wl_registry.global` callback.  Binds `wl_compositor` (≥4) and
/// `wl_subcompositor` (=1) when encountered.
///
/// # Safety
/// Called by libwayland with valid `data`, `registry`, `interface` pointers.
unsafe extern "C" fn on_global(
    data: *mut c_void,
    registry: *mut c_void,
    name: c_uint,
    interface: *const c_char,
    version: c_uint,
) {
    if data.is_null() || interface.is_null() {
        return;
    }

    // SAFETY: `data` is a `*mut BindState` we passed as user_data.
    let state = unsafe { &mut *(data as *mut BindState) };

    // SAFETY: `interface` is a null-terminated string from the compositor.
    let iface = unsafe { CStr::from_ptr(interface) };

    // `wl_registry_bind` opcode is 0.
    // Args for bind: [name: u32, interface_name: *const c_char, version: u32, new_id: null]
    // wl_proxy_marshal_array_constructor_versioned handles the new_id typed-arg expansion.

    if iface.to_bytes() == b"wl_compositor" && state.compositor.is_null() {
        let bind_version = version.min(4); // cap at v4; v4 introduced wl_surface.offset
        let mut args = [
            WlArgument { u: name },
            WlArgument {
                s: WL_COMPOSITOR_INTERFACE.name,
            },
            WlArgument { u: bind_version },
            WlArgument {
                o: std::ptr::null(),
            },
        ];
        // SAFETY: registry is a valid wl_proxy; args are correctly laid out.
        let proxy = unsafe {
            wl_proxy_marshal_array_constructor_versioned(
                registry,
                0,
                args.as_mut_ptr(),
                &WL_COMPOSITOR_INTERFACE,
                bind_version,
            )
        };
        state.compositor = proxy;
    } else if iface.to_bytes() == b"wl_subcompositor" && state.subcompositor.is_null() {
        let bind_version = version.min(1);
        let mut args = [
            WlArgument { u: name },
            WlArgument {
                s: WL_SUBCOMPOSITOR_INTERFACE.name,
            },
            WlArgument { u: bind_version },
            WlArgument {
                o: std::ptr::null(),
            },
        ];
        // SAFETY: registry is a valid wl_proxy; args are correctly laid out.
        let proxy = unsafe {
            wl_proxy_marshal_array_constructor_versioned(
                registry,
                0,
                args.as_mut_ptr(),
                &WL_SUBCOMPOSITOR_INTERFACE,
                bind_version,
            )
        };
        state.subcompositor = proxy;
    }
}

/// `wl_registry.global_remove` callback — no-op.
unsafe extern "C" fn on_global_remove(_data: *mut c_void, _registry: *mut c_void, _name: c_uint) {}

static REGISTRY_LISTENER: WlRegistryListener = WlRegistryListener {
    global: on_global,
    global_remove: on_global_remove,
};

// ── Public API ────────────────────────────────────────────────────────────────

/// Bind `wl_compositor` and `wl_subcompositor` from `wl_display` via a
/// synchronous registry roundtrip.
///
/// Returns `(wl_compositor*, wl_subcompositor*)`.  Either pointer may be null
/// if the global was not advertised.  Callers must treat null as a
/// deferred-to-#152 situation and proceed with only the display/surface handles.
///
/// # Safety
///
/// `wl_display` must be a valid `*mut wl_display` for the lifetime of the call.
/// The returned proxies are valid until the display is destroyed.
pub(crate) unsafe fn bind_compositor_globals(
    wl_display: *mut c_void,
) -> (*mut c_void, *mut c_void) {
    // ── 1. wl_display.get_registry (opcode 1) ────────────────────────────────
    //
    // `wl_display` is itself a proxy; its opcode 1 is `get_registry`.
    // The single arg is the new_id for the registry proxy.
    let mut get_registry_args = [WlArgument { n: 0 }];
    // SAFETY: wl_display is a valid proxy; args and interface are correct.
    let registry = unsafe {
        wl_proxy_marshal_array_constructor_versioned(
            wl_display,
            1, // WL_DISPLAY_GET_REGISTRY
            get_registry_args.as_mut_ptr(),
            &WL_REGISTRY_INTERFACE,
            1,
        )
    };
    if registry.is_null() {
        tracing::warn!("wayland_globals: wl_display.get_registry returned NULL");
        return (std::ptr::null_mut(), std::ptr::null_mut());
    }

    // ── 2. Attach listener ────────────────────────────────────────────────────
    let mut state = BindState {
        compositor: std::ptr::null_mut(),
        subcompositor: std::ptr::null_mut(),
    };
    // SAFETY: REGISTRY_LISTENER is a static; `&state` is valid for this fn's stack frame.
    // `wl_proxy_add_listener` stores the pointers; the listener fires synchronously
    // within `wl_display_roundtrip` below, before this frame returns.
    let ret = unsafe {
        wl_proxy_add_listener(
            registry,
            &REGISTRY_LISTENER as *const WlRegistryListener as *mut unsafe extern "C" fn(),
            std::ptr::addr_of_mut!(state) as *mut c_void,
        )
    };
    if ret != 0 {
        tracing::warn!("wayland_globals: wl_proxy_add_listener failed (ret={ret})");
        // SAFETY: registry is valid; destroy it before returning.
        unsafe { wl_proxy_destroy(registry) };
        return (std::ptr::null_mut(), std::ptr::null_mut());
    }

    // ── 3. Roundtrip — all global callbacks fire here ─────────────────────────
    // SAFETY: wl_display is valid; listener has been attached.
    let n = unsafe { wl_display_roundtrip(wl_display) };
    tracing::debug!("wayland_globals: roundtrip dispatched {n} events");

    // ── 4. Release registry proxy (we don't need further events) ─────────────
    // SAFETY: registry is valid.
    unsafe { wl_proxy_destroy(registry) };

    (state.compositor, state.subcompositor)
}
