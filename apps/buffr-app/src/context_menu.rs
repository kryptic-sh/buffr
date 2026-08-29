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
    /// Pre-resolved overlay entries (label Strings, separator and enabled
    /// flags). Built once when the menu opens — the item list cannot
    /// change while it is open — so per-event hit-testing reuses this
    /// instead of re-cloning every label on each mouse move.
    entries: Vec<ContextMenuEntry>,
    /// Panel width in DIPs, measured once from `entries` (perf §14-15).
    /// Per-mouse-move contains/hit_test reuse it instead of re-measuring
    /// every label with a glyph-lock lookup each.
    panel_w: u32,
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
        let entries = Self::build_entries(&request);
        let panel_w = ContextMenuOverlay::preferred_width_for(&entries);
        Self {
            request,
            entries,
            panel_w,
            selected,
        }
    }

    /// Resolve the overlay entries for a request. A pure function of the
    /// request — see the [`Self::entries`] field doc for why it is
    /// computed once.
    fn build_entries(request: &ContextMenuRequest) -> Vec<ContextMenuEntry> {
        request
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| ContextMenuEntry {
                label: item.label().to_string(),
                is_separator: item.is_separator(),
                enabled: Self::item_enabled(&request.items, idx),
            })
            .collect()
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
        Self::item_enabled(&self.request.items, idx)
    }

    fn item_enabled(items: &[ContextMenuItem], idx: usize) -> bool {
        match items.get(idx) {
            Some(ContextMenuItem::HistoryBack { enabled }) => *enabled,
            Some(ContextMenuItem::HistoryForward { enabled }) => *enabled,
            Some(ContextMenuItem::TabCloseOthers { enabled }) => *enabled,
            Some(ContextMenuItem::TabCloseToRight { enabled }) => *enabled,
            Some(item) => !item.is_separator(),
            None => false,
        }
    }

    /// Build the `ContextMenuOverlay` snapshot for the renderer.
    ///
    /// Called once per paint. Per-event hit-testing uses
    /// [`Self::contains`] / [`Self::hit_test`] on the cached entries
    /// instead, so the label Strings are not re-cloned on every mouse
    /// move.
    pub(crate) fn to_overlay(&self, _win_w: u32, win_h: u32) -> ContextMenuOverlay {
        // Convert CEF click coords (browser-local pixels) to chrome buffer
        // coords. CEF OSR pixel space == logical window pixels on 1× HiDPI;
        // at 2× CEF sends doubled values. We use the raw coords and clamp
        // — the overlay widget itself clamps to the buffer bounds.
        let x = self.request.x;
        let y = self.request.y.clamp(0, win_h as i32);

        // Panel width was already measured once when the menu opened
        // (perf §14-15); reuse it here so `paint` doesn't re-walk every
        // label per dirty frame (perf §22 C-P1).
        let panel_w = self.panel_w;
        let panel_h = ContextMenuOverlay::preferred_height_for(&self.entries);
        ContextMenuOverlay {
            entries: self.entries.clone(),
            selected: self.selected,
            x,
            y,
            panel_w,
            panel_h,
        }
    }

    /// Whether pixel `(x, y)` (logical chrome-buffer coords) falls inside
    /// the open menu's clamped panel. Cheap: reuses the cached entries,
    /// no overlay / label clones.
    pub(crate) fn contains(&self, buf_w: usize, buf_h: usize, x: i32, y: i32) -> bool {
        ContextMenuOverlay::contains_at(
            &self.entries,
            self.panel_w,
            self.request.x,
            self.request.y,
            buf_w,
            buf_h,
            x,
            y,
        )
    }

    /// Resolve pixel `(x, y)` to a row index in the open menu, or `None`
    /// for separators, the border, or outside the panel. Same geometry as
    /// the overlay the renderer paints; reuses the cached entries so a
    /// mouse move costs no String allocations.
    pub(crate) fn hit_test(&self, buf_w: usize, buf_h: usize, x: i32, y: i32) -> Option<usize> {
        ContextMenuOverlay::hit_test(
            &self.entries,
            self.panel_w,
            self.request.x,
            self.request.y,
            buf_w,
            buf_h,
            x,
            y,
        )
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
                // Use the URL CEF actually navigated, not the display URL:
                // on a `buffr://` page the display form is `buffr://new`,
                // which the buffr-src: gate rejects (not http(s)); the
                // loopback form is what the same-host exception covers.
                let current_url = self
                    .active_engine_dyn()
                    .map(|e| e.active_tab_cef_url())
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
                    // Phase 3: "last tab" exit must check across ALL
                    // engines — closing the active engine's last tab while
                    // another engine still has tabs is not an exit.
                    let remaining: usize = self.engines.values().map(|e| e.tab_count()).sum();
                    self.refresh_tab_strip();
                    if remaining == 0 {
                        self.request_exit();
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
    /// recorded tab is gone.
    ///
    /// The tab is located by the id captured when the menu opened, never
    /// by the recorded slot — a background `window.open` or another close
    /// can shift indices while the menu is open, and acting on the slot
    /// would fire against the wrong tab (review §10-7). The returned
    /// index is the tab's *current* position, for the close-to-the-right
    /// arm.
    pub(crate) fn resolve_tab_target(
        &self,
        request: &ContextMenuRequest,
    ) -> Option<(usize, TabId, String, bool)> {
        let ContextMenuTarget::Tab { id, .. } = request.target else {
            return None;
        };
        let host = self.active_engine_dyn()?;
        let summaries = host.tabs_summary();
        let (index, t) = locate_tab_by_id(&summaries, id)?;
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

/// Locate a tab by id in a summary list, returning its current slot.
///
/// Used by [`AppState::resolve_tab_target`] so context-menu tab actions
/// re-locate the clicked tab instead of trusting a slot index that may
/// have shifted since the menu opened (review §10-7).
fn locate_tab_by_id(
    summaries: &[buffr_engine::TabSummary],
    id: u64,
) -> Option<(usize, &buffr_engine::TabSummary)> {
    summaries.iter().enumerate().find(|(_, t)| t.id.0 == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_tab_by_id_ignores_slot_index() {
        let summaries = vec![
            buffr_engine::TabSummary {
                id: TabId(10),
                browser_id: 1,
                title: "a".into(),
                url: "https://a.example/".into(),
                progress: 1.0,
                is_loading: false,
                pinned: false,
                private: false,
            },
            buffr_engine::TabSummary {
                id: TabId(20),
                browser_id: 2,
                title: "b".into(),
                url: "https://b.example/".into(),
                progress: 1.0,
                is_loading: false,
                pinned: false,
                private: false,
            },
            buffr_engine::TabSummary {
                id: TabId(30),
                browser_id: 3,
                title: "c".into(),
                url: "https://c.example/".into(),
                progress: 1.0,
                is_loading: false,
                pinned: true,
                private: false,
            },
        ];
        // The menu opened on the tab at slot 1 (id 20); a background
        // open inserted a tab at slot 1. The lookup must follow the id.
        let (idx, t) = locate_tab_by_id(&summaries, 20).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(t.id, TabId(20));
        let (idx, t) = locate_tab_by_id(&summaries, 30).unwrap();
        assert_eq!(idx, 2);
        assert!(t.pinned);
        // A tab closed while the menu was open resolves to nothing — the
        // action is dropped, never misaimed onto a different tab.
        assert!(locate_tab_by_id(&summaries, 99).is_none());
    }
}
