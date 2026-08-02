//! Right-click context menu: item construction, active-menu state,
//! and the actions each item dispatches.
//!
//! The menu is built from a neutral `buffr_engine::ContextMenuRequest`
//! rather than a CEF type so the same code serves every engine
//! backend. [`ActiveContextMenu`] holds the open menu's request plus
//! the keyboard-selected row.
//!
//! Uses the `crate::*` glob for the same reason as
//! [`crate::event_loop`]: the `AppState` methods here reach broadly
//! into the crate root until `AppState` itself moves out.

use crate::*;

/// Build the item list for a CEF page right-click from a neutral
/// `buffr_engine::ContextMenuRequest`. Uses the same `buffr_core::build_model`
/// function as the CEF handler, reconstructing the necessary flags from the
/// neutral fields:
///   - `is_editable` → editable bucket (cut / copy / paste / …)
///   - `link_url.is_some()` → link bucket
///   - `media_type == Image` or `has_image_contents` → image bucket
///   - `media_type == Video | Audio` → media bucket (limited state info)
///   - `selection_text.is_some()` → selection bucket
///   - else → page bucket (back / forward / reload / view-source / inspect)
///
/// The CEF media-state flags (muted, looped, PiP, etc.) are not available in
/// the neutral type — the media bucket items appear but without dynamic state.
pub(crate) fn build_context_menu_items_from_neutral(
    req: &buffr_engine::ContextMenuRequest,
    can_go_back: bool,
    can_go_forward: bool,
    is_loading: bool,
) -> Vec<ContextMenuItem> {
    use buffr_core::context_menu::{
        MEDIATYPE_AUDIO, MEDIATYPE_IMAGE, MEDIATYPE_NONE, MEDIATYPE_VIDEO, TYPEFLAG_EDITABLE,
        TYPEFLAG_LINK, TYPEFLAG_MEDIA, TYPEFLAG_PAGE, TYPEFLAG_SELECTION,
    };
    use buffr_engine::types::MediaType;

    let type_flags: u32 = if req.is_editable {
        TYPEFLAG_EDITABLE
    } else if req.link_url.is_some() {
        TYPEFLAG_LINK
    } else if req.has_image_contents
        || matches!(
            req.media_type,
            MediaType::Image | MediaType::Video | MediaType::Audio
        )
    {
        TYPEFLAG_MEDIA
    } else if req.selection_text.is_some() {
        TYPEFLAG_SELECTION
    } else {
        TYPEFLAG_PAGE
    };

    let media_type_raw: u32 = match req.media_type {
        MediaType::Image => MEDIATYPE_IMAGE,
        MediaType::Video => MEDIATYPE_VIDEO,
        MediaType::Audio => MEDIATYPE_AUDIO,
        _ => MEDIATYPE_NONE,
    };

    buffr_core::build_context_menu_model(
        type_flags,
        media_type_raw,
        0, // media_state_flags: no state info in neutral type
        req.is_editable,
        req.link_url.is_some(),
        req.selection_text.is_some(),
        can_go_back,
        can_go_forward,
        is_loading,
    )
}

/// Active right-click context menu state.
///
/// Holds the most-recent [`ContextMenuRequest`] and the keyboard-selected
/// row index. Created when a right-click request arrives; destroyed on
/// Esc, Enter, or click-outside.
pub(crate) struct ActiveContextMenu {
    pub(crate) request: ContextMenuRequest,
    /// Index into `request.items` of the currently highlighted row.
    /// Always points at a non-separator item; updated by Up/Down.
    pub(crate) selected: usize,
}

impl ActiveContextMenu {
    pub(crate) fn new(request: ContextMenuRequest) -> Self {
        // Pre-select the first selectable item.
        let selected = request
            .items
            .iter()
            .position(|i| !i.is_separator())
            .unwrap_or(0);
        Self { request, selected }
    }

    /// Move selection up, skipping separators. No-op at the first item.
    pub(crate) fn select_prev(&mut self) {
        let items = &self.request.items;
        let mut idx = self.selected;
        loop {
            if idx == 0 {
                break;
            }
            idx -= 1;
            if !items[idx].is_separator() {
                self.selected = idx;
                break;
            }
        }
    }

    /// Move selection down, skipping separators. No-op at the last item.
    pub(crate) fn select_next(&mut self) {
        let items = &self.request.items;
        let mut idx = self.selected;
        loop {
            if idx + 1 >= items.len() {
                break;
            }
            idx += 1;
            if !items[idx].is_separator() {
                self.selected = idx;
                break;
            }
        }
    }

