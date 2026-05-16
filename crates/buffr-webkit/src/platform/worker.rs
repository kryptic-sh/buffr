//! GLib worker thread for the WPE WebKit backend.
//!
//! # Threading model
//!
//! ```text
//! WebKitEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  wpe_thread (mpsc::SyncSender)
//!                                ├─ glib::MainLoop::run()
//!                                ├─ timeout_add_local (10 ms poll) ──▶ drain commands
//!                                └─ WpeRuntime (WebView + FDO exportable)
//!   engine_state ──────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by GLib signal handlers on the worker thread)
//! ```
//!
//! WPE WebKit has no process-wide default MainContext ownership concept
//! (unlike GTK4), so the worker creates its own `glib::MainLoop::new(None, false)`
//! and immediately owns it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use super::error::WebKitError;
use super::ffi::*;
use super::runtime::{TabInfo, WpeRuntime};

// ── EngineState ───────────────────────────────────────────────────────────────

/// Thread-safe engine snapshot. Updated by the worker thread; read from any
/// thread behind `Mutex<EngineState>`.
pub(crate) struct EngineState {
    /// Open tabs in strip order.
    pub tabs: Vec<TabInfo>,
    /// Index of the active tab.
    pub active_idx: Option<usize>,
    /// Next tab id counter.
    pub next_id: u64,
    /// Current viewport width.
    pub width: u32,
    /// Current viewport height.
    pub height: u32,
    /// Set by signal handlers to signal a URL/title change.
    pub address_changed: bool,
    /// OSR sleep flag: true = skip repaints.
    pub osr_sleeping: Arc<AtomicBool>,
    /// Cached audio-active state (polled on the worker thread).
    pub audio_active: Arc<AtomicBool>,
}

