//! Pure-Rust context menu data model and sink.
//!
//! [`build_model`] converts raw CEF flag integers into a typed
//! [`Vec<ContextMenuItem>`].  It is intentionally free of any CEF types so
//! it can be unit-tested without a live CEF runtime.
//!
//! [`ContextMenuSink`] is the thread-safe queue that `run_context_menu`
//! pushes [`ContextMenuRequest`]s onto.  The apps layer drains it each tick
//! via [`BrowserHost::drain_context_menu_requests`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ── CEF bit-flag constants (from cef::sys / cef-dll-sys) ─────────────────────

// cef_context_menu_type_flags_t inner values (transparent u32 wrapper)
pub const TYPEFLAG_NONE: u32 = 0;
pub const TYPEFLAG_PAGE: u32 = 1;
pub const TYPEFLAG_FRAME: u32 = 2;
pub const TYPEFLAG_LINK: u32 = 4;
pub const TYPEFLAG_MEDIA: u32 = 8;
pub const TYPEFLAG_SELECTION: u32 = 16;
pub const TYPEFLAG_EDITABLE: u32 = 32;

// cef_context_menu_media_type_t enum values
pub const MEDIATYPE_NONE: u32 = 0;
pub const MEDIATYPE_IMAGE: u32 = 1;
pub const MEDIATYPE_VIDEO: u32 = 2;
pub const MEDIATYPE_AUDIO: u32 = 3;

// cef_context_menu_media_state_flags_t inner values
pub const MEDIAFLAG_NONE: u32 = 0;
pub const MEDIAFLAG_IN_ERROR: u32 = 1;
pub const MEDIAFLAG_PAUSED: u32 = 2;
pub const MEDIAFLAG_MUTED: u32 = 4;
pub const MEDIAFLAG_LOOP: u32 = 8;
pub const MEDIAFLAG_CAN_SAVE: u32 = 16;
pub const MEDIAFLAG_CAN_TOGGLE_CONTROLS: u32 = 64;
pub const MEDIAFLAG_CONTROLS: u32 = 128;
pub const MEDIAFLAG_CAN_PICTURE_IN_PICTURE: u32 = 1024;
pub const MEDIAFLAG_PICTURE_IN_PICTURE: u32 = 2048;

// ── Queue capacity ────────────────────────────────────────────────────────────

/// Maximum number of [`ContextMenuRequest`]s held in the sink at once.
/// When the queue is full the oldest entry is dropped before the new one
/// is pushed, so the UI thread always sees the most-recent requests.
pub const CONTEXT_MENU_REQUEST_QUEUE_CAP: usize = 8;

// ── Item model ────────────────────────────────────────────────────────────────

/// One entry in a context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuItem {
    // ── Navigation (Page bucket) ──────────────────────────────────────────
    HistoryBack {
        enabled: bool,
    },
    HistoryForward {
        enabled: bool,
    },
    Reload,
    StopLoading,
    ViewPageSource,
    InspectElement,

    // ── Selection ─────────────────────────────────────────────────────────
    CopySelection,
    /// Search selected text with the default engine.
    SearchSelection,

    // ── Link ──────────────────────────────────────────────────────────────
    OpenLinkInNewTab,
    OpenLinkInBackgroundTab,
    OpenLinkInNewWindow,
    CopyLinkAddress,
    SaveLinkAs,

    // ── Image ─────────────────────────────────────────────────────────────
    OpenImageInNewTab,
    SaveImageAs,
    CopyImage,
    CopyImageAddress,

    // ── Video / Audio ─────────────────────────────────────────────────────
    MediaPlayPause {
        playing: bool,
    },
    MediaMute {
        muted: bool,
    },
    MediaLoop {
        looped: bool,
    },
    MediaShowControls,
    MediaSaveAs,
    CopyMediaAddress,
    PictureInPicture,

    // ── Tab strip (Tab target) ────────────────────────────────────────────
    /// Reload the right-clicked tab. Selects it first if it isn't active.
    TabReload,
    /// Open a copy of the right-clicked tab's URL in a new tab.
    TabDuplicate,
    /// Toggle the right-clicked tab's pinned state. `pinned` is the
    /// *current* state — label flips to "Pin Tab" / "Unpin Tab".
    TabPin {
        pinned: bool,
    },
    /// Copy the right-clicked tab's URL to the clipboard.
    TabCopyUrl,
    /// Close the right-clicked tab.
    TabClose,
    /// Close every tab except the right-clicked one. Disabled when it is
    /// the only tab. Pinned tabs are skipped (Chrome behaviour).
    TabCloseOthers {
        enabled: bool,
    },
    /// Close every tab to the right of the right-clicked one. Disabled
    /// when it is the last tab. Pinned tabs are skipped.
    TabCloseToRight {
        enabled: bool,
    },

    // ── Editable ──────────────────────────────────────────────────────────
    Cut,
    Copy,
    Paste,
    PasteAsPlainText,
    SelectAll,
    Undo,
    Redo,

    Separator,
}