    /// Whether the item at `idx` should accept activation. Mirrors the
    /// `enabled` derivation in [`Self::to_overlay`].
    pub(crate) fn is_enabled(&self, idx: usize) -> bool {
        match self.request.items.get(idx) {
            Some(ContextMenuItem::HistoryBack { enabled }) => *enabled,
            Some(ContextMenuItem::HistoryForward { enabled }) => *enabled,
            Some(ContextMenuItem::TabCloseOthers { enabled }) => *enabled,
            Some(ContextMenuItem::TabCloseToRight { enabled }) => *enabled,
            Some(item) => !item.is_separator(),
            None => false,
        }
    }

    /// Build the `ContextMenuOverlay` snapshot for the renderer.
    pub(crate) fn to_overlay(&self, _win_w: u32, win_h: u32) -> ContextMenuOverlay {
        let entries: Vec<ContextMenuEntry> = self
            .request
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| ContextMenuEntry {
                label: item.label().to_string(),
                is_separator: item.is_separator(),
                enabled: self.is_enabled(idx),
            })
            .collect();

        // Convert CEF click coords (browser-local pixels) to chrome buffer
        // coords. CEF OSR pixel space == logical window pixels on 1× HiDPI;
        // at 2× CEF sends doubled values. We use the raw coords and clamp
        // — the overlay widget itself clamps to the buffer bounds.
        let x = self.request.x;
        let y = self.request.y.clamp(0, win_h as i32);

        ContextMenuOverlay {
            entries,
            selected: self.selected,
            x,
            y,
        }
    }
}

impl AppState {
    /// Dismiss the active context menu and repaint.
    pub(crate) fn dismiss_context_menu(&mut self) {
        if self.context_menu.take().is_some() {
            self.mark_chrome_dirty();
            self.request_redraw();
        }
    }

    /// Dispatch the action for a context-menu item activation.
    ///
    /// Called from both the Enter-key path and the mouse-click path after the
    /// menu has been taken (dropped). Logs at INFO so activations are grep-able
    /// with `RUST_LOG=buffr::context_menu=info`.
    pub(crate) fn dispatch_context_menu_item(
        &mut self,
        item: &ContextMenuItem,
        request: &ContextMenuRequest,
    ) {
        use ContextMenuItem as I;

        // ── Navigation ────────────────────────────────────────────────────────
        match item {
            I::HistoryBack { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "history_back", "dispatch");
                self.dispatch_action(&buffr_modal::PageAction::HistoryBack);
            }
            I::HistoryForward { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "history_forward", "dispatch");
                self.dispatch_action(&buffr_modal::PageAction::HistoryForward);
            }
            I::Reload => {
                tracing::info!(target: "buffr::context_menu", action = "reload", "dispatch");
                self.dispatch_action(&buffr_modal::PageAction::Reload);
            }
            I::StopLoading => {
                tracing::info!(target: "buffr::context_menu", action = "stop_loading", "dispatch");
                self.dispatch_action(&buffr_modal::PageAction::StopLoading);
            }

            // ── Edit (frame ops) ─────────────────────────────────────────────
            I::Undo => {
                tracing::info!(target: "buffr::context_menu", action = "undo", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_undo();
                }
            }
            I::Redo => {
                tracing::info!(target: "buffr::context_menu", action = "redo", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_redo();
                }
            }
            I::Cut => {
                tracing::info!(target: "buffr::context_menu", action = "cut", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_cut();
                }
            }
            I::Copy => {
                tracing::info!(target: "buffr::context_menu", action = "copy", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_copy();
                }
            }
            I::Paste => {
                tracing::info!(target: "buffr::context_menu", action = "paste", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_paste();
                }
            }
            I::PasteAsPlainText => {
                tracing::info!(target: "buffr::context_menu", action = "paste_plain", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_paste_plain();
                }
            }
            I::SelectAll => {
                tracing::info!(target: "buffr::context_menu", action = "select_all", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.frame_select_all();
                }
            }

