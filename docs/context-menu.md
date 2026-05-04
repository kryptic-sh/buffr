# Right-click context menu

Shipped in v0.4.0 (#23). Replaces Chromium's native context menu with a
custom-rendered `buffr-ui::ContextMenuOverlay` panel, driven by CEF's
`ContextMenuHandler`. Items are bucketed by hit-test type; only the items
relevant to the click target are shown.

## Triggering the menu

Right-click anywhere in the page area. The menu appears at the cursor position
(clamped to the viewport). Dismiss it with `<Esc>`, a click outside the panel,
or by activating an item.

**Navigation in the menu:** `<Up>` / `<Down>` move row selection. `<Enter>`
activates the selected item. Disabled rows (greyed text) cannot be activated.
Any non-navigation key dismisses the menu and is passed to the normal page-mode
dispatcher.

## Bucket priority

When multiple flags apply to a click target, the highest-priority bucket wins:

| Priority | Bucket    | Trigger condition                              |
| -------- | --------- | ---------------------------------------------- |
| 1        | Editable  | `TYPEFLAG_EDITABLE` or `is_editable`           |
| 2        | Link      | `TYPEFLAG_LINK` or a non-empty `link_url`      |
| 3        | Image     | `TYPEFLAG_MEDIA` + `MEDIATYPE_IMAGE`           |
| 4        | Media     | `TYPEFLAG_MEDIA` + `MEDIATYPE_VIDEO/AUDIO`     |
| 5        | Selection | `TYPEFLAG_SELECTION` (and not Editable)        |
| 6        | Page      | Fallback — always shown when no bucket matches |

## Per-bucket items

### Page (fallback)

Shown on a right-click on the page background, margin, or any element that
doesn't match a higher-priority bucket.

| Variant                            | Label            | Notes                                             |
| ---------------------------------- | ---------------- | ------------------------------------------------- |
| `HistoryBack { enabled: bool }`    | Back             | Greyed when there is no previous history entry.   |
| `HistoryForward { enabled: bool }` | Forward          | Greyed when there is no forward history entry.    |
| `Reload`                           | Reload           | Only when the page is not loading.                |
| `StopLoading`                      | Stop Loading     | Only while the page is loading (replaces Reload). |
| `ViewPageSource`                   | View Page Source | Opens `buffr-src:<url>` in a new tab. See below.  |
| `InspectElement`                   | Inspect Element  | Opens DevTools at the right-click hit-point.      |

### Link

Right-clicking a hyperlink (`<a href="...">` or any element with a URL).

| Variant                   | Label                       | Notes                                        |
| ------------------------- | --------------------------- | -------------------------------------------- |
| `OpenLinkInNewTab`        | Open Link in New Tab        | Foreground tab.                              |
| `OpenLinkInBackgroundTab` | Open Link in Background Tab | Background tab (tab strip focus unchanged).  |
| `OpenLinkInNewWindow`     | Open Link in New Window     | Currently treated as a new tab (#18).        |
| `CopyLinkAddress`         | Copy Link Address           | Writes the URL to the clipboard.             |
| `SaveLinkAs`              | Save Link As…               | Triggers a CEF download of the link URL.     |
| `InspectElement`          | Inspect Element             | Opens DevTools at the right-click hit-point. |

### Image

Right-clicking an `<img>` or other image-type media element.

| Variant                  | Label                   | Notes                                                      |
| ------------------------ | ----------------------- | ---------------------------------------------------------- |
| `OpenImageInNewTab`      | Open Image in New Tab   | Navigates to the image URL directly.                       |
| `SaveImageAs`            | Save Image As…          | Triggers a CEF download.                                   |
| `CopyImage`              | Copy Image              | Fetches off-thread, transcodes to PNG, writes clipboard.   |
| `CopyImageAddress`       | Copy Image Address      | Writes the image URL as text to the clipboard.             |
| `RotateClockwise`        | Rotate Clockwise        | Rotates +90° via `el.style.transform`. Composes on repeat. |
| `RotateCounterclockwise` | Rotate Counterclockwise | Rotates −90° via `el.style.transform`. Composes on repeat. |
| `InspectElement`         | Inspect Element         | Opens DevTools at the right-click hit-point.               |

`CopyImage` falls back to writing the image URL as text when the clipboard
backend doesn't support image MIME (e.g. OSC52 over SSH).

### Media (video / audio)

Right-clicking a `<video>` or `<audio>` element. Items whose media-state flag is
not set are omitted (e.g. `MediaSaveAs` only appears when `CAN_SAVE` is
reported; `PictureInPicture` only when `CAN_PICTURE_IN_PICTURE` is reported).

| Variant                            | Label               | Notes                                                            |
| ---------------------------------- | ------------------- | ---------------------------------------------------------------- |
| `MediaPlayPause { playing: bool }` | Pause / Play        | "Pause" when playing; "Play" when paused.                        |
| `MediaMute { muted: bool }`        | Mute / Unmute       | "Unmute" when already muted.                                     |
| `MediaLoop { looped: bool }`       | Enable/Disable Loop | Toggles `<video>.loop`.                                          |
| `MediaShowControls`                | Show Controls       | Toggling native controls; only shown when `CAN_TOGGLE_CONTROLS`. |
| `MediaSaveAs`                      | Save Media As…      | Only shown when `CAN_SAVE` flag is set.                          |
| `CopyMediaAddress`                 | Copy Media Address  | Writes the media URL as text to the clipboard.                   |
| `PictureInPicture`                 | Picture in Picture  | Only shown when `CAN_PICTURE_IN_PICTURE` is set.                 |
| `InspectElement`                   | Inspect Element     | Opens DevTools at the right-click hit-point.                     |

All media actions resolve the target element via `document.elementFromPoint`
with a `querySelector('video, audio')` fallback for sites (e.g. YouTube) where
the click target is a sibling/overlay rather than an ancestor of the media
element.

### Selection

Right-clicking a text selection that is _not_ inside an editable field.

| Variant           | Label                | Notes                                                |
| ----------------- | -------------------- | ---------------------------------------------------- |
| `CopySelection`   | Copy                 | Writes `selection_text` to clipboard (CEF fallback). |
| `SearchSelection` | Search for Selection | Opens the selection as a search query in a new tab.  |
| `InspectElement`  | Inspect Element      | Opens DevTools at the right-click hit-point.         |

### Editable

Right-clicking an editable field (`<input>`, `<textarea>`, contenteditable). All
ops are dispatched as CEF frame edit commands to the focused frame.

| Variant            | Label               | Notes                                 |
| ------------------ | ------------------- | ------------------------------------- |
| `Cut`              | Cut                 | `cef_frame_t` edit command.           |
| `Copy`             | Copy                | `cef_frame_t` edit command.           |
| `Paste`            | Paste               | `cef_frame_t` edit command.           |
| `PasteAsPlainText` | Paste as Plain Text | Strips rich formatting before insert. |
| `SelectAll`        | Select All          | `cef_frame_t` edit command.           |
| `Undo`             | Undo                | `cef_frame_t` edit command.           |
| `Redo`             | Redo                | `cef_frame_t` edit command.           |

## `buffr-src:` URL prefix

`ViewPageSource` navigates to `buffr-src:<url>` — the user-facing alias for
Chromium's `view-source:` scheme. Navigations to `buffr-src:<url>` are rewritten
to `view-source:<url>` at the CEF boundary. The omnibar, tab strip, and session
file all store the `buffr-src:` prefixed form uniformly.

You can also type `buffr-src:https://example.com` directly in the omnibar to
view source without using the menu.

## Known issues

- **`PictureInPicture` no-ops on YouTube.** The item fires
  `media_picture_in_picture`, which calls the browser's PiP API via JS
  injection. YouTube's embedded player rejects PiP calls unless they are
  initiated from a trusted user gesture — CEF's JS injection does not carry
  transient user activation across the OSR boundary. Tracked in
  [issue #31](https://github.com/kryptic-sh/buffr/issues/31).