impl ContextMenuItem {
    /// Human-readable label for the menu item.
    ///
    /// Used by the overlay renderer so label text lives in one place.
    /// `Separator` returns an empty string — callers render it as a
    /// divider line, not as text.
    pub fn label(&self) -> &'static str {
        use ContextMenuItem as I;
        match self {
            I::HistoryBack { .. } => "Back",
            I::HistoryForward { .. } => "Forward",
            I::Reload => "Reload",
            I::StopLoading => "Stop Loading",
            I::ViewPageSource => "View Page Source",
            I::InspectElement => "Inspect Element",
            I::CopySelection => "Copy",
            I::SearchSelection => "Search for Selection",
            I::OpenLinkInNewTab => "Open Link in New Tab",
            I::OpenLinkInBackgroundTab => "Open Link in Background Tab",
            I::OpenLinkInNewWindow => "Open Link in New Window",
            I::CopyLinkAddress => "Copy Link Address",
            I::SaveLinkAs => "Save Link As…",
            I::OpenImageInNewTab => "Open Image in New Tab",
            I::SaveImageAs => "Save Image As…",
            I::CopyImage => "Copy Image",
            I::CopyImageAddress => "Copy Image Address",
            I::MediaPlayPause { playing: true } => "Pause",
            I::MediaPlayPause { playing: false } => "Play",
            I::MediaMute { muted: true } => "Unmute",
            I::MediaMute { muted: false } => "Mute",
            I::MediaLoop { looped: true } => "Disable Loop",
            I::MediaLoop { looped: false } => "Enable Loop",
            I::MediaShowControls => "Show Controls",
            I::MediaSaveAs => "Save Media As…",
            I::CopyMediaAddress => "Copy Media Address",
            I::PictureInPicture => "Picture in Picture",
            I::TabReload => "Reload Tab",
            I::TabDuplicate => "Duplicate Tab",
            I::TabPin { pinned: true } => "Unpin Tab",
            I::TabPin { pinned: false } => "Pin Tab",
            I::TabCopyUrl => "Copy Tab URL",
            I::TabClose => "Close Tab",
            I::TabCloseOthers { .. } => "Close Other Tabs",
            I::TabCloseToRight { .. } => "Close Tabs to the Right",
            I::Cut => "Cut",
            I::Copy => "Copy",
            I::Paste => "Paste",
            I::PasteAsPlainText => "Paste as Plain Text",
            I::SelectAll => "Select All",
            I::Undo => "Undo",
            I::Redo => "Redo",
            I::Separator => "",
        }
    }

    /// Whether this item is a visual separator (non-selectable).
    pub fn is_separator(&self) -> bool {
        matches!(self, ContextMenuItem::Separator)
    }
}

// ── Request ───────────────────────────────────────────────────────────────────