            // ── Selection ────────────────────────────────────────────────────
            I::CopySelection => {
                tracing::info!(target: "buffr::context_menu", action = "copy_selection", "dispatch");
                // selection_text is already extracted by CEF into the request.
                let text = request.selection_text.clone();
                if !text.is_empty() {
                    // Fast path: write text directly to clipboard.
                    if let Some(engine) = self.active_engine_dyn()
                        && !engine.clipboard_set_text(&text)
                    {
                        tracing::warn!(
                            target: "buffr::context_menu",
                            "copy_selection: clipboard write failed"
                        );
                    }
                } else {
                    // Fallback: ask the engine to copy the page selection.
                    if let Some(engine) = self.active_engine_dyn() {
                        engine.frame_copy();
                    }
                }
            }
            I::SearchSelection => {
                tracing::info!(target: "buffr::context_menu", action = "search_selection", "dispatch");
                let query = request.selection_text.trim().to_string();
                if query.is_empty() {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        "search_selection: empty selection text"
                    );
                    return;
                }
                let url = buffr_config::search::resolve_input(&query, &self.search_config);
                if let Some(engine) = self.active_engine_dyn()
                    && let Err(err) = engine.open_tab(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        error = %err,
                        "search_selection: open_tab failed"
                    );
                }
            }

            // ── Link ─────────────────────────────────────────────────────────
            I::OpenLinkInNewTab => {
                tracing::info!(target: "buffr::context_menu", action = "open_link_new_tab", "dispatch");
                let url = request.link_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn()
                    && let Err(err) = engine.open_tab(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        error = %err,
                        "open_link_new_tab: open_tab failed"
                    );
                }
            }
            I::OpenLinkInBackgroundTab => {
                tracing::info!(target: "buffr::context_menu", action = "open_link_background_tab", "dispatch");
                let url = request.link_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn()
                    && let Err(err) = engine.open_tab_background(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        error = %err,
                        "open_link_background_tab: open_tab_background failed"
                    );
                }
            }
            I::OpenLinkInNewWindow => {
                // Multi-window is issue #18 — treat as new tab for now.
                tracing::info!(
                    target: "buffr::context_menu",
                    action = "open_link_new_window",
                    "dispatch (treated as new tab — multi-window is #18)"
                );
                let url = request.link_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn()
                    && let Err(err) = engine.open_tab(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        error = %err,
                        "open_link_new_window: open_tab failed"
                    );
                }
            }
            I::CopyLinkAddress => {
                tracing::info!(target: "buffr::context_menu", action = "copy_link_address", "dispatch");
                let url = request.link_url.clone();
                if let Some(engine) = self.active_engine_dyn()
                    && !engine.clipboard_set_text(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        "copy_link_address: clipboard write failed"
                    );
                }
            }
            I::SaveLinkAs => {
                tracing::info!(target: "buffr::context_menu", action = "save_link_as", "dispatch");
                let url = request.link_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn() {
                    engine.start_download(&url);
                }
            }

            // ── Image ────────────────────────────────────────────────────────
            I::OpenImageInNewTab => {
                tracing::info!(target: "buffr::context_menu", action = "open_image_new_tab", "dispatch");
                let url = request.source_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(host) = self.active_engine_dyn()
                    && let Err(err) = host.open_tab(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        error = %err,
                        "open_image_new_tab: open_tab failed"
                    );
                }
            }
            I::CopyImageAddress => {
                tracing::info!(target: "buffr::context_menu", action = "copy_image_address", "dispatch");
                let url = request.source_url.clone();
                if let Some(engine) = self.active_engine_dyn()
                    && !engine.clipboard_set_text(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        "copy_image_address: clipboard write failed"
                    );
                }
            }
            I::CopyImage => {
                tracing::info!(target: "buffr::context_menu", action = "copy_image", "dispatch");
                let url = request.source_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn() {
                    // Spawns a worker; logs success / fallback / failure.
                    engine.copy_image_url_to_clipboard(&url);
                }
            }
            I::SaveImageAs => {
                tracing::info!(target: "buffr::context_menu", action = "save_image_as", "dispatch");
                let url = request.source_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn() {
                    engine.start_download(&url);
                }
            }

            // ── Page ─────────────────────────────────────────────────────────
            I::ViewPageSource => {
                tracing::info!(target: "buffr::context_menu", action = "view_page_source", "dispatch");
                let current_url = self
                    .active_engine_dyn()
                    .map(|e| e.active_tab_live_url())
                    .unwrap_or_default();
                if current_url.is_empty() {
                    return;
                }
                let view_src_url = format!("buffr-src:{current_url}");
                if let Some(host) = self.active_engine_dyn()
                    && let Err(err) = host.open_tab(&view_src_url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        error = %err,
                        "view_page_source: open_tab failed"
                    );
                }
            }
            I::InspectElement => {
                tracing::info!(target: "buffr::context_menu", action = "inspect_element", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.show_dev_tools_at(request.x, request.y);
                }
            }

            // ── Media ────────────────────────────────────────────────────────
            I::MediaPlayPause { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "media_play_pause", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.media_play_pause(request.x, request.y);
                }
            }
            I::MediaMute { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "media_mute", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.media_toggle_mute(request.x, request.y);
                }
            }
            I::MediaLoop { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "media_loop", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.media_toggle_loop(request.x, request.y);
                }
            }
            I::MediaShowControls => {
                tracing::info!(target: "buffr::context_menu", action = "media_show_controls", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.media_toggle_controls(request.x, request.y);
                }
            }
            I::PictureInPicture => {
                tracing::info!(target: "buffr::context_menu", action = "picture_in_picture", "dispatch");
                if let Some(engine) = self.active_engine_dyn() {
                    engine.media_picture_in_picture(request.x, request.y);
                }
            }
            I::MediaSaveAs => {
                tracing::info!(target: "buffr::context_menu", action = "media_save_as", "dispatch");
                let url = request.source_url.clone();
                if url.is_empty() {
                    return;
                }
                if let Some(engine) = self.active_engine_dyn() {
                    engine.start_download(&url);
                }
            }
            I::CopyMediaAddress => {
                tracing::info!(target: "buffr::context_menu", action = "copy_media_address", "dispatch");
                let url = request.source_url.clone();
                if let Some(engine) = self.active_engine_dyn()
                    && !engine.clipboard_set_text(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        "copy_media_address: clipboard write failed"
                    );
                }
            }

            // ── Tab strip ────────────────────────────────────────────────────
            I::TabReload => {
                tracing::info!(target: "buffr::context_menu", action = "tab_reload", "dispatch");
                if let Some((_, id, _, _)) = self.resolve_tab_target(request)
                    && let Some(host) = self.active_engine_dyn()
                {
                    // Focus the tab first so the active-tab reload hits it.
                    host.select_tab(id);
                    self.on_tab_switch();
                    self.close_overlay();
                    self.refresh_tab_strip();
                    self.dispatch_action(&buffr_modal::PageAction::Reload);
                }
            }
            I::TabDuplicate => {
                tracing::info!(target: "buffr::context_menu", action = "tab_duplicate", "dispatch");
                if let Some((index, _, url, _)) = self.resolve_tab_target(request)
                    && !url.is_empty()
                    && let Some(host) = self.active_engine_dyn()
                {
                    // Insert the copy right after the source tab, Chrome-style.
                    if let Err(err) = host.open_tab_at(&url, index + 1) {
                        tracing::warn!(
                            target: "buffr::context_menu",
                            error = %err,
                            "tab_duplicate: open_tab_at failed"
                        );
                    }
                    self.refresh_tab_strip();
                    self.mark_session_dirty();
                }
            }
            I::TabPin { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "tab_pin", "dispatch");
                if let Some((_, id, _, pinned)) = self.resolve_tab_target(request)
                    && let Some(host) = self.active_engine_dyn()
                {
                    host.set_pinned(id, !pinned);
                    self.refresh_tab_strip();
                    self.mark_session_dirty();
                }
            }
            I::TabCopyUrl => {
                tracing::info!(target: "buffr::context_menu", action = "tab_copy_url", "dispatch");
                if let Some((_, _, url, _)) = self.resolve_tab_target(request)
                    && let Some(engine) = self.active_engine_dyn()
                    && !engine.clipboard_set_text(&url)
                {
                    tracing::warn!(
                        target: "buffr::context_menu",
                        "tab_copy_url: clipboard write failed"
                    );
                }
            }
            I::TabClose => {
                tracing::info!(target: "buffr::context_menu", action = "tab_close", "dispatch");
                if let Some((_, id, _, pinned)) = self.resolve_tab_target(request) {
                    if pinned && self.confirm_close_pinned.is_none() {
                        // Mirror the middle-click guard: don't let a
                        // pinned tab be lost from a context-menu misaim.
                        self.confirm_close_pinned = Some(id);
                        self.request_redraw();
                        return;
                    }
                    let remaining = if let Some(host) = self.active_engine_dyn() {
                        let _ = host.close_tab(id);
                        host.tab_count()
                    } else {
                        0
                    };
                    self.refresh_tab_strip();
                    if remaining == 0 {
                        self.save_session_now();
                        self.mark_clean_shutdown();
                        // Graceful exit — see close_active_tab_or_exit.
                        self.shutdown_flag.store(true, Ordering::SeqCst);
                        self.request_redraw();
                    }
                    self.mark_session_dirty();
                }
            }
            I::TabCloseOthers { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "tab_close_others", "dispatch");
                if let Some((_, keep_id, _, _)) = self.resolve_tab_target(request) {
                    // Snapshot (id, pinned) before closing — closing
                    // shifts indices. Skip the kept tab and any pinned
                    // tabs (Chrome leaves pinned tabs alone).
                    let victims: Vec<TabId> = self
                        .active_engine_dyn()
                        .map(|e| {
                            e.tabs_summary()
                                .into_iter()
                                .filter(|t| t.id != keep_id && !t.pinned)
                                .map(|t| t.id)
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(host) = self.active_engine_dyn() {
                        for id in victims {
                            let _ = host.close_tab(id);
                        }
                        host.select_tab(keep_id);
                        self.on_tab_switch();
                    }
                    self.refresh_tab_strip();
                    self.mark_session_dirty();
                }
            }
            I::TabCloseToRight { .. } => {
                tracing::info!(target: "buffr::context_menu", action = "tab_close_to_right", "dispatch");
                if let Some((index, _, _, _)) = self.resolve_tab_target(request) {
                    // Snapshot ids for slots strictly after the target,
                    // skipping pinned tabs, before any closure shifts
                    // indices.
                    let victims: Vec<TabId> = self
                        .active_engine_dyn()
                        .map(|e| {
                            e.tabs_summary()
                                .into_iter()
                                .enumerate()
                                .filter(|(i, t)| *i > index && !t.pinned)
                                .map(|(_, t)| t.id)
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(host) = self.active_engine_dyn() {
                        for id in victims {
                            let _ = host.close_tab(id);
                        }
                    }
                    self.refresh_tab_strip();
                    self.mark_session_dirty();
                }
            }

            I::Separator => {
                // Separators are never activated (disabled in is_enabled).
            }
        }
    }

    /// Resolve a [`ContextMenuTarget::Tab`] request to
    /// `(slot_index, tab_id, url, pinned)` against the *current* tab
    /// list. Returns `None` if the request isn't a tab target or the
    /// recorded slot no longer exists (the tab list changed since the
    /// menu opened).
    pub(crate) fn resolve_tab_target(
        &self,
        request: &ContextMenuRequest,
    ) -> Option<(usize, TabId, String, bool)> {
        let ContextMenuTarget::Tab { index } = request.target else {
            return None;
        };
        let host = self.active_engine_dyn()?;
        let summaries = host.tabs_summary();
        let t = summaries.get(index)?;
        Some((index, t.id, t.url.clone(), t.pinned))
    }

    /// Route a wayr `KeyEvent` to the open context menu. Returns `true`
    /// if the event was consumed (caller skips all other key sinks).
    ///
    /// Up/Down move selection. Enter activates + dismisses. Esc dismisses.
    /// Any other key dismisses and returns `false` so the key still reaches
    /// the normal page-mode dispatcher.
    pub(crate) fn context_menu_handle_key(&mut self, event: &crate::windowing::KeyEvent) -> bool {
        if self.context_menu.is_none() {
            return false;
        }
        // Only handle key-press, not release.
        if event.state != crate::windowing::KeyState::Pressed {
            return true; // swallow release events while menu is open
        }
        let Some(chord) = key_event_to_chord_with_repeat(event) else {
            return true; // swallow unmappable keys
        };
        let key = chord.key;
        match key {
            Key::Named(NamedKey::Esc) => {
                self.dismiss_context_menu();
                true
            }
            Key::Named(NamedKey::Up) => {
                if let Some(cm) = self.context_menu.as_mut() {
                    cm.select_prev();
                }
                self.mark_chrome_dirty();
                self.request_redraw();
                true
            }
            Key::Named(NamedKey::Down) => {
                if let Some(cm) = self.context_menu.as_mut() {
                    cm.select_next();
                }
                self.mark_chrome_dirty();
                self.request_redraw();
                true
            }
            Key::Named(NamedKey::CR) => {
                if let Some(cm) = self.context_menu.as_ref()
                    && cm.is_enabled(cm.selected)
                {
                    let cm = self.context_menu.take().unwrap();
                    let item = cm.request.items[cm.selected].clone();
                    let request = cm.request.clone();
                    self.dispatch_context_menu_item(&item, &request);
                    self.mark_chrome_dirty();
                    self.request_redraw();
                }
                // Disabled-on-Enter: keep the menu open, ignore the key.
                true
            }
            _ => {
                // Any other key dismisses the menu but lets the key through.
                self.dismiss_context_menu();
                false
            }
        }
    }
}