impl EngineState {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            tabs: Vec::new(),
            active_idx: None,
            next_id: 1,
            width,
            height,
            address_changed: false,
            osr_sleeping: Arc::new(AtomicBool::new(false)),
            audio_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn active_tab_info(&self) -> Option<&TabInfo> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    pub(crate) fn tabs_summary(&self) -> Vec<TabSummary> {
        self.tabs.iter().map(|t| t.to_summary()).collect()
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

pub(crate) enum Command {
    OpenTab {
        url: String,
        reply: mpsc::SyncSender<Result<TabId, String>>,
    },
    Navigate {
        url: String,
    },
    Resize {
        width: u32,
        height: u32,
    },
    OsrResize {
        width: u32,
        height: u32,
    },
    KeyEvent {
        ev: WpeKeyEvent,
    },
    MouseMove {
        x: i32,
        y: i32,
        modifiers: u32,
    },
    MouseClick {
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    MouseWheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    },
    Focus {
        focused: bool,
    },
    OsrSleep {
        sleep: bool,
    },
    Shutdown,
}

/// Neutral keyboard event carried over the command channel.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WpeKeyEvent {
    pub key_code: u32,
    pub hardware_key_code: u32,
    pub pressed: bool,
    pub modifiers: u32,
}

// ── WorkerHandle ──────────────────────────────────────────────────────────────

/// Handle to the GLib worker thread.
pub(crate) struct WorkerHandle {
    pub cmd_tx: mpsc::SyncSender<Command>,
    pub engine_state: Arc<Mutex<EngineState>>,
    _thread: thread::JoinHandle<()>,
}

/// Spawn the GLib worker thread and return a handle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    initial_url: &str,
    width: u32,
    height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
) -> Result<WorkerHandle, WebKitError> {
    // ── Initialise wpe_loader (must happen before any WPE calls) ─────────────
    //
    // `wpe_loader_init` is safe to call multiple times; subsequent calls are
    // no-ops. On Arch Linux the backend SO is `libWPEBackend-fdo-1.0.so`.
    //
    // SAFETY: string literal is valid UTF-8 and null-terminated.
    let ok = unsafe { wpe_loader_init(b"libWPEBackend-fdo-1.0.so\0".as_ptr() as *const _) };
    if !ok {
        return Err(WebKitError::InitFailed(
            "wpe_loader_init(WPEBackend-fdo-1.0) failed".into(),
        ));
    }
    tracing::info!("webkit: wpe_loader_init OK");

    let engine_state = Arc::new(Mutex::new(EngineState::new(width, height)));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Command>(64);

    let initial_url = initial_url.to_owned();
    let es = Arc::clone(&engine_state);

    let thread = thread::Builder::new()
        .name("buffr-webkit-worker".into())
        .spawn(move || {
            tracing::info!("webkit worker: starting GLib main loop");

            // Build the GLib main loop bound to this thread's default context.
            let main_loop = glib::MainLoop::new(None, false);

            let mut runtime =
                WpeRuntime::new(Arc::clone(&frame), Arc::clone(&view), Arc::clone(&es));

            // Open the initial tab.
            if let Err(e) = runtime.open_tab(&initial_url) {
                tracing::error!("webkit worker: initial open_tab failed: {e}");
            }

            use std::cell::RefCell;
            use std::rc::Rc;
            let runtime_rc = Rc::new(RefCell::new(runtime));

            // ── Audio-activity poll (500 ms) ──────────────────────────────
            {
                let rt = Rc::clone(&runtime_rc);
                let es_audio = Arc::clone(&es);
                glib::timeout_add_local(Duration::from_millis(500), move || {
                    let any = rt.borrow().any_audio_active();
                    if let Ok(st) = es_audio.lock() {
                        st.audio_active.store(any, Ordering::Relaxed);
                    }
                    glib::ControlFlow::Continue
                });
            }

            // ── Command poll (10 ms) ─────────────────────────────────────
            {
                let ml = main_loop.clone();
                let rt = Rc::clone(&runtime_rc);
                glib::timeout_add_local(Duration::from_millis(10), move || {
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(cmd) => {
                                if handle_command(cmd, &mut rt.borrow_mut(), &ml) {
                                    return glib::ControlFlow::Break;
                                }
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                return glib::ControlFlow::Continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                tracing::info!("webkit worker: command channel closed");
                                ml.quit();
                                return glib::ControlFlow::Break;
                            }
                        }
                    }
                });
            }

            tracing::info!("webkit worker: entering GLib main loop");
            main_loop.run();
            tracing::info!("webkit worker: main loop exited");
        })
        .map_err(|e| WebKitError::InitFailed(format!("failed to spawn worker: {e}")))?;

    Ok(WorkerHandle {
        cmd_tx,
        engine_state,
        _thread: thread,
    })
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Returns `true` if the main loop should exit.
fn handle_command(cmd: Command, rt: &mut WpeRuntime, ml: &glib::MainLoop) -> bool {
    match cmd {
        Command::OpenTab { url, reply } => {
            let res = rt.open_tab(&url).map_err(|e| e.to_string());
            let _ = reply.try_send(res);
        }
        Command::Navigate { url } => {
            rt.navigate(&url);
        }
        Command::Resize { width, height } | Command::OsrResize { width, height } => {
            rt.resize(width, height);
        }
        Command::KeyEvent { ev } => {
            let wpe_ev = wpe_input_keyboard_event {
                time: timestamp_ms(),
                key_code: ev.key_code,
                hardware_key_code: ev.hardware_key_code,
                pressed: ev.pressed,
                modifiers: ev.modifiers,
            };
            rt.dispatch_keyboard(wpe_ev);
        }
        Command::MouseMove { x, y, modifiers } => {
            use super::ffi::wpe_input_pointer_event_type_wpe_input_pointer_event_type_motion as MOTION;
            let ev = wpe_input_pointer_event {
                type_: MOTION,
                time: timestamp_ms(),
                x,
                y,
                button: 0,
                state: 0,
                modifiers: translate_modifiers(modifiers),
            };
            rt.dispatch_pointer(ev);
        }
        Command::MouseClick {
            x,
            y,
            button,
            pressed,
            modifiers,
        } => {
            use super::ffi::wpe_input_pointer_event_type_wpe_input_pointer_event_type_button as BUTTON;
            let ev = wpe_input_pointer_event {
                type_: BUTTON,
                time: timestamp_ms(),
                x,
                y,
                button,
                state: if pressed { 1 } else { 0 },
                modifiers: translate_modifiers(modifiers),
            };
            rt.dispatch_pointer(ev);
        }
        Command::MouseWheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        } => {
            use super::ffi::wpe_input_axis_event_type_wpe_input_axis_event_type_motion as MOTION;
            // Dispatch two separate axis events for X and Y if non-zero.
            if delta_y != 0 {
                let ev = wpe_input_axis_event {
                    type_: MOTION,
                    time: timestamp_ms(),
                    x,
                    y,
                    axis: 0,         // vertical
                    value: -delta_y, // WPE: positive = scroll up = negative delta_y
                    modifiers: translate_modifiers(modifiers),
                };
                rt.dispatch_axis(ev);
            }
            if delta_x != 0 {
                let ev = wpe_input_axis_event {
                    type_: MOTION,
                    time: timestamp_ms(),
                    x,
                    y,
                    axis: 1, // horizontal
                    value: delta_x,
                    modifiers: translate_modifiers(modifiers),
                };
                rt.dispatch_axis(ev);
            }
        }
        Command::Focus { focused } => {
            if let Some(tab) = rt.tab.as_ref() {
                let backend = tab.wpe_backend();
                if !backend.is_null() {
                    unsafe {
                        use super::ffi::{
                            wpe_view_activity_state_wpe_view_activity_state_focused as FOCUSED,
                            wpe_view_activity_state_wpe_view_activity_state_in_window as IN_WINDOW,
                            wpe_view_activity_state_wpe_view_activity_state_visible as VISIBLE,
                        };
                        if focused {
                            wpe_view_backend_add_activity_state(
                                backend,
                                VISIBLE | FOCUSED | IN_WINDOW,
                            );
                        } else {
                            wpe_view_backend_remove_activity_state(backend, FOCUSED);
                        }
                    }
                }
            }
        }
        Command::OsrSleep { sleep } => {
            if let Ok(st) = rt.engine_state.lock() {
                st.osr_sleeping.store(sleep, Ordering::Relaxed);
            }
        }
        Command::Shutdown => {
            ml.quit();
            return true;
        }
    }
    false
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn timestamp_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis()
}

/// Map CEF EVENTFLAG_* bitmask → WPE `wpe_input_modifier`.
///
/// CEF flags (from `cef_event_flags_t`):
///   SHIFT=1<<1=2, CONTROL=1<<2=4, ALT=1<<3=8, META=1<<5=32
/// WPE modifiers:
///   control=1, shift=2, alt=4, meta=8
fn translate_modifiers(cef: u32) -> u32 {
    let mut wpe = 0u32;
    // EVENTFLAG_SHIFT_DOWN = 0x02
    if cef & 0x02 != 0 {
        wpe |= 2; // wpe shift
    }
    // EVENTFLAG_CONTROL_DOWN = 0x04
    if cef & 0x04 != 0 {
        wpe |= 1; // wpe control
    }
    // EVENTFLAG_ALT_DOWN = 0x08
    if cef & 0x08 != 0 {
        wpe |= 4; // wpe alt
    }
    // EVENTFLAG_COMMAND_DOWN = 0x20
    if cef & 0x20 != 0 {
        wpe |= 8; // wpe meta
    }
    wpe
}