/// What the right-click landed on. `Page` requests come from CEF (a
/// click inside the rendered page); `Tab` requests are synthesised by
/// the apps layer when the click hit a tab in the tab strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextMenuTarget {
    /// A click inside the page content. Default for CEF-originated requests.
    #[default]
    Page,
    /// A click on a tab strip entry. `index` is the tab's slot in the
    /// runtime tab order at the moment the menu opened — dispatch should
    /// tolerate the tab list having changed since.
    Tab { index: usize },
}

/// A right-click that has been translated into a menu model.
///
/// All data is owned — no CEF handles are kept alive. The apps layer can
/// inspect and dispatch the request entirely from Rust without crossing
/// thread boundaries back into CEF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMenuRequest {
    /// Click coordinates in browser-local (CEF) pixel space.
    pub x: i32,
    pub y: i32,
    /// CEF `Browser::identifier()`. Routes dispatch to the right tab.
    /// Unused (`0`) for [`ContextMenuTarget::Tab`] requests.
    pub browser_id: i32,
    /// Ordered list of items to render.
    pub items: Vec<ContextMenuItem>,
    /// What the click landed on. Drives which dispatch arm fires.
    pub target: ContextMenuTarget,
    /// Source data carried so dispatch does not have to re-query CEF.
    /// Empty when the click was not on a link / image / media element.
    pub link_url: String,
    pub source_url: String,
    pub selection_text: String,
}

// ── Sink ──────────────────────────────────────────────────────────────────────

/// Thread-safe queue of [`ContextMenuRequest`]s.
///
/// Produced by [`crate::handlers::BuffrContextMenuHandler`] on the CEF IO
/// thread and drained by the apps layer on the UI thread.
pub type ContextMenuSink = Arc<Mutex<VecDeque<ContextMenuRequest>>>;

