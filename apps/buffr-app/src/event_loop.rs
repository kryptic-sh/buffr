//! The `ApplicationHandler` implementation: buffr's event loop.
//!
//! Every platform event enters here -- window events, user events from
//! the engine and IPC, and the `about_to_wait` deadline computation
//! that decides when to sleep and for how long.
//!
//! This is the imperative counterpart to [`crate::paint_policy`]: the
//! decisions live there as pure functions, and this module applies
//! them to `AppState`.
//!
//! The `crate::*` glob is deliberate while `AppState` still lives in
//! `main.rs`: this impl reaches most of the crate root's surface, and
//! an explicit list would be both long and pure churn once `AppState`
//! moves out. Narrow it then.

use std::time::{Duration, Instant};

use crate::*;

/// Cap on live popup windows. A page that evades CEF's popup blocker
/// (a gesture-triggered chain, a popunder) must not grow unbounded
/// windows / GPU surfaces / fds. Matches the CEF-side pending-alloc
/// queue cap (`PENDING_POPUP_ALLOC_CAP` in buffr-cef's osr.rs), which
/// bounds the same flood before `on_after_created`.
const MAX_LIVE_POPUPS: usize = 32;

impl ApplicationHandler<BuffrUserEvent> for AppState {
    fn user_event(&mut self, event_loop: &mut EventLoop<BuffrUserEvent>, event: BuffrUserEvent) {
        // Heartbeat + shutdown check: fold the logic that was in new_events (winit)
        // here so it runs at the top of every user_event delivery (the earliest hook
        // wayr offers aside from about_to_wait).
        if self.check_shutdown(event_loop) {
            return;
        }
        self.tick_heartbeat();
        match event {
            BuffrUserEvent::Shutdown => {
                // ctrl+c handler set shutdown_flag and posted this event.
                // Exit directly here in addition to setting the flag —
                // relying on about_to_wait to fire after user_event has
                // a known failure mode on Wayland where the loop never
                // reaches it, leaving Ctrl+C stuck.
                tracing::debug!("user_event: Shutdown");
                self.save_session_now();
                self.mark_clean_shutdown();
                event_loop.exit();
            }
            BuffrUserEvent::OsrFrame => {
                tracing::trace!("user_event: OsrFrame -> request_redraw");
                self.request_redraw();
            }
            BuffrUserEvent::OsrFramePopup(browser_id) => {
                tracing::trace!(browser_id, "user_event: OsrFramePopup -> request_redraw");
                if let Some(&wid) = self.popup_window_id_by_browser.get(&browser_id)
                    && let Some(popup) = self.popups.get(&wid)
                {
                    popup.window.request_redraw();
                }
            }
            BuffrUserEvent::OpenUrls(urls) => {
                debug!(count = urls.len(), "single_instance: opened forwarded URLs");
                if let Some(host) = self.active_engine_dyn() {
                    for url in &urls {
                        if let Err(err) = host.open_tab_background(url) {
                            warn!(error = %err, %url, "single_instance: open_tab_background failed");
                        }
                    }
                } else {
                    // Browser not yet created — queue as pending tabs so they open on resumed.
                    self.pending_new_tabs.extend(urls.clone());
                }
                // Bring the window to the front via xdg_activation_v1 so the
                // user actually sees the forwarded tab(s). Best-effort: the
                // compositor will reject the request if it doesn't trust the
                // serial we attach (e.g. no recent user input on our surfaces
                // — likely when this process has been backgrounded for a
                // while). The downgrade-on-reject is benign; the tab is open.
                if let Some(window) = self.window.as_ref()
                    && let Err(err) = window.request_activation(event_loop)
                {
                    debug!(?err, "xdg_activation request skipped");
                }
            }
            BuffrUserEvent::ClipboardPasteText(text) => {
                let Some(text) = text else { return };
                if text.is_empty() {
                    return;
                }
                self.insert_text_via_exec(&text);
            }
        }
    }