/// Construct an empty [`ContextMenuSink`].
pub fn new_context_menu_sink() -> ContextMenuSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Build the ordered item list for a right-click event.
///
/// All inputs are plain integers extracted from CEF's `ContextMenuParams` by
/// the caller.  This keeps the function pure and unit-testable without a live
/// CEF runtime.
///
/// ## Bucket priority (highest → lowest)
///
/// 1. Editable  — `TYPEFLAG_EDITABLE` set
/// 2. Link      — `TYPEFLAG_LINK` set  (wins over Image even if both flags fire)
/// 3. Image     — `MEDIATYPE_IMAGE` and `TYPEFLAG_MEDIA` set
/// 4. Media     — `MEDIATYPE_VIDEO` or `MEDIATYPE_AUDIO` with `TYPEFLAG_MEDIA`
/// 5. Selection — `TYPEFLAG_SELECTION` set (but not Editable)
/// 6. Page      — fallback; always includes navigation items
///
/// Within Page: Reload and StopLoading are mutually exclusive — when
/// `is_loading` is true only `StopLoading` is emitted, otherwise only
/// `Reload`.
#[allow(clippy::too_many_arguments)]
pub fn build_model(
    type_flags: u32,
    media_type: u32,
    media_state_flags: u32,
    is_editable: bool,
    has_link_url: bool,
    has_selection: bool,
    can_go_back: bool,
    can_go_forward: bool,
    is_loading: bool,
) -> Vec<ContextMenuItem> {
    use ContextMenuItem as I;

    let flag = |f: u32| type_flags & f != 0;

    // ── 1. Editable ───────────────────────────────────────────────────────
    if is_editable || flag(TYPEFLAG_EDITABLE) {
        let mut items = vec![I::Cut, I::Copy, I::Paste, I::PasteAsPlainText];
        items.push(I::Separator);
        items.push(I::SelectAll);
        items.push(I::Separator);
        items.push(I::Undo);
        items.push(I::Redo);
        return items;
    }

    // ── 2. Link ───────────────────────────────────────────────────────────
    if flag(TYPEFLAG_LINK) || has_link_url {
        let mut items = vec![
            I::OpenLinkInNewTab,
            I::OpenLinkInBackgroundTab,
            I::OpenLinkInNewWindow,
        ];
        items.push(I::Separator);
        items.push(I::CopyLinkAddress);
        items.push(I::SaveLinkAs);
        items.push(I::Separator);
        items.push(I::InspectElement);
        return items;
    }

    // ── 3. Image ─────────────────────────────────────────────────────────
    if flag(TYPEFLAG_MEDIA) && media_type == MEDIATYPE_IMAGE {
        let mut items = vec![
            I::OpenImageInNewTab,
            I::SaveImageAs,
            I::CopyImage,
            I::CopyImageAddress,
        ];
        items.push(I::Separator);
        items.push(I::InspectElement);
        return items;
    }

    // ── 4. Video / Audio ─────────────────────────────────────────────────
    if flag(TYPEFLAG_MEDIA) && (media_type == MEDIATYPE_VIDEO || media_type == MEDIATYPE_AUDIO) {
        let playing = media_state_flags & MEDIAFLAG_PAUSED == 0
            && media_state_flags & MEDIAFLAG_IN_ERROR == 0;
        let muted = media_state_flags & MEDIAFLAG_MUTED != 0;
        let looped = media_state_flags & MEDIAFLAG_LOOP != 0;
        let can_save = media_state_flags & MEDIAFLAG_CAN_SAVE != 0;
        let can_pip = media_state_flags & MEDIAFLAG_CAN_PICTURE_IN_PICTURE != 0;
        let show_controls = media_state_flags & MEDIAFLAG_CAN_TOGGLE_CONTROLS != 0;

        let mut items = vec![
            I::MediaPlayPause { playing },
            I::MediaMute { muted },
            I::MediaLoop { looped },
        ];
        if show_controls {
            items.push(I::MediaShowControls);
        }
        items.push(I::Separator);
        if can_save {
            items.push(I::MediaSaveAs);
        }
        items.push(I::CopyMediaAddress);
        if can_pip {
            items.push(I::Separator);
            items.push(I::PictureInPicture);
        }
        items.push(I::Separator);
        items.push(I::InspectElement);
        return items;
    }

    // ── 5. Selection ─────────────────────────────────────────────────────
    if (flag(TYPEFLAG_SELECTION) || has_selection) && !is_editable {
        let mut items = vec![I::CopySelection, I::SearchSelection];
        items.push(I::Separator);
        items.push(I::InspectElement);
        return items;
    }

    // ── 6. Page (fallback) ───────────────────────────────────────────────
    let mut items = vec![
        I::HistoryBack {
            enabled: can_go_back,
        },
        I::HistoryForward {
            enabled: can_go_forward,
        },
    ];
    if is_loading {
        items.push(I::StopLoading);
    } else {
        items.push(I::Reload);
    }
    items.push(I::Separator);
    items.push(I::ViewPageSource);
    items.push(I::InspectElement);
    items
}

/// Build the item list for a right-click on a tab strip entry.
///
/// `tab_count` is the total number of open tabs, `tab_index` the slot of
/// the right-clicked tab, and `pinned` its current pinned state. The
/// "close others" / "close to the right" items carry an `enabled` bit so
/// the renderer can dim them when there's nothing they'd close.
///
/// This is the [`ContextMenuTarget::Tab`] counterpart to [`build_model`];
/// the apps layer synthesises the request since tab clicks never reach
/// CEF.
pub fn build_tab_model(tab_count: usize, tab_index: usize, pinned: bool) -> Vec<ContextMenuItem> {
    use ContextMenuItem as I;
    vec![
        I::TabReload,
        I::TabDuplicate,
        I::Separator,
        I::TabPin { pinned },
        I::Separator,
        I::TabCopyUrl,
        I::Separator,
        I::TabClose,
        I::TabCloseOthers {
            enabled: tab_count > 1,
        },
        I::TabCloseToRight {
            enabled: tab_index + 1 < tab_count,
        },
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ContextMenuItem as I;

    fn page_click(
        can_go_back: bool,
        can_go_forward: bool,
        is_loading: bool,
    ) -> Vec<ContextMenuItem> {
        build_model(
            TYPEFLAG_PAGE | TYPEFLAG_FRAME,
            MEDIATYPE_NONE,
            MEDIAFLAG_NONE,
            false,
            false,
            false,
            can_go_back,
            can_go_forward,
            is_loading,
        )
    }

    #[test]
    fn page_click_no_history() {
        let items = page_click(false, false, false);
        assert!(items.contains(&I::HistoryBack { enabled: false }));
        assert!(items.contains(&I::HistoryForward { enabled: false }));
        assert!(items.contains(&I::Reload));
        assert!(!items.contains(&I::StopLoading));
        assert!(items.contains(&I::ViewPageSource));
        assert!(items.contains(&I::InspectElement));
    }

    #[test]
    fn page_click_with_history() {
        let items = page_click(true, true, false);
        assert!(items.contains(&I::HistoryBack { enabled: true }));
        assert!(items.contains(&I::HistoryForward { enabled: true }));
        assert!(items.contains(&I::Reload));
    }

    #[test]
    fn page_click_while_loading() {
        let items = page_click(false, false, true);
        assert!(items.contains(&I::StopLoading));
        assert!(!items.contains(&I::Reload));
    }

    #[test]
    fn selection_gives_copy_and_search() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_SELECTION,
            MEDIATYPE_NONE,
            MEDIAFLAG_NONE,
            false,
            false,
            true,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::CopySelection));
        assert!(items.contains(&I::SearchSelection));
        // Must NOT have page nav items
        assert!(!items.iter().any(|i| matches!(i, I::HistoryBack { .. })));
    }

    #[test]
    fn editable_click() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_EDITABLE,
            MEDIATYPE_NONE,
            MEDIAFLAG_NONE,
            true,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::Cut));
        assert!(items.contains(&I::Copy));
        assert!(items.contains(&I::Paste));
        assert!(items.contains(&I::PasteAsPlainText));
        assert!(items.contains(&I::SelectAll));
        assert!(items.contains(&I::Undo));
        assert!(items.contains(&I::Redo));
        // Must NOT have link or page items
        assert!(!items.contains(&I::OpenLinkInNewTab));
        assert!(!items.iter().any(|i| matches!(i, I::HistoryBack { .. })));
    }

    #[test]
    fn link_click() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_LINK,
            MEDIATYPE_NONE,
            MEDIAFLAG_NONE,
            false,
            true,
            false,
            true,
            false,
            false,
        );
        assert!(items.contains(&I::OpenLinkInNewTab));
        assert!(items.contains(&I::OpenLinkInBackgroundTab));
        assert!(items.contains(&I::OpenLinkInNewWindow));
        assert!(items.contains(&I::CopyLinkAddress));
        assert!(items.contains(&I::SaveLinkAs));
        assert!(items.contains(&I::InspectElement));
        // Must NOT have page navigation items
        assert!(!items.iter().any(|i| matches!(i, I::HistoryBack { .. })));
    }

    #[test]
    fn image_click() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_MEDIA,
            MEDIATYPE_IMAGE,
            MEDIAFLAG_NONE,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::OpenImageInNewTab));
        assert!(items.contains(&I::SaveImageAs));
        assert!(items.contains(&I::CopyImage));
        assert!(items.contains(&I::CopyImageAddress));
        assert!(items.contains(&I::InspectElement));
    }

    #[test]
    fn video_click_playing() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_MEDIA,
            MEDIATYPE_VIDEO,
            MEDIAFLAG_NONE, // not paused → playing
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::MediaPlayPause { playing: true }));
        assert!(items.contains(&I::MediaMute { muted: false }));
        assert!(items.contains(&I::MediaLoop { looped: false }));
    }

    #[test]
    fn video_click_paused() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_MEDIA,
            MEDIATYPE_VIDEO,
            MEDIAFLAG_PAUSED | MEDIAFLAG_MUTED,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::MediaPlayPause { playing: false }));
        assert!(items.contains(&I::MediaMute { muted: true }));
    }

    #[test]
    fn link_wins_over_image() {
        // LINK + MEDIA + IMAGE — link bucket must win
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_LINK | TYPEFLAG_MEDIA,
            MEDIATYPE_IMAGE,
            MEDIAFLAG_NONE,
            false,
            true,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::OpenLinkInNewTab));
        assert!(!items.contains(&I::OpenImageInNewTab));
    }

    #[test]
    fn editable_wins_over_link() {
        // Editable + Link — editable must win
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_LINK | TYPEFLAG_EDITABLE,
            MEDIATYPE_NONE,
            MEDIAFLAG_NONE,
            true,
            true,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::Cut));
        assert!(!items.contains(&I::OpenLinkInNewTab));
    }

    #[test]
    fn reload_vs_stop_mutually_exclusive() {
        let loading = page_click(false, false, true);
        let idle = page_click(false, false, false);

        assert!(loading.contains(&I::StopLoading));
        assert!(!loading.contains(&I::Reload));

        assert!(idle.contains(&I::Reload));
        assert!(!idle.contains(&I::StopLoading));
    }

    #[test]
    fn audio_click() {
        let items = build_model(
            TYPEFLAG_PAGE | TYPEFLAG_MEDIA,
            MEDIATYPE_AUDIO,
            MEDIAFLAG_NONE,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(items.contains(&I::MediaPlayPause { playing: true }));
        assert!(items.contains(&I::CopyMediaAddress));
    }

    // ── Tab strip menu ───────────────────────────────────────────────────

    #[test]
    fn tab_model_middle_tab_all_enabled() {
        // 3 tabs, right-clicked the middle one (index 1).
        let items = build_tab_model(3, 1, false);
        assert!(items.contains(&I::TabReload));
        assert!(items.contains(&I::TabDuplicate));
        assert!(items.contains(&I::TabPin { pinned: false }));
        assert!(items.contains(&I::TabCopyUrl));
        assert!(items.contains(&I::TabClose));
        assert!(items.contains(&I::TabCloseOthers { enabled: true }));
        assert!(items.contains(&I::TabCloseToRight { enabled: true }));
        // No page/link/media items leaked in.
        assert!(!items.iter().any(|i| matches!(i, I::HistoryBack { .. })));
        assert!(!items.contains(&I::Reload));
    }

    #[test]
    fn tab_model_only_tab_disables_close_others_and_right() {
        let items = build_tab_model(1, 0, false);
        assert!(items.contains(&I::TabCloseOthers { enabled: false }));
        assert!(items.contains(&I::TabCloseToRight { enabled: false }));
    }

    #[test]
    fn tab_model_last_tab_disables_close_to_right() {
        // 3 tabs, right-clicked the last one (index 2).
        let items = build_tab_model(3, 2, false);
        assert!(items.contains(&I::TabCloseOthers { enabled: true }));
        assert!(items.contains(&I::TabCloseToRight { enabled: false }));
    }

    #[test]
    fn tab_model_pinned_flag_flows_through() {
        assert!(build_tab_model(2, 0, true).contains(&I::TabPin { pinned: true }));
        assert!(build_tab_model(2, 0, false).contains(&I::TabPin { pinned: false }));
    }

    #[test]
    fn tab_pin_labels_flip() {
        assert_eq!(I::TabPin { pinned: true }.label(), "Unpin Tab");
        assert_eq!(I::TabPin { pinned: false }.label(), "Pin Tab");
    }

    #[test]
    fn context_menu_target_defaults_to_page() {
        assert_eq!(ContextMenuTarget::default(), ContextMenuTarget::Page);
    }
}