    fn resumed(&mut self, event_loop: &mut EventLoop<BuffrUserEvent>) {
        if self.window.is_some() {
            return;
        }
        let window = match Toplevel::builder()
            .with_title(self.title_for(self.current_mode_label, &self.statusline.url))
            .with_app_id("sh.kryptic.buffr")
            .with_initial_size(crate::windowing::Size::new(1280, 800))
            .build(event_loop)
        {
            Ok(w) => w,
            Err(err) => {
                warn!(error = %err, "failed to create window");
                event_loop.exit();
                return;
            }
        };
        // Toplevel is not Send+Sync (Wayland objects are main-thread-only);
        // Arc is used only for shared ownership within the main thread.
        #[allow(clippy::arc_with_non_send_sync)]
        let window = Arc::new(window);

        // Pass the same page viewport used by later resize events so
        // CEF paints the first frame in the area below the tab strip and
        // above the statusline.
        let inner = window.physical_size();
        let (_cef_x, _cef_y, cef_w, cef_h) =
            self.cef_child_rect(inner.width.max(1), inner.height.max(1));

        // ── Phase 3: multi-engine registry ───────────────────────────────────
        //
        // Iterate over `engines_config.effective_instances()` and construct
        // one `BrowserHost` per instance. All instances share the same CEF
        // process init (`cef::initialize` is once-per-process) and the same
        // on-disk cache (per-engine cache isolation via `RequestContext` is
        // Phase 5+). The first successful instance becomes the active engine.
        // winit does not expose current_monitor/refresh_rate via OSR; default to 60 Hz.
        let display_hz: u32 = 60;
        let os_scale = window.scale_factor() as f32;
        let effective_scale = std::env::var("BUFFR_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(os_scale);
        let instances = self.engines_config.effective_instances().into_owned();
        let mut router_builder = engine_router::EngineRouter::builder()
            .default_engine(buffr_engine::EngineId::new(&self.engines_config.default));
        let mut first_instance = true;
        for inst in &instances {
            match inst.backend.as_str() {
                "cef" => {
                    // Only the first (active) engine gets the full sinks wired up.
                    // Additional instances share the same sinks so that hint/find/
                    // edit events route correctly regardless of which instance
                    // raised them. The popup sinks are also shared — `resumed` sets
                    // them once from the active host.
                    let counters_opt = if first_instance {
                        Some(self.counters.clone())
                    } else {
                        None
                    };
                    // Default: ~/.local/share/buffr/engines/<id>/ (XDG_DATA_HOME).
                    // Rooted in data_root, not cache_dir, because CEF stores
                    // persistent profile state (cookies, localStorage, IndexedDB)
                    // and ephemeral cache together — and the XDG spec says
                    // ~/.cache may be "lost without warning" (systemd-tmpfiles,
                    // tmpfs, cleanup tools).
                    //
                    // NOTE: the CEF backend currently DISCARDS this value — see
                    // `BrowserHost::new_with_options` and kryptic-sh/buffr#158.
                    // Every instance shares the process-global root_cache_path
                    // (which is `paths.data`). Kept wired so per-engine
                    // isolation is a one-line change once Alloy is replaced.
                    let data_dir_buf: Option<std::path::PathBuf> =
                        Some(match inst.data_dir.as_deref() {
                            Some(explicit) => std::path::PathBuf::from(explicit),
                            None => self.data_root.join("engines").join(&inst.id),
                        });
                    let options = BackendOpenOptions {
                        engine_id: buffr_engine::EngineId::new(&inst.id),
                        data_dir: data_dir_buf.as_deref(),
                        cache_dir: None,
                        initial_url: &self.homepage,
                        frame_rate: display_hz as i32,
                        device_scale: effective_scale as f64,
                        initial_size: (cef_w, cef_h),
                        private: self.private,
                        // CEF manages downloads via CefEngineSinks; these
                        // fields are for blink-cdp only.
                        history: None,
                        download_dir: None,
                        downloads: None,
                        notice_queue: None,
                        find_sink: None,
                        sinks: Box::new(CefEngineSinks {
                            history: self.history.clone(),
                            downloads: self.downloads.clone(),
                            downloads_config: self.downloads_config.clone(),
                            zoom: self.zoom.clone(),
                            permissions: self.permissions.clone(),
                            notice_queue: self.download_notice_queue.clone(),
                            find_sink: self.find_sink.clone(),
                            hint_sink: self.hint_sink.clone(),
                            edit_sink: self.edit_sink.clone(),
                            hint_alphabet: self.hint_alphabet.clone(),
                            counters: counters_opt,
                            show_favicons: self.show_favicons,
                        }),
                        prefer_native: false,
                        wayland_handles: None,
                        internal_server: self.internal_server.as_ref().map(Arc::clone),
                    };
                    match self.backend.open_engine(options) {
                        Ok(host_dyn) => {
                            info!(engine_id = %inst.id, "browser host created (OSR)");
                            host_dyn.osr_focus(true);
                            if first_instance {
                                // Wire popup sinks and OSR wake on the active engine.
                                self.popup_create_sink = host_dyn.popup_create_sink();
                                self.popup_close_sink = host_dyn.popup_close_sink();
                            }
                            // Every engine gets its own OSR wake — a single closure
                            // that calls `request_redraw`. The paint pass reads the
                            // active engine's frame, so wakes from inactive engines
                            // may overdraw slightly but are always correct.
                            let proxy = self.event_proxy.clone();
                            host_dyn.set_osr_wake(Arc::new(move || {
                                let _ = proxy.send_event(BuffrUserEvent::OsrFrame);
                            }));
                            host_dyn.set_frame_rate(display_hz);
                            host_dyn.set_device_scale(effective_scale);
                            let engine_id = buffr_engine::EngineId::new(&inst.id);
                            router_builder =
                                router_builder.register(engine_id.clone(), Arc::clone(&host_dyn));
                            self.engines.insert(engine_id.clone(), host_dyn);
                            if first_instance {
                                self.active_engine = engine_id;
                                tracing::debug!(
                                    os_scale,
                                    effective_scale,
                                    display_hz,
                                    "initial device scale + frame rate applied to active engine",
                                );
                            }
                            first_instance = false;
                        }
                        Err(err) => {
                            warn!(engine_id = %inst.id, error = %err, "failed to create browser host");
                        }
                    }
                }
                other => {
                    warn!(backend = %other, engine_id = %inst.id, "unknown engine backend — skipping");
                }
            }
        }
        if self.engines.is_empty() {
            warn!("no engine instances constructed — browser will not function");
        }
        // Add routing rules and build the router.
        for rule in &self.engines_config.rules {
            router_builder = router_builder.rule(rule.pattern.clone(), rule.engine.clone());
        }
        match router_builder.build() {
            Ok(router) => {
                tracing::debug!("engine router built ({} engine(s))", {
                    router.engine_ids().count()
                });
                self.engine_router = Some(Arc::new(router));
            }
            Err(err) => {
                tracing::warn!(error = %err, "engine router build failed — tab-spawn will fall back to active engine directly");
            }
        }

        // Initialise wgpu renderer. On failure, log and exit — there is
        // no CPU-only fallback in this code path. Pass an Arc clone so
        // wgpu holds its own ref and the Toplevel survives wgpu's
        // Surface drop (fixes the shutdown SIGSEGV that the leak
        // workaround in run_browser used to paper over).
        let win_size = window.physical_size();
        match crate::render::Renderer::new(Arc::clone(&window), (win_size.width, win_size.height)) {
            Ok(r) => self.renderer = Some(r),
            Err(err) => {
                warn!(error = %err, "wgpu renderer init failed");
                // Under --smoke-test this must be a hard failure. The
                // smoke contract is "the windowing backend reached a
                // first paint"; exiting the event loop here would exit
                // 0 without ever painting, so a renderer regression --
                // or simply a runner with no Vulkan driver -- would
                // report a green, entirely vacuous pass.
                if SMOKE_TEST_ACTIVE.load(Ordering::SeqCst) {
                    eprintln!("smoke-test: wgpu renderer init failed: {err}");
                    std::process::exit(4);
                }
                event_loop.exit();
                return;
            }
        }

        // Schedule the find smoke-test dispatch for 1.5s after window
        // creation. This is a coarse "page is probably ready" timer
        // because we don't yet hook `OnLoadEnd` into the host.
        if self.pending_find.is_some() {
            self.find_smoke_at = Some(Instant::now() + Duration::from_millis(1500));
        }

        // Restore extra tabs from session + CLI now that the host
        // exists. The first session tab (if any) replaces the
        // homepage on tab 0; the rest open in the background.
        self.open_pending_tabs();
        self.refresh_tab_strip();

        // Construct the platform idle-inhibitor now that the window
        // exists. On macOS / Windows the pointers are ignored (IOKit /
        // SetThreadExecutionState). On Linux the Wayland inhibitor needs
        // live wl_display + wl_surface pointers; these were previously
        // extracted via wayr's FFI accessors. With winit as the sole
        // backend, the idle-inhibitor is parked (null pointers → the
        // Wayland backend returns InhibitError::PlatformError which we
        // warn and swallow). Re-wire when wayr returns or when winit
        // exposes the raw Wayland display/surface handle directly.
        let display_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let surface_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: null pointers are explicitly handled by the Linux
        // Wayland backend (returns PlatformError); macOS/Windows ignore them.
        match unsafe { new_inhibitor(display_ptr, surface_ptr) } {
            Ok(inh) => self.idle_inhibitor = Some(inh),
            Err(err) => {
                tracing::warn!(error = %err, "idle_inhibit: construction failed");
            }
        }

        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &mut EventLoop<BuffrUserEvent>,
        surface_id: SurfaceId,
        event: WindowEvent,
    ) {
        // Heartbeat stamp + shutdown on every window event.
        if self.check_shutdown(event_loop) {
            return;
        }
        self.tick_heartbeat();
        // Dispatch popup windows before the main window path.
        if self.popups.contains_key(&surface_id) {
            self.handle_popup_window_event(event_loop, surface_id, event);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested");
                self.save_session_now();
                self.mark_clean_shutdown();
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                tracing::trace!("redraw: RedrawRequested");
                self.paint_chrome();
                // Smoke-test mode: first RedrawRequested proves the
                // windowing backend reached steady state. Mark and
                // exit cleanly; the watchdog thread will be a no-op
                // when it eventually fires.
                if SMOKE_TEST_ACTIVE.load(Ordering::SeqCst) {
                    SMOKE_TEST_SAW_REDRAW.store(true, Ordering::SeqCst);
                    tracing::info!("smoke-test: RedrawRequested received; exiting 0");
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(new_size) => {
                // crate::windowing::Size is logical pixels. Get physical via window.physical_size().
                let phys = self
                    .window
                    .as_ref()
                    .map(|w| w.physical_size())
                    .unwrap_or_default();
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0);
                let (_x, _y, cef_w, cef_h) =
                    self.cef_child_rect(phys.width.max(1), phys.height.max(1));
                debug!(
                    new_w = new_size.width,
                    new_h = new_size.height,
                    phys_w = phys.width,
                    phys_h = phys.height,
                    scale,
                    cef_w,
                    cef_h,
                    has_host = !self.engines.is_empty(),
                    "wayr: Resized",
                );
                // Debounce CEF resize: arm/refresh the pending deadline rather
                // than calling host.osr_resize immediately. Hyprland fires many
                // Resized events per second during a drag; CEF only needs to learn
                // the final post-drag size. The renderer GPU-stretches the stale
                // OSR frame to fill the live browser_rect during the debounce window
                // so there is no visual regression. The actual osr_resize call is
                // fired in about_to_wait once the deadline elapses.
                self.pending_cef_resize
                    .arm(cef_w, cef_h, Instant::now(), CEF_RESIZE_DEBOUNCE);
                // Paint synchronously so the configure ack carries a
                // buffer matching this event's size. Hyprland (and other
                // wlroots compositors) anchor top-edge resize at the
                // cursor — the window bounds grow immediately and any
                // client-paint latency shows up as a letterbox at the
                // bottom of the window while the stale buffer is still
                // attached. With the GPU compositor a paint is ~1-2 ms
                // so doing it inline here is cheaper than the visible
                // lag coalescing produces.
                //
                // Pass `new_size` explicitly: `window.inner_size()` can
                // lag the event on Hyprland during a fast top-edge drag,
                // and if paint_chrome reads the stale value we present
                // a buffer smaller than the configured surface — the
                // compositor then fills the gap by replicating the
                // bottom edge of the buffer (statusline last row),
                // which reads as a "stretched" bottom bar.
                // Update the subsurface position BEFORE paint_chrome runs.
                // wl_subsurface.set_position is double-buffered against the
                // PARENT surface commit (applies on parent commit, not on
                // child commit, even in desync mode). paint_chrome below
                // commits the parent via wgpu present, so set_size must
                // queue the new position into the parent's pending state
                // first — otherwise the position update is one frame
                // behind and the subsurface tracks the previous resize.
                // paint_chrome_with calls sub.set_size internally before
                // the wgpu present, keeping the subsurface position synced
                // with the parent commit's buffer dims. Don't duplicate the
                // call here — both paths would do the same work.
                let w = phys.width.max(1);
                let h = phys.height.max(1);
                self.mark_chrome_dirty();
                // Option A (issue #17 / sleep integration): set surface_drifted
                // so the sleep guard in paint_chrome_with lets the resize paint
                // through even while OSR is paused.  Without this the compositor
                // keeps a stale-sized buffer and produces a persistent letterbox.
                self.surface_drifted = true;
                self.paint_chrome_with(Some((w, h)));
            }
            // WindowEvent::Moved — no Wayland equivalent; position is compositor-managed.
            // TODO(wayr): window position not exposed; saved value stays 0.
            WindowEvent::ScaleFactorChanged {
                new_scale_factor, ..
            } => {
                let scale_factor = new_scale_factor;
                debug!(scale_factor, "wayr: ScaleFactorChanged");
                {
                    // Fan out scale change to ALL registered engines so that
                    // inactive engines stay in sync when they become active.
                    let s = std::env::var("BUFFR_SCALE")
                        .ok()
                        .and_then(|v| v.parse::<f32>().ok())
                        .unwrap_or(scale_factor as f32);
                    for host in self.engines.values() {
                        host.set_device_scale(s);
                    }
                }
                // chrome_heights_physical now uses the new scale, so
                // browser_w/h on the next paint will differ from what
                // osr_view holds (last set at the OLD scale). Re-sync
                // osr_view dims so view_rect returns matching values
                // and CEF on_paint lands at the new browser_w/h.
                // Without this, monitor swaps with no follow-up Resized
                // leave the loading animation stuck.
                self.resync_cef_rect();
                // Force a chrome repaint so the chrome layer re-rasterizes.
                self.mark_chrome_dirty();
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            // ModifiersChanged — no wayr equivalent; modifiers ride inside each KeyEvent.
            // self.modifiers is updated from Key events directly.
            WindowEvent::Focused => {
                let mode = self.engine.lock().ok().map(|e| e.mode());
                tracing::info!(
                    edit_focus = ?self.edit_focus,
                    mode = ?mode,
                    "WindowEvent::Focused"
                );
                self.window_focused = true;
                // Force wake on focus regain. The present-latency
                // occlusion heuristic is reactive (2s probe interval)
                // and was the only wake signal after the winit → wayr
                // port — meaning a user who tabbed back into buffr saw
                // stale OSR pixels for up to two seconds while the
                // probe re-evaluated. Focus is an immediate
                // visibility signal; clear the occluded flag and
                // recompute paint policy so CEF gets `osr_sleep(false)`
                // and resumes painting on the next frame.
                if self.occluded {
                    tracing::debug!("focus: clearing heuristic occlusion");
                    self.occluded = false;
                    self.present_us_history.clear();
                    self.next_probe_at = None;
                    self.recompute_paint_policy();
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::Unfocused => {
                let mode = self.engine.lock().ok().map(|e| e.mode());
                tracing::info!(
                    edit_focus = ?self.edit_focus,
                    mode = ?mode,
                    "WindowEvent::Unfocused"
                );
                self.window_focused = false;
                // Don't touch paint policy here — losing keyboard
                // focus doesn't mean the window is hidden. The user
                // may have clicked another window while buffr is still
                // visible alongside it. The present-latency heuristic
                // still detects true occlusion (workspace switch,
                // minimize, fullscreen cover) via slow-frame samples;
                // we just don't pre-empt that by going to sleep on
                // every Alt-Tab.
            }
            WindowEvent::Occluded(occluded) => {
                // wayr 0.1.9+ surfaces xdg_toplevel.state.suspended as
                // an authoritative occlusion signal (xdg-shell v6+).
                // On v6 sessions this is more accurate than the
                // present-latency heuristic — fires the moment the
                // compositor marks the surface obscured (workspace
                // switch, minimize, opaque cover) instead of waiting
                // for the rolling window of slow frames to fill.
                //
                // The heuristic stays as fallback for v5 compositors
                // that never advertise Suspended; both signals feed the
                // same `self.occluded` flag so paint policy doesn't
                // need to know which source flipped it.
                tracing::debug!(occluded, "WindowEvent::Occluded (wayr)");
                if occluded {
                    // Arm the occlude → sleep debounce: commit occluded=true
                    // only when the grace window elapses without a reveal
                    // (see OCCLUDE_SLEEP_DEBOUNCE), so workspace-switch /
                    // overlay thrash doesn't emit spurious sleep/wake cycles.
                    self.sleep_deadline = Some(Instant::now() + OCCLUDE_SLEEP_DEBOUNCE);
                } else {
                    // Reveal: wake immediately — drop any pending debounce,
                    // invalidate any stale CEF OSR buffer that accumulated
                    // while we were sleeping, and clear the probe deadline
                    // so paint resumes immediately.
                    self.sleep_deadline = None;
                    self.occluded = false;
                    self.present_us_history.clear();
                    self.next_probe_at = None;
                }
                self.recompute_paint_policy();
                if !occluded && let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::PointerLeft => {
                if let Some(host) = self.active_engine_dyn() {
                    let mods = mods_to_cef(&self.modifiers);
                    host.osr_mouse_leave(mods);
                }
            }
            WindowEvent::PointerMoved { position } => {
                if let Some(host) = self.active_engine_dyn() {
                    // Convert from window coords to browser-region coords.
                    // osr_cursor tracks logical pixels for chrome hit-tests.
                    // CEF OSR consumes DIPs (logical pixels), forwarded directly.
                    // wayr PointerPosition wraps Position { x: i32, y: i32 } (logical).
                    let size = self
                        .window
                        .as_ref()
                        .map(|w| w.physical_size())
                        .unwrap_or_default();
                    let win_w = size.width.max(1);
                    let win_h = size.height.max(1);
                    let (_cx, cef_y, _cw, _ch) = self.cef_child_rect(win_w, win_h);
                    // position.0 is Position { x: i32, y: i32 } in logical pixels.
                    let phys_bx = position.0.x;
                    let phys_by = position.0.y.saturating_sub(cef_y as i32);
                    self.osr_cursor = (phys_bx, phys_by);

                    // Engine-badge tooltip: when the cursor moves onto a tab
                    // that carries an engine badge, write the engine id to the
                    // statusline's `engine_hint` cell so the user can identify
                    // which engine backs the tab without clicking. Cleared when
                    // the cursor is not on a badged tab.
                    {
                        let hovered_idx = self.hit_test_tab_strip();
                        let badge_engine = hovered_idx
                            .and_then(|idx| self.tab_strip.tabs.get(idx))
                            .filter(|t| t.engine_badge.is_some())
                            .and_then(|_| {
                                let router = self.engine_router.as_ref()?;
                                router
                                    .show_badges()
                                    .then(|| self.active_engine.as_str().to_owned())
                            });
                        if self.statusline.engine_hint != badge_engine {
                            self.statusline.engine_hint = badge_engine;
                            self.mark_chrome_dirty();
                            self.request_redraw();
                        }
                    }

                    // Context-menu hover: when the cursor is inside the
                    // menu panel, force the default arrow cursor, update
                    // the hovered row, and DON'T forward mouse-move to
                    // CEF — otherwise the page sees hover events and
                    // changes its cursor (text I-beam over text, link
                    // pointer over links, etc).
                    //
                    // Logical space, matching the paint site (M30): the panel
                    // is drawn into the logical chrome buffer, so a physical
                    // cursor never lands on it at scale != 1.
                    let hover_scale = self.current_scale();
                    let (lwidth, lheight) = logical_chrome_dims(win_w, win_h, hover_scale);
                    let (abs_x, abs_y) =
                        physical_cursor_to_dip(position.0.x, position.0.y, 0, hover_scale);
                    if let Some(cm) = self.context_menu.as_ref() {
                        let lw = lwidth as usize;
                        let lh = lheight as usize;
                        // Hit-test the cached entries — no overlay rebuild
                        // (label clones) per mouse move.
                        if cm.contains(lw, lh, abs_x, abs_y) {
                            // set_cursor needs event_loop but we don't have it here;
                            // cursor reset is a best-effort cosmetic — skip in this path.
                            // TODO(wayr): pass event_loop down to pump_cursor_changes.
                            if let Some(row) = cm.hit_test(lw, lh, abs_x, abs_y)
                                && let Some(cm_mut) = self.context_menu.as_mut()
                                && cm_mut.selected != row
                            {
                                cm_mut.selected = row;
                                self.mark_chrome_dirty();
                                self.request_redraw();
                            }
                            return;
                        }
                    }

                    // Logical (DIP) coords for CEF — route through helper.
                    let scale = self.current_scale();
                    let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, scale);
                    let mods = mods_to_cef(&self.modifiers) | self.osr_mouse_buttons;
                    host.osr_mouse_move(bx, by, mods);

                    // Promote to Visual the moment a left-button drag
                    // crosses the threshold — Chromium has already begun
                    // extending the page selection (see osr_mouse_buttons
                    // wiring), so the engine should reflect that without
                    // waiting for button-up.
                    if (self.osr_mouse_buttons & 16) != 0
                        && let Some((sx, sy)) = self.osr_drag_start
                    {
                        const DRAG_THRESHOLD_PX: i32 = 4;
                        let dx = (bx - sx).abs();
                        let dy = (by - sy).abs();
                        if dx > DRAG_THRESHOLD_PX || dy > DRAG_THRESHOLD_PX {
                            let already_visual = self
                                .engine
                                .lock()
                                .map(|e| e.mode() == PageMode::Visual)
                                .unwrap_or(true);
                            if !already_visual {
                                if let Ok(mut e) = self.engine.lock() {
                                    e.set_mode(PageMode::Visual);
                                }
                                self.refresh_title();
                            }
                            // Clear so MouseInput release path doesn't
                            // double-fire the Visual transition.
                            self.osr_drag_start = None;
                        }
                    }
                }
            }
            WindowEvent::PointerButton {
                state,
                button,
                modifiers,
            } => {
                use crate::windowing::PointerButton as MouseButton;
                use crate::windowing::PointerButtonState::Pressed;
                // Update cached modifier state from pointer event payload.
                self.modifiers = modifiers;
                tracing::trace!(?state, ?button, cursor = ?self.osr_cursor, "input: mouse_button");
                // Pinned-close confirmation hit-test: a left click on
                // the Yes / No button resolves the prompt. Anywhere else
                // is swallowed so the click can't reach the page or
                // the tab strip while a modal banner is up.
                if state == Pressed
                    && button == MouseButton::Left
                    && self.confirm_close_pinned.is_some()
                {
                    // The prompt is painted into the LOGICAL chrome buffer, so
                    // the hit-test has to run in logical space too — testing
                    // physical coords against it missed the buttons entirely
                    // on every HiDPI scale (M30).
                    let (px, py) = self.osr_cursor;
                    let size = self
                        .window
                        .as_ref()
                        .map(|w| w.physical_size())
                        .unwrap_or_default();
                    let win_w = size.width.max(1);
                    let win_h = size.height.max(1);
                    let scale = self.current_scale();
                    let (lwidth, lheight) = logical_chrome_dims(win_w, win_h, scale);
                    // osr_cursor is browser-region-relative physical; make it
                    // window-absolute, then convert to DIPs.
                    let phys_abs_y = py + self.cef_child_rect(win_w, win_h).1 as i32;
                    let (lx, ly) = physical_cursor_to_dip(px, phys_abs_y, 0, scale);
                    match hit_test_confirm_buttons(lwidth, lheight, lx, ly) {
                        Some(true) => {
                            self.resolve_pinned_close(true);
                            return;
                        }
                        Some(false) => {
                            self.resolve_pinned_close(false);
                            return;
                        }
                        None => {}
                    }
                    // Click missed the buttons — swallow the event so
                    // it doesn't fall through to tab-strip / page hit
                    // testing while the modal is open.
                    return;
                }
                // Context-menu click handling: any press dismisses the menu.
                // A click inside the menu area activates the hovered row
                // (if any); a click outside just dismisses.
                if self.context_menu.is_some() && state == Pressed {
                    let (px, py) = self.osr_cursor;
                    let size = self
                        .window
                        .as_ref()
                        .map(|w| w.physical_size())
                        .unwrap_or_default();
                    let win_w = size.width.max(1);
                    let win_h = size.height.max(1);
                    // The menu is painted into the LOGICAL chrome buffer
                    // (`cm.to_overlay(lwidth, lheight)`), so build and test the
                    // overlay in logical space as well — at scale 2 the
                    // physical-coordinate version missed the visible panel and
                    // fell into the "clicked outside → dismiss" branch (M30).
                    let scale = self.current_scale();
                    let (lwidth, lheight) = logical_chrome_dims(win_w, win_h, scale);
                    // osr_cursor is browser-region-relative; convert to
                    // full-window coords by adding the browser y-offset.
                    let cef_y_offset = self.cef_child_rect(win_w, win_h).1 as i32;
                    let (abs_x, abs_y) = physical_cursor_to_dip(px, py + cef_y_offset, 0, scale);
                    if let Some(cm) = self.context_menu.as_ref() {
                        let lw = lwidth as usize;
                        let lh = lheight as usize;
                        if cm.contains(lw, lh, abs_x, abs_y) {
                            match cm.hit_test(lw, lh, abs_x, abs_y) {
                                Some(row) if cm.is_enabled(row) => {
                                    // Activate by clicking — same as Enter.
                                    let cm = self.context_menu.take().unwrap();
                                    let item = cm.request.items[row].clone();
                                    let request = cm.request.clone();
                                    self.dispatch_context_menu_item(&item, &request);
                                }
                                _ => {
                                    // Click on separator or disabled row —
                                    // dismiss without firing.
                                    self.dismiss_context_menu();
                                }
                            }
                        } else {
                            // Clicked outside the panel — dismiss.
                            self.dismiss_context_menu();
                        }
                    }
                    self.mark_chrome_dirty();
                    self.request_redraw();
                    return;
                }

                // Back/Forward side buttons → history navigation regardless
                // of host mode. Intercept before OSR dispatch.
                if state == Pressed {
                    match button {
                        MouseButton::Back => {
                            self.dispatch_action(&buffr_modal::PageAction::HistoryBack);
                            return;
                        }
                        MouseButton::Forward => {
                            self.dispatch_action(&buffr_modal::PageAction::HistoryForward);
                            return;
                        }
                        _ => {}
                    }
                }

                // Tab-strip click: Left = focus / drag, Middle = close,
                // Right = tab context menu. Press on left selects the tab
                // AND records a drag src; release on left finalizes the
                // drag if the cursor moved to a different slot.
                let tab_strip_idx = self.hit_test_tab_strip();
                if state == Pressed
                    && button == MouseButton::Right
                    && let Some(idx) = tab_strip_idx
                {
                    // Synthesise a Tab-target context-menu request. Tab
                    // clicks never reach CEF, so there's no
                    // drain_context_menu_requests path for this — build
                    // the request here and show it directly.
                    let (pinned, url) = self
                        .active_engine_dyn()
                        .and_then(|e| e.tabs_summary().get(idx).map(|t| (t.pinned, t.url.clone())))
                        .unwrap_or((false, String::new()));
                    let tab_count = self.tab_ids.len().max(1);
                    let items = buffr_core::build_tab_context_menu_model(tab_count, idx, pinned);
                    // Cursor → chrome-buffer coords. The chrome buffer is
                    // LOGICAL, so the anchor has to be in DIPs or the menu
                    // is drawn at 2x the cursor position on HiDPI (M30).
                    // osr_cursor is browser-region-relative; add the CEF
                    // y-offset first, then convert.
                    let size = self
                        .window
                        .as_ref()
                        .map(|w| w.physical_size())
                        .unwrap_or_default();
                    let win_w = size.width.max(1);
                    let win_h = size.height.max(1);
                    let cef_y_offset = self.cef_child_rect(win_w, win_h).1 as i32;
                    let (anchor_x, anchor_y) = physical_cursor_to_dip(
                        self.osr_cursor.0,
                        self.osr_cursor.1 + cef_y_offset,
                        0,
                        self.current_scale(),
                    );
                    let request = ContextMenuRequest {
                        x: anchor_x,
                        y: anchor_y,
                        browser_id: 0,
                        items,
                        target: ContextMenuTarget::Tab {
                            index: idx,
                            // The tab's id at menu-open time, so dispatch
                            // can re-locate it if the list shifts (a
                            // background window.open, another close).
                            id: self.tab_ids.get(idx).map(|t| t.0).unwrap_or(0),
                        },
                        link_url: url,
                        source_url: String::new(),
                        selection_text: String::new(),
                    };
                    self.context_menu = Some(ActiveContextMenu::new(request));
                    self.mark_chrome_dirty();
                    self.request_redraw();
                    return;
                }
                if state == Pressed
                    && button == MouseButton::Left
                    && let Some(idx) = tab_strip_idx
                {
                    if let Some(host) = self.active_engine_dyn() {
                        host.select_tab(self.tab_ids[idx]);
                        self.on_tab_switch();
                    }
                    // Tab switch implies the user moved focus away from
                    // whatever they were typing in the overlay (omnibar /
                    // find / cmdline) — dismiss it.
                    self.close_overlay();
                    self.tab_drag_src = Some(idx);
                    return;
                }
                if state != Pressed
                    && button == MouseButton::Left
                    && let Some(src) = self.tab_drag_src.take()
                    && let Some(dst) = tab_strip_idx
                    && dst != src
                    && let Some(host) = self.active_engine_dyn()
                {
                    host.move_tab(src, dst);
                    self.mark_session_dirty();
                    self.refresh_tab_strip();
                    self.request_redraw();
                    return;
                }
                if state == Pressed
                    && button == MouseButton::Middle
                    && let Some(idx) = tab_strip_idx
                {
                    let id = self.tab_ids[idx];
                    // Middle-click on a pinned tab also gates through
                    // the confirmation overlay so the user can't lose
                    // a pinned tab by misaiming.
                    let pinned = self
                        .active_engine_dyn()
                        .and_then(|e| e.tabs_summary().get(idx).map(|t| t.pinned))
                        .unwrap_or(false);
                    // A pinned tab is never closed silently: arm the
                    // confirmation — or block on one already pending
                    // (possibly for a different tab) — rather than fall
                    // through to an unconfirmed close (§11-15).
                    if pinned && self.arm_pinned_close(id) {
                        return;
                    }
                    if let Some(host) = self.active_engine_dyn() {
                        let _ = host.close_tab(id);
                    }
                    // Phase 3: check across all engines for the exit decision.
                    let total_remaining: usize = self.engines.values().map(|e| e.tab_count()).sum();
                    self.refresh_tab_strip();
                    if total_remaining == 0 {
                        info!("tab_close: last tab gone (all engines) — requesting graceful exit");
                        self.request_exit();
                    }
                    return;
                }

                let mut enter_visual = false;
                let mut exit_visual = false;
                if let Some(host) = self.active_engine_dyn()
                    && let Some(cef_button) = button_to_neutral(&button)
                {
                    use crate::windowing::PointerButtonState::Released;
                    let mouse_up = state == Released;
                    // Track held mouse buttons so subsequent CursorMoved
                    // events carry the *_MOUSE_BUTTON event flag — without
                    // it, Chromium's hit-test treats drag-motion as plain
                    // hover and won't extend the text selection.
                    let btn_flag: u32 = if cef_button == NeutralMouseButton::Left {
                        16
                    } else if cef_button == NeutralMouseButton::Middle {
                        32
                    } else if cef_button == NeutralMouseButton::Right {
                        64
                    } else {
                        0
                    };
                    if mouse_up {
                        self.osr_mouse_buttons &= !btn_flag;
                    } else {
                        self.osr_mouse_buttons |= btn_flag;
                    }
                    // Double-click detection.
                    let now = Instant::now();
                    let same_button = self
                        .osr_last_click_button
                        .map(|b| b == cef_button)
                        .unwrap_or(false);
                    if !mouse_up {
                        if same_button
                            && now.duration_since(self.osr_last_click_at) < DOUBLE_CLICK_WINDOW
                        {
                            self.osr_click_count = (self.osr_click_count + 1).min(3);
                        } else {
                            self.osr_click_count = 1;
                        }
                        self.osr_last_click_at = now;
                        self.osr_last_click_button = Some(cef_button);
                        // Promote CEF widget focus on the first real
                        // click into the OSR region. We deliberately
                        // skip set_focus(1) on load so page-autofocus
                        // doesn't drive a caret-blink paint loop; this
                        // is the place the user finally tells CEF the
                        // page is theirs to interact with.
                        host.osr_focus(true);
                        if button == MouseButton::Left {
                            // Track drag origin so a left-button release
                            // far from the press point promotes the
                            // engine to Visual mode (CEF natively
                            // selects the swept text).
                            self.osr_drag_start = Some(self.osr_cursor);
                        }
                    } else if button == MouseButton::Left {
                        // osr_drag_start = Some at release ⇒ press did
                        // not cross the drag threshold (CursorMoved would
                        // have cleared it). That's a click — branch on
                        // click_count. None means a drag already promoted
                        // to Visual during the move; nothing to do.
                        if self.osr_drag_start.take().is_some() {
                            if self.osr_click_count >= 2 {
                                // Double / triple click — CEF auto-selects
                                // a word / line. Reflect that in the
                                // engine.
                                enter_visual = true;
                                tracing::debug!(
                                    n = self.osr_click_count,
                                    "osr multi-click → Visual mode"
                                );
                            } else {
                                // Single click. Drop Visual if active.
                                // Clicking an input still goes to Insert
                                // via the JS focusin path.
                                exit_visual = true;
                            }
                        }
                    }
                    let mods = mods_to_cef(&self.modifiers) | self.osr_mouse_buttons;
                    // osr_cursor is in physical pixels (browser-region-relative);
                    // CEF OSR takes DIPs — route through helper (cef_y_offset=0
                    // because osr_cursor is already region-relative).
                    let (phys_bx, phys_by) = self.osr_cursor;
                    let click_scale = self.current_scale();
                    let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, click_scale);
                    host.osr_mouse_click(bx, by, cef_button, mouse_up, self.osr_click_count, mods);
                }
                if enter_visual {
                    if let Ok(mut e) = self.engine.lock() {
                        e.set_mode(PageMode::Visual);
                    }
                    self.refresh_title();
                    self.request_redraw();
                } else if exit_visual {
                    let was_visual = self
                        .engine
                        .lock()
                        .map(|e| e.mode() == PageMode::Visual)
                        .unwrap_or(false);
                    if was_visual {
                        if let Ok(mut e) = self.engine.lock() {
                            e.set_mode(PageMode::Normal);
                        }
                        self.refresh_title();
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::Scroll(scroll_ev) => {
                if self.engines.is_empty() {
                    return;
                }

                // Two-finger horizontal-swipe back/forward — only on
                // high-res (pixel) input. If a swipe commits or we're still
                // mid-gesture after a commit, swallow the event so it
                // doesn't also scroll the page.
                let is_pixel = matches!(
                    scroll_ev.source,
                    crate::windowing::AxisSource::Finger | crate::windowing::AxisSource::Continuous
                );
                if is_pixel {
                    let (swipe_dx, swipe_dy) = scroll_swipe_delta(&scroll_ev);
                    if let Some(action) = self.detect_swipe(surface_id, swipe_dx, swipe_dy) {
                        self.dispatch_action(&action);
                        return;
                    }
                    if self.swipe.committed {
                        return;
                    }
                }

                let Some(host) = self.active_engine_dyn() else {
                    // No active engine yet (startup race) — silently discard the event.
                    return;
                };
                let (dx, dy, is_pixel_delta) = scroll_to_cef_delta(&scroll_ev);
                if is_pixel_delta {
                    // Track velocity only for high-res input; discrete
                    // wheel ticks have their own physical inertia.
                    self.osr_wheel_velocity = (dx as f32, dy as f32);
                    self.osr_wheel_last_at = Some(Instant::now());
                } else {
                    // Cancel any in-flight momentum on discrete tick.
                    self.osr_wheel_velocity = (0.0, 0.0);
                    self.osr_wheel_last_at = None;
                }
                let mods = mods_to_cef(&self.modifiers);
                // osr_cursor is physical (browser-region-relative); CEF OSR takes DIPs.
                let (phys_bx, phys_by) = self.osr_cursor;
                let wheel_scale = self.current_scale();
                let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, wheel_scale);
                tracing::trace!(dx, dy, bx, by, "input: scroll -> CEF");
                host.osr_mouse_wheel(bx, by, dx, dy, mods);
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                // winit may dispatch ModifiersChanged AFTER the
                // matching key-release event on some backends. Without
                // this arm `self.modifiers` would stay at the
                // pre-release state because the key event carries the
                // cached (still-pre-release) modifiers — the v0.14.3
                // Ctrl-sticky regression.
                self.modifiers = modifiers;
            }
            WindowEvent::Key(event) => {
                // Sync `self.modifiers` from the event so downstream
                // consumers that read the cached field (CEF VK
                // forwarding, edit-mode chord routing, pointer paths
                // between key events) see the modifiers that were
                // live AT THE TIME of this key event. The bridge also
                // emits ModifiersChanged for modifier-only transitions
                // (see arm above) — both paths run, last writer wins.
                self.modifiers = event.modifiers;
                // Pinned-close confirmation takes precedence over
                // everything else: `y` or `<Enter>` confirms, `n` /
                // `<Esc>` dismisses. Other keys are swallowed so the
                // page underneath can't receive stray input while a
                // modal banner is up.
                if self.confirm_close_pinned.is_some() && self.confirm_handle_key(&event) {
                    return;
                }
                // Permissions prompt takes precedence over every other
                // key sink. Pressing `a`/`d`/`A`/`D`/Esc resolves the
                // request; nothing else is allowed through until the
                // queue drains.
                if self.permissions_handle_key(&event) {
                    return;
                }
                // Context-menu overlay: Up/Down/Enter/Esc are consumed.
                // Other keys dismiss the menu and fall through to normal
                // key dispatch.
                if self.context_menu_handle_key(&event) {
                    return;
                }
                // Overlay open → all keys route to it.
                if self.overlay_handle_key(&event) {
                    return;
                }
                // Hint mode: route printable chars + Esc + BS straight
                // to the host's hint-session API. The modal engine
                // already sits in `Mode::Hint` (set by the action
                // dispatch below), but the engine itself doesn't know
                // about per-keystroke hint matching.
                if self.hint_mode_handle_key(&event) {
                    return;
                }
                // Edit-mode takes precedence over the page-mode FSM
                // once a field is focused (Editing state). Esc is
                // intercepted; all other keys forward directly to CEF.
                //
                // The engine can also sit in `PageMode::Insert` with
                // NO field focused — `enter_insert_mode` is bindable
                // user config, and a focused field can go away under
                // us. There, `Engine::feed` answers every chord with
                // `Step::EditModeActive` before consulting the trie,
                // so no binding (Esc included) can fire and the
                // keyboard is stranded until the user clicks an input
                // or kills the window. Run the same handler in that
                // state so its Esc branch still gets a look; it
                // returns `false` for every other key, which falls
                // through to the dispatch below exactly as before.
                let insert_no_focus = !matches!(&self.edit_focus, EditFocus::Editing { .. })
                    && matches!(self.engine.lock().map(|e| e.mode()), Ok(PageMode::Insert));
                if (matches!(&self.edit_focus, EditFocus::Editing { .. }) || insert_no_focus)
                    && self.edit_mode_handle_key(&event)
                {
                    return;
                }
                // Page-mode dispatch accepts auto-repeat events so
                // holding e.g. `H` / `L` cycles tabs at OS repeat speed.
                // Per-action filtering happens after resolution: see
                // `PageAction::is_repeatable`.
                let is_repeat = event.repeat;
                let Some(chord) = key_event_to_chord_with_repeat(&event) else {
                    return;
                };
                let now = self.startup.elapsed();
                let (step, post_mode) = match self.engine.lock() {
                    Ok(mut e) => {
                        let s = e.feed(chord, now);
                        let m = e.mode();
                        (s, m)
                    }
                    Err(_) => return,
                };
                match step {
                    Step::Resolved(action) => {
                        // Drop auto-repeat events for actions that
                        // shouldn't stream (TabClose, OpenOmnibar, etc).
                        if is_repeat && !action.is_repeatable() {
                            return;
                        }
                        // `EnterInsertMode` (`i`) flips the engine into
                        // PageMode::Insert. Entry into a specific field is
                        // handled via the JS focusin bridge; `i` alone
                        // without a focused input is a no-op at the page
                        // level — the engine mode flip is sufficient to
                        // unblock subsequent keys once a field is clicked.
                        if action == buffr_modal::PageAction::EnterInsertMode {
                            self.refresh_title();
                            return;
                        }
                        // OpenOmnibar / OpenCommandLine flip the
                        // engine into Mode::Command and ALSO open the
                        // matching overlay UI. The host's `dispatch`
                        // for these is a no-op log, so we handle the
                        // UI side here.
                        match &action {
                            buffr_modal::PageAction::OpenOmnibar => {
                                self.open_omnibar();
                            }
                            buffr_modal::PageAction::OpenCommandLine => {
                                self.open_command_line();
                            }
                            buffr_modal::PageAction::Find { forward } => {
                                self.open_find(*forward);
                            }
                            _ => {
                                self.dispatch_action(&action);
                            }
                        }
                    }
                    Step::Pending | Step::Ambiguous { .. } => {
                        // Phase 3 chrome will surface a count/pending
                        // buffer indicator in the status line. For
                        // now, silently accumulate.
                    }
                    Step::Reject => {
                        // Vim-style: only pass unbound keys to the page
                        // in modes where the page owns input (Edit /
                        // Command). In Normal, Visual, Hint, and Pending
                        // the modal layer owns the keyboard — silently
                        // swallow so typing `a`, `s`, etc. doesn't
                        // type into a focused field or trigger browser
                        // shortcuts.
                        let pass_through =
                            matches!(post_mode, PageMode::Insert | PageMode::Command);
                        if pass_through {
                            // A surrogate-pair character (emoji, rare CJK)
                            // cannot travel as a CHAR key event — insert it
                            // as text instead (§11-16).
                            if let Some(text) = multi_unit_char_text(event.text.as_deref()) {
                                self.insert_text_via_exec(text);
                            } else if let Some(host) = self.active_engine_dyn() {
                                let mods = mods_to_cef(&self.modifiers);
                                // Reject path: in Insert/Command modes a
                                // text input may or may not be focused.
                                // Use `edit_focus` to set the flag.
                                let editable = matches!(self.edit_focus, EditFocus::Editing { .. });
                                tracing::warn!(
                                    state = ?event.state,
                                    key_code = ?event.key_code,
                                    post_mode = ?post_mode,
                                    edit_focus = ?self.edit_focus,
                                    editable,
                                    "page-mode Reject pass-through (key bypassed edit_mode_handle_key)"
                                );
                                for ev in key_to_neutral_events(&event, mods, editable) {
                                    host.osr_key_event(ev);
                                }
                            }
                        } else {
                            trace!(
                                ?chord,
                                ?post_mode,
                                "key not bound — swallowed in modal mode"
                            );
                        }
                    }
                    Step::EditModeActive => {
                        // Engine is already in PageMode::Insert; consume
                        // the key. If a field is focused, edit_mode_handle_key
                        // above already handled it; otherwise the key is
                        // silently dropped (no input is active).
                    }
                }
                self.refresh_title();
            }
            // ── IME composition (#86) ─────────────────────────────────────────
            //
            // Route wayr IME lifecycle events through the active engine's
            // neutral IME trait methods:
            //
            //   Preedit("") with cursor None → cancel (empty preedit = dismiss)
            //   Preedit(s)  with cursor      → set_composition
            //   Commit(s)                    → commit
            //
            // wayr has no Enabled/Disabled variants — the consumer calls
            // Ime::enable()/disable() directly; we never see them here.
            //
            // Both the CEF backend (via `BrowserHost::ime_*`) and the blink-cdp
            // backend (via CDP `Input.imeSetComposition` / `Input.insertText`)
            // implement these methods; the default no-op handles future backends.
            WindowEvent::Ime(ime_event) => {
                use crate::windowing::ImeEvent;
                if let Some(engine) = self.active_engine_dyn() {
                    // wayr's ImeEvent is #[non_exhaustive] from another
                    // crate, so the `_` arm is reachable on Linux. On
                    // non-Linux the bridge ImeEvent is same-crate so
                    // clippy flags it unreachable — allow either way
                    // since we want the forward-compat catch-all to
                    // survive.
                    #[allow(unreachable_patterns)]
                    match ime_event {
                        ImeEvent::Preedit { text, cursor } => {
                            if text.is_empty() {
                                // Empty preedit signals the OS is cancelling the
                                // composition (e.g. Esc pressed in the IME window).
                                engine.ime_cancel();
                            } else {
                                engine.ime_set_composition(
                                    &text,
                                    cursor.map(|c| (c as usize, c as usize)),
                                );
                            }
                        }
                        ImeEvent::Commit(text) => {
                            engine.ime_commit(&text);
                        }
                        ImeEvent::DeleteSurroundingText { .. } => {
                            // No engine API for this yet — ignore.
                        }
                        // ImeEvent is #[non_exhaustive]; match unknown variants.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &mut EventLoop<BuffrUserEvent>) {
        // Heartbeat stamp at every loop iteration (replaces winit's new_events).
        self.tick_heartbeat();
        // Ctrl+C single-press exit: the ctrlc handler sets this flag;
        // we check it here before doing any other work so the exit is
        // clean (session saved, CEF not left in a wedged state).
        if self.check_shutdown(event_loop) {
            return;
        }

        // Pump CEF every frame. On macOS native windowed CEF integrates
        // with AppKit; calling CefDoMessageLoopWork from inside winit's
        // AppKit event handler can re-enter winit and trip its macOS
        // reentrancy guard.
        pump_cef_message_loop(&*self.backend, &mut self.cef_next_pump_at);

        // Wheel-momentum tick: synthesize a decaying wheel event once
        // high-res input has gone quiet, mimicking native Chrome's
        // post-swipe ease-out. No-op while real input is still arriving.
        self.tick_wheel_momentum();

        // Edit-mode: drain focus/blur/mutate events from the JS bridge.
        // Runs before the engine tick so state is consistent when key
        // routing fires later in the same event-loop iteration.
        self.drain_edit_focus_events();
        // Defer-then-flip for Tab transfer: if the grace window after a
        // Blur expired without a sibling Focus arriving, finalize the
        // exit from Insert mode now.
        self.expire_pending_blur();

        // OSR sleep policy: process focus-blur debounce and audio events.
        // Must run before policy changes affect the rest of the tick.
        {
            // 1. Expire occlude debounce: if the grace window after Occluded(true)
            //    elapsed without an Occluded(false) arriving, commit occluded=true.
            if self
                .sleep_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.occluded = true;
                self.sleep_deadline = None;
            }

            // 2. Drain audio events from all engines (fan-out via trait).
            //    Non-CEF engines (blink-cdp) return Vec::new() by default.
            {
                let mut any_audio_events = false;
                for engine in self.engines.values() {
                    let events = engine.drain_audio_events();
                    if !events.is_empty() {
                        any_audio_events = true;
                    }
                }
                if any_audio_events {
                    // Recompute media_active across all engines via the trait.
                    self.media_active = self.engines.values().any(|h| h.any_audio_active());
                    tracing::debug!(
                        media_active = self.media_active,
                        "audio events drained (multi-engine fan-out)"
                    );
                }
                self.video_active = self.engines.values().any(|h| h.any_video_active());
            }

            // Evaluate idle-inhibit policy and acquire/release as needed.
            {
                let cfg = &*self.idle_inhibit_config;
                let want_inhibit = cfg.enabled
                    && (self.video_active || (cfg.inhibit_audio_only && self.media_active))
                    && (!cfg.require_focus || self.window_focused);
                if let Some(ref inhibitor) = self.idle_inhibitor {
                    let is_active = inhibitor.is_active();
                    if want_inhibit && !is_active {
                        tracing::debug!("idle_inhibit: acquiring");
                        if let Err(e) = inhibitor.acquire() {
                            tracing::warn!(error = %e, "idle_inhibit: acquire failed");
                        }
                    } else if !want_inhibit && is_active {
                        tracing::debug!("idle_inhibit: releasing");
                        if let Err(e) = inhibitor.release() {
                            tracing::warn!(error = %e, "idle_inhibit: release failed");
                        }
                    }
                }
            }

            // 3. Fire the JS media probe while occluded so navigator.mediaSession
            //    and fullscreen-video changes are detected at ~2 s cadence.
            //    Probe runs on the active engine only — it's the one presenting.
            if self.occluded
                && let Some(engine) = self.active_engine_dyn()
            {
                let now = Instant::now();
                let due = self.media_probe_next.map(|t| now >= t).unwrap_or(true);
                if due {
                    engine.run_media_probe();
                    self.media_probe_next = Some(now + MEDIA_PROBE_INTERVAL);
                }
            }

            // 4. Recompute paint policy and apply transitions.
            self.recompute_paint_policy();

            // 5. Heuristic-occlusion wake probe: while Sleeping with a
            //    next_probe_at set, do a one-shot present to test whether
            //    the compositor is releasing buffers again.  The probe
            //    bypasses the sleep guard via probe_pending; the post-paint
            //    observe_present_us either wakes us (fast) or re-sleeps
            //    and reschedules (still slow).
            if self.paint_policy == PaintPolicy::Sleeping
                && self.next_probe_at.is_some_and(|t| Instant::now() >= t)
            {
                self.probe_pending = true;
                self.next_probe_at = None;
                if let Some(host) = self.active_engine_dyn() {
                    host.osr_sleep(false);
                    host.osr_invalidate_view();
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
        }

        // Engine ambiguity timeout: if a single-chord prefix is
        // sitting on the buffer past the timeout window, fire the
        // shorter binding. This is the vim `&timeoutlen` behaviour.
        let now = self.startup.elapsed();
        let action = match self.engine.lock() {
            Ok(mut e) => e.tick(now),
            Err(_) => None,
        };
        if let Some(action) = action {
            self.dispatch_action(&action);
            self.refresh_title();
        }

        // Address-change events: drain URL updates pushed by
        // on_address_change. No CEF call; purely reads from the shared
        // VecDeque. Fires before find so Tab.url is current.
        if let Some(host) = self.active_engine_dyn()
            && host.pump_address_changes()
        {
            self.mark_session_dirty();
            // edit.js is re-injected on each new page load, which
            // reassigns all field IDs from f1. Any saved ID is stale.
            self.last_focused_field = None;
            self.request_redraw();

            // Cross-engine navigation check (Phase 3): if the active tab's
            // new URL routes to a different engine, open a new tab on the
            // target and close the in-flight tab on the source.
            self.check_cross_engine_nav();
        }

        // Forward any pending CEF cursor change to wayr's per-seat
        // cursor-shape device. Reads the shared cursor state and
        // calls EventLoop::set_cursor once per tick (last writer wins
        // — CEF can fire many times per frame as the cursor moves).
        self.pump_cursor_changes(event_loop);

        // Drain any find result the CEF browser thread posted since
        // the last tick, then check whether the `--find` smoke
        // dispatch is due.
        self.pump_find_results();
        self.maybe_dispatch_find_smoke();
        self.maybe_dispatch_find_live();

        // Drain any hint event (Ready / Error from the renderer) and
        // refresh the statusline indicator off the live session.
        if let Some(engine) = self.active_engine_dyn() {
            if engine.pump_hint_events() {
                self.request_redraw();
            }
            // `hint_status()` locks the host's tabs + active tab and clones
            // the typed buffer on every tick; with no live session it can
            // only ever yield `None`, so skip it unless hint mode is active.
            // `pump_hint_events` above still drains every tick so mode
            // transitions are noticed promptly, and `exit_hint_mode` clears
            // `hint_state` on the way out.
            if engine.is_hint_mode() {
                let new_status = engine.hint_status().map(|h| UiHintStatus {
                    typed: h.typed,
                    match_count: h.match_count as u32,
                    background: h.background,
                });
                if new_status != self.statusline.hint_state {
                    self.statusline.hint_state = new_status;
                    self.mark_chrome_dirty();
                    self.request_redraw();
                }
            }
        }

        // Context-menu: drain any right-click requests. Only the most
        // recent is shown (earlier ones are dropped — only one menu visible
        // at a time). Mark chrome dirty so the overlay appears immediately.
        if let Some(engine) = self.active_engine_dyn() {
            let neutral_reqs = engine.drain_context_menu_requests();
            if let Some(req) = neutral_reqs.into_iter().last() {
                // Rebuild the item list from the neutral fields + engine state.
                let items = build_context_menu_items_from_neutral(
                    &req,
                    engine.can_go_back(),
                    engine.can_go_forward(),
                    engine.is_loading(),
                );
                tracing::debug!(
                    browser_id = req.browser_id,
                    items = items.len(),
                    "context_menu: showing overlay"
                );
                // Convert the neutral request back to the apps-layer type so
                // `ActiveContextMenu`, `dispatch_context_menu_item`, and
                // `resolve_tab_target` can work with a single unified type.
                let source_url = req
                    .image_url
                    .as_deref()
                    .or(req.media_url.as_deref())
                    .unwrap_or("")
                    .to_string();
                let core_req = ContextMenuRequest {
                    x: req.x,
                    y: req.y,
                    browser_id: req.browser_id,
                    items,
                    target: ContextMenuTarget::Page,
                    link_url: req.link_url.unwrap_or_default(),
                    source_url,
                    selection_text: req.selection_text.unwrap_or_default(),
                };
                self.context_menu = Some(ActiveContextMenu::new(core_req));
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // CEF popup re-route: drain URLs queued by on_before_popup for
        // NEW_FOREGROUND_TAB / NEW_BACKGROUND_TAB dispositions and open
        // each as a tab. Popup-window dispositions (OAuth, etc) are not
        // queued — CEF handles those natively.
        if let Some(engine) = self.active_engine_dyn() {
            for target in drain_popup_targets(&engine.popup_queue()) {
                let url = target.url;
                // Ctrl/Cmd+click and middle-click queue reading for later and
                // must not move the user; `target="_blank"` and `window.open`
                // are "take me there". Both used to call open_tab, so every
                // Ctrl+click yanked the user off the page they were reading.
                let result = if target.focus.is_foreground() {
                    engine.open_tab(&url)
                } else {
                    engine.open_tab_background(&url)
                };
                if let Err(err) = result {
                    warn!(
                        error = %err,
                        %url,
                        foreground = target.focus.is_foreground(),
                        "popup -> open_tab failed"
                    );
                }
            }
        }

        // Popup create: drain PopupCreated events and spawn a winit window
        // + wgpu renderer for each new popup browser.
        let popup_creates = drain_popup_creates(&self.popup_create_sink);
        for created in popup_creates {
            // Cap live popup windows: a page that evades CEF's popup
            // blocker (a gesture-triggered chain, a popunder) must not
            // grow unbounded windows / GPU surfaces / fds. Matches the
            // CEF-side pending-alloc queue cap (PENDING_POPUP_ALLOC_CAP
            // in buffr-cef's osr.rs), which bounds the same flood
            // before on_after_created. Over the cap the browser is
            // closed without a window; its teardown drains through the
            // popup-close sink below (no window id registered, so the
            // drop path is a no-op).
            if self.popups.len() >= MAX_LIVE_POPUPS {
                warn!(
                    cap = MAX_LIVE_POPUPS,
                    browser_id = created.browser_id,
                    "popup: live window cap reached — closing browser without a window"
                );
                if let Some(engine) = self.active_engine_dyn() {
                    engine.popup_close(created.browser_id);
                }
                continue;
            }
            let title = if created.url.is_empty() {
                "buffr popup".to_string()
            } else {
                created.url.clone()
            };
            #[allow(clippy::arc_with_non_send_sync)]
            let popup_win = match Toplevel::builder()
                .with_title(&title)
                .with_initial_size(crate::windowing::Size::new(800, 600))
                .build(event_loop)
            {
                Ok(w) => Arc::new(w),
                Err(err) => {
                    warn!(error = %err, browser_id = created.browser_id, "popup: create_window failed");
                    continue;
                }
            };
            let popup_size = popup_win.physical_size();
            let popup_renderer = match crate::render::Renderer::new(
                Arc::clone(&popup_win),
                (popup_size.width, popup_size.height),
            ) {
                Ok(r) => r,
                Err(err) => {
                    warn!(error = %err, browser_id = created.browser_id, "popup: renderer init failed");
                    continue;
                }
            };
            // Initial OSR resize to the popup's CEF page rect — the window
            // minus its address-bar strip, not the full window (M35). The
            // rect must be the one the quad is painted into, or CEF lays the
            // page out for rows that are never displayed.
            let inner = popup_win.physical_size();
            let (_, _, pw, ph) = popup_cef_rect_pure(
                inner.width.max(1),
                inner.height.max(1),
                popup_win.scale_factor() as f32,
            );
            if let Some(engine) = self.active_engine_dyn() {
                engine.popup_resize(created.browser_id, pw, ph);
            }
            // Wire OSR on_paint → popup window redraw via EventLoopProxy.
            let proxy = self.event_proxy.clone();
            let bid = created.browser_id;
            created.view.set_wake(Arc::new(move || {
                let _ = proxy.send_event(BuffrUserEvent::OsrFramePopup(bid));
            }));
            let wid = popup_win.id();
            debug!(
                browser_id = created.browser_id,
                ?wid,
                "popup: window created"
            );
            self.popup_window_id_by_browser
                .insert(created.browser_id, wid);
            self.popups.insert(
                wid,
                PopupWindow {
                    window: popup_win,
                    renderer: popup_renderer,
                    browser_id: created.browser_id,
                    frame: created.frame,
                    view: created.view,
                    url: created.url,
                    last_osr_generation: 0,
                    osr_gpu_stale: false,
                    last_osr_dims: None,
                    pending_cef_resize: None,
                    repaint_retry_at: None,
                    osr_scratch: Vec::new(),
                    chrome_generation: 1,
                    last_painted_chrome_gen: 0,
                    cursor: (0, 0),
                    mouse_buttons: 0,
                    modifiers: Modifiers::default(),
                    last_click_at: Instant::now(),
                    last_click_button: None,
                    click_count: 1,
                },
            );
        }

        // Popup close: drain browser-id events and drop their windows.
        let popup_closes: Vec<i32> = drain_popup_closes(&self.popup_close_sink);
        for browser_id in popup_closes {
            if let Some(wid) = self.popup_window_id_by_browser.remove(&browser_id) {
                self.popups.remove(&wid);
                debug!(browser_id, "popup: window dropped");
            }
        }

        // Popup URL updates: drain address-change events for popup browsers
        // and update the corresponding popup window's URL bar.
        let popup_addr_changes: Vec<(i32, String)> = if let Some(engine) = self.active_engine_dyn()
        {
            engine.popup_drain_address_changes()
        } else {
            Vec::new()
        };
        for (browser_id, url) in popup_addr_changes {
            if let Some(&wid) = self.popup_window_id_by_browser.get(&browser_id)
                && let Some(popup) = self.popups.get_mut(&wid)
                && popup.url != url
            {
                popup.url = url.clone();
                popup.chrome_generation = popup.chrome_generation.wrapping_add(1);
                popup.window.request_redraw();
                debug!(browser_id, %url, "popup: URL updated");
            }
        }

        // Popup title updates: drain title-change events for popup browsers
        // and update the winit window title.
        let popup_title_changes: Vec<(i32, String)> = if let Some(engine) = self.active_engine_dyn()
        {
            engine.popup_drain_title_changes()
        } else {
            Vec::new()
        };
        for (browser_id, title) in popup_title_changes {
            if let Some(&wid) = self.popup_window_id_by_browser.get(&browser_id)
                && let Some(popup) = self.popups.get(&wid)
            {
                popup.window.set_title(&title);
                debug!(browser_id, %title, "popup: title updated");
            }
        }

        // Permission prompt: pull the front of the queue into a
        // visible widget. `sync_permissions_prompt` leaves an active
        // prompt alone while it is still the front of the queue, so the
        // user always sees one request at a time — but it does take a
        // prompt down when the backend has withdrawn the request behind
        // it (tab navigated away), so no keystroke can be answered
        // against a request that is no longer on offer.
        if self.sync_permissions_prompt() {
            self.mark_chrome_dirty();
            self.request_redraw();
        }

        // Live URL / zoom sync: throttled to ~4 Hz (250 ms).
        // URL is now cheap (reads cached Tab.url; no CEF call).
        // Zoom polls host.zoom_level() at the same cadence.
        // Also detects navigation, active-index, and tab-list changes
        // for the session dirty flag.
        // Collect the poll results outside the borrow so we can call
        // `mark_session_dirty` (which takes &mut self) afterwards.
        let url_poll_result: Option<(String, Option<usize>, Vec<TabId>, f64)> =
            if let Some(host) = self.active_engine_dyn() {
                let now = Instant::now();
                if now.duration_since(self.last_url_poll) >= Duration::from_millis(250) {
                    self.last_url_poll = now;
                    let live = host.active_tab_live_url();
                    let active_idx = host.active_index();
                    // Tab-list snapshot: reuse the id list `refresh_tab_strip`
                    // populated last tick instead of materialising N
                    // `TabSummary` structs (each with String title/url) just
                    // to read their ids. One tick stale is fine — the
                    // comparison below is against `last_session_tab_ids`, so
                    // a change is still detected (a tick later) and marks the
                    // session dirty.
                    let current_ids = self.tab_ids.clone();
                    let zoom = host.active_zoom_level();
                    Some((live, active_idx, current_ids, zoom))
                } else {
                    None
                }
            } else {
                None
            };
        if let Some((live, active_idx, current_ids, zoom)) = url_poll_result {
            if !live.is_empty() && live != self.statusline.url {
                self.statusline.url = live.clone();
                self.refresh_title();
                self.mark_chrome_dirty();
                self.request_redraw();
            }
            // Session dirty detection: URL changed since last save.
            if !live.is_empty() && live != self.last_session_url {
                self.mark_session_dirty();
            }
            // Active-index changed.
            if active_idx != self.last_session_active {
                tracing::debug!(
                    new_idx = ?active_idx,
                    last_idx = ?self.last_session_active,
                    "session: active-index changed -> mark_session_dirty"
                );
                self.mark_session_dirty();
            }
            // Tab-list (open / close / reorder) changed.
            if current_ids != self.last_session_tab_ids {
                self.mark_session_dirty();
            }
            // Zoom level: poll active tab and update statusline.
            if (zoom - self.statusline.zoom_level).abs() > f64::EPSILON {
                self.statusline.zoom_level = zoom;
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // Flush session when dirty and the debounce window has expired.
        // Shutdown paths (CloseRequested, last-tab-gone, ctrl-c) call
        // `save_session_now` directly, bypassing this check.
        if self.session_dirty {
            let debounce = Duration::from_millis(SESSION_SAVE_DEBOUNCE_MS);
            let elapsed_enough = self
                .session_dirty_since
                .map(|t| t.elapsed() >= debounce)
                .unwrap_or(true);
            if elapsed_enough {
                self.save_session_now();
            }
        }

        // Download notices: drop any that have lived past their expiry
        // window. Trigger a redraw + resync when the queue changes so
        // the chrome immediately reclaims the strip height.
        {
            let dropped = expire_stale_notices(&self.download_notice_queue);
            if dropped > 0 {
                self.resync_cef_rect();
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // Refresh tab-strip render input. The host's tab list can
        // change underneath us (LoadHandler updates URL/title;
        // dispatched tab actions add/remove rows) so we resync every
        // tick. The redraw is gated on the returned diff; the tab
        // summaries are handed to the favicon pump below so it
        // doesn't re-query the engine a second time this tick.
        let (tabs_changed, summaries) = self.refresh_tab_strip();
        if tabs_changed {
            self.request_redraw();
        }

        // Drain any decoded favicons from CEF and stash by browser id,
        // applying cache hits and prefills. refresh_tab_strip picks
        // them up on the next tab refresh.
        if self.pump_favicon_updates(&summaries) {
            self.mark_chrome_dirty();
            self.request_redraw();
        }

        // Phase 6 telemetry: 60-second background flush so an abrupt
        // exit (segfault from CEF, OOM kill, etc.) loses at most one
        // minute of counter increments. No-op when disabled.
        let wall_now = Instant::now();
        if wall_now.duration_since(self.counters_flush_at) >= Duration::from_secs(60) {
            self.counters_flush_at = wall_now;
            self.counters.flush();
        }

        // Cursor blink for the open overlay. 500ms toggle; we only
        // request a redraw when the bit actually flips so the page
        // region isn't repainted needlessly.
        if self.overlay.is_some() {
            let now = Instant::now();
            if now.duration_since(self.cursor_blink_at) >= Duration::from_millis(500) {
                self.cursor_blink_at = now;
                if let Some(overlay) = self.overlay.as_mut() {
                    let bar = overlay.input_mut();
                    bar.cursor_visible = !bar.cursor_visible;
                }
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // Loading animation tick: if the animation is active and its
        // scheduled wake time has passed, request a redraw so the next
        // frame advances. `paint_chrome_with` will update
        // `loading_anim_next_wake` for the subsequent tick, or clear it
        // when the OSR buffer becomes usable. This is the only place
        // that drives the animation forward between real input events,
        // consistent with the no-idle-loop Wayland policy.
        if self
            .loading_anim_next_wake
            .is_some_and(|at| Instant::now() >= at)
        {
            self.request_redraw();
        }

        // New-tab splash JS push: when the active tab is `buffr://new`,
        // push the current splash frame's HTML into the page's
        // `<pre id="buffr-splash">` element via execute_javascript. Tick-
        // deduped so a busy loop iteration doesn't spam the renderer; the
        // wake schedule arms the next push at the splash period boundary.
        self.tick_splash_js_push();

        // CEF resize debounce: fire the pending osr_resize once the drag
        // has been quiet for CEF_RESIZE_DEBOUNCE. Each Resized event refreshes
        // the deadline; we only call host.osr_resize once per drag gesture.
        //
        // Recompute dims from the *current* window + chrome state instead of
        // using the (w, h) queued at the original Resized event. Between then
        // and now, chrome layout may have changed (e.g. a download notice
        // expired in `expire_stale_notices` above, which already called
        // `resync_cef_rect` with the correct no-notice dims). Using the queued
        // value would clobber that resync with stale dims, leaving CEF
        // painting at the wrong size and `last_osr_dims != browser_w/h`
        // forever — the loading animation would stay stuck.
        if self.pending_cef_resize.should_fire(Instant::now()) {
            if let Some(window) = self.window.as_ref() {
                let size = window.physical_size();
                let (_, _, w, h) = self.cef_child_rect(size.width.max(1), size.height.max(1));
                tracing::info!(
                    target: "buffr::resize_path",
                    w, h,
                    engines = self.engines.len(),
                    "fire: osr_resize fan-out"
                );
                // Fan out resize to all engines; watchdog tracks active engine.
                for (id, host) in &self.engines {
                    host.osr_resize(w, h);
                    if id == &self.active_engine {
                        self.resize_paint_watchdog.arm(
                            w,
                            h,
                            Instant::now(),
                            RESIZE_PAINT_WATCHDOG_TIMEOUT,
                        );
                    }
                }
                if !self.engines.is_empty() {
                    tracing::debug!(w, h, "wayr: pending Resized debounce elapsed -> osr_resize");
                }
            }
            self.pending_cef_resize.clear();
            self.request_redraw();
        }

        // Popup CEF resize debounce: same logic for each live popup window.
        let popup_ids: Vec<SurfaceId> = self.popups.keys().copied().collect();
        for wid in popup_ids {
            let pending = self.popups.get(&wid).and_then(|p| p.pending_cef_resize);
            if let Some((w, h, at)) = pending
                && Instant::now() >= at
            {
                let (browser_id, pop_scale) = self
                    .popups
                    .get(&wid)
                    .map(|p| (p.browser_id, p.window.scale_factor() as f32))
                    .unwrap_or((-1, 1.0));
                // Same rect the quad is painted into — see popup_cef_rect_pure.
                let (_, _, cef_w, cef_h) = popup_cef_rect_pure(w, h, pop_scale);
                if browser_id >= 0
                    && let Some(engine) = self.active_engine_dyn()
                {
                    engine.popup_resize(browser_id, cef_w, cef_h);
                    tracing::debug!(
                        browser_id,
                        w = cef_w,
                        h = cef_h,
                        "popup: pending Resized debounce elapsed -> popup_resize"
                    );
                }
                if let Some(popup) = self.popups.get_mut(&wid) {
                    popup.pending_cef_resize = None;
                }
                self.request_redraw();
            }

            // Skipped-frame retry for this popup (see AppState::repaint_retry_at).
            if let Some(popup) = self.popups.get_mut(&wid)
                && popup.repaint_retry_at.is_some_and(|t| Instant::now() >= t)
            {
                popup.repaint_retry_at = None;
                popup.window.request_redraw();
            }
        }

        // Resize-paint watchdog: if CEF hasn't produced an on_paint at the
        // expected (post-resize) dims within RESIZE_PAINT_WATCHDOG_TIMEOUT,
        // nudge it via a was_hidden cycle — the same repaint trigger the
        // tab-switch workaround relies on.
        //
        // Guard: skip while sleeping.  `force_repaint_active` cycles
        // was_hidden(1)→was_hidden(0) — if CEF is already was_hidden(1) via
        // osr_sleep, the cycle would wake it spuriously and fight the sleep
        // policy.  The watchdog is harmless to skip while paused because CEF
        // isn't painting anyway and the watchdog's purpose (un-stick a stale
        // paint) is moot.
        if self.paint_policy == PaintPolicy::Active
            && self
                .resize_paint_watchdog
                .should_force_repaint(Instant::now())
        {
            if let Some(host) = self.active_engine_dyn() {
                let r = self.resize_paint_watchdog.retry_count();
                tracing::debug!(retry = r, "watchdog: forcing repaint");
                host.force_repaint_active();
            }
            self.resize_paint_watchdog
                .record_force_repaint(Instant::now(), RESIZE_PAINT_WATCHDOG_TIMEOUT);
            self.request_redraw();
        }

        // Skipped-frame retry: the renderer dropped the last frame (worker
        // still presenting, swapchain texture unavailable, channel full) so
        // the chrome dirty flag is still set and the pixels are still only
        // in `osr_scratch`. Re-request the paint once the throttle window
        // elapses — going through the event-loop deadline instead of
        // request_redraw()-ing straight from the redraw handler is what
        // keeps a wedged render worker from spinning the loop.
        if self.repaint_retry_at.is_some_and(|t| Instant::now() >= t) {
            self.repaint_retry_at = None;
            self.request_redraw();
        }

        // No idle paint loop: we respect Wayland's frame-callback model
        // and only repaint on explicit `request_redraw` (e.g. resize,
        // input, mode/url change). OSR `on_paint` updates that arrive
        // while no redraw is queued show on the next compositor-driven
        // frame.

        // Cap the event-loop wakeup cadence at the display's refresh
        // rate so CEF's message pump (which needs regular service)
        // stops pinning the main thread at 100% CPU. Real-time
        // wakeups (input, OSR on_paint → EventLoopProxy, Resized)
        // preempt the deadline; the deadline only fires when nothing
        // else woke us. wgpu's surface itself runs Fifo (vsync) so
        // render rate is already capped to display refresh; this
        // matches the pump cadence to it.
        //
        // Pull the live refresh rate from wayr's OutputInfo (millihertz
        // → ms-per-frame). Take the FASTEST advertised output's rate
        // so multi-monitor mixes pace to the fastest panel rather
        // than down-clocking to a stale 60Hz default. If no output
        // has reported yet (early startup) fall back to 60 Hz.
        //
        // `event_loop.outputs()` allocates a `Vec<OutputInfo>` with
        // per-output String name/description clones, so it is only
        // re-queried once per second and the cached period is reused
        // for the intervening ticks (the loop otherwise runs at the
        // display's refresh rate).
        let frame_period = {
            let now = Instant::now();
            match self.last_outputs_recompute {
                Some((at, period)) if now.duration_since(at) < Duration::from_secs(1) => period,
                _ => {
                    let outputs = event_loop.outputs();
                    let max_mhz = outputs.iter().map(|o| o.refresh_mhz).max().unwrap_or(0);
                    let period = if max_mhz > 0 {
                        // mhz -> period in ms: 1000 * 1000 / mhz.
                        Duration::from_micros(1_000_000_000 / max_mhz as u64)
                    } else {
                        Duration::from_millis(16)
                    };
                    self.last_outputs_recompute = Some((now, period));
                    period
                }
            }
        };
        let next_wakeup = Instant::now() + frame_period;
        // If CEF has scheduled a pump, wake up no later than that.
        let deadline = match self.cef_next_pump_at {
            Some(at) if at < next_wakeup => at,
            _ => next_wakeup,
        };
        // If the loading animation is active, wake up at ~12 fps so it
        // advances even when no other event arrives. The animation path
        // requests a redraw inside paint_chrome_with, and `WaitUntil`
        // ensures the loop wakes to service it. Clamp to the earliest of
        // the three candidates so we never sleep *past* an animation tick.
        let deadline = match self.loading_anim_next_wake {
            Some(anim_at) if anim_at < deadline => anim_at,
            _ => deadline,
        };
        // If a debounced CEF resize is pending, wake up no later than its
        // deadline so the resize fires promptly after the drag ends.
        let deadline = match self.pending_cef_resize.deadline() {
            Some(resize_at) if resize_at < deadline => resize_at,
            _ => deadline,
        };
        // Same for any pending popup resize.
        let deadline = self
            .popups
            .values()
            .filter_map(|p| p.pending_cef_resize)
            .map(|(_, _, at)| at)
            .fold(deadline, |acc, at| if at < acc { at } else { acc });
        // ...and for any popup frame the renderer skipped.
        let deadline = self
            .popups
            .values()
            .filter_map(|p| p.repaint_retry_at)
            .fold(deadline, |acc, at| if at < acc { at } else { acc });
        // If the resize-paint watchdog is armed, wake up no later than its
        // deadline so the force-repaint nudge fires on time.
        let deadline = match self.resize_paint_watchdog.deadline() {
            Some(at) if at < deadline => at,
            _ => deadline,
        };
        // Occlude debounce: if a debounce is pending, wake up no later
        // than its deadline so the sleep transition fires promptly after
        // the grace window expires (occlude detection without busy-wait).
        let deadline = match self.sleep_deadline {
            Some(at) if at < deadline => at,
            _ => deadline,
        };
        // Media probe: if a probe fire is due, ensure we wake up to dispatch it.
        let deadline = match self.media_probe_next {
            Some(at) if at < deadline => at,
            _ => deadline,
        };
        // Occlusion wake probe: while heuristically sleeping, wake up at
        // the scheduled probe time so we can present once and check whether
        // the compositor is releasing buffers again.
        let deadline = match self.next_probe_at {
            Some(at) if at < deadline => at,
            _ => deadline,
        };
        // Skipped-frame retry: wake up to re-attempt a paint the renderer
        // dropped, so the pending chrome/OSR update isn't stranded until
        // the next unrelated event.
        let deadline = match self.repaint_retry_at {
            Some(at) if at < deadline => at,
            _ => deadline,
        };
        // New-tab splash JS push: clamp wake to the next splash period so
        // the animation advances without input.
        let deadline = match self.splash_js_next_push {
            Some(at) if at < deadline => at,
            _ => deadline,
        };
        // Heartbeat liveness probe: stamp the atomic the background
        // heartbeat thread reads.  No socket write happens here — the
        // bg thread owns the supervisor socket and sends pings on its
        // own 1 Hz timer, so the wakeup deadline does NOT need to be
        // clamped to a heartbeat-driven instant any more.  Drop the
        // handle if the bg thread observed a fatal write error.
        self.tick_heartbeat();
        // Hand the computed deadline to wayr's loop so the next
        // `blocking_pump` is capped at min(50 ms, deadline-now,
        // key-repeat next-fire). Real input still preempts via
        // poll(2); the deadline only takes effect when no socket
        // events arrive sooner. Single-shot — re-arms every
        // about_to_wait iteration.
        event_loop.wait_until(deadline);
    }
}

#[cfg(target_os = "macos")]
fn pump_cef_message_loop(backend: &dyn Backend, next_pump_at: &mut Option<Instant>) {
    if let Some(delay_ms) = backend.scheduled_pump_delay_ms() {
        let delay = Duration::from_millis(delay_ms.try_into().unwrap_or(0));
        let at = Instant::now() + delay;
        tracing::trace!(delay_ms, ?at, "cef: schedule next pump");
        *next_pump_at = Some(at);
    }
    if let Some(at) = *next_pump_at
        && Instant::now() >= at
    {
        tracing::trace!("cef: do_message_loop_work");
        backend.pump_message_loop();
        *next_pump_at = None;
    }
}

#[cfg(not(target_os = "macos"))]
fn pump_cef_message_loop(backend: &dyn Backend, _next_pump_at: &mut Option<Instant>) {
    backend.pump_message_loop();
}
