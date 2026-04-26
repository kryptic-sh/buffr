# Multi-tab architecture

Phase 3 (tab strip) and Phase 5 (tabs / session restore) share one design: the
[`BrowserHost`](../crates/buffr-core/src/host.rs) is a manager owning a
`Vec<Tab>` of CEF browsers. All tabs are parented to the **same** X11 window
(the winit window the embedder constructed); only the active browser is visible.
Switching tabs flips visibility and focus.

## Single `Client`, many `Browser`s

`buffr-core::handlers::make_client` is called once per `open_tab`. Every client
returned from that factory shares the same `Arc<History>`, `Arc<Downloads>`,
`Arc<ZoomStore>`, plus the find / hint mailboxes. This means new visits,
downloads, and zoom rows all funnel into one set of sinks — the chrome doesn't
have to demux per-tab.

Each `Tab` owns its own `cef::Browser` returned from
`browser_host_create_browser_sync`. Tab IDs are minted by the manager (monotonic
`AtomicU64`) and are independent of CEF's own `Browser::identifier()`, which can
collide on close+reopen.

## Tab switching

```rust
prev.host().was_hidden(true);
prev.host().set_focus(false);
next.host().was_hidden(false);
next.host().was_resized();
next.host().set_focus(true);
```

The `was_resized` call exists because hidden browsers don't repaint, and when
they come back the cached size may not match the current chrome geometry.
Calling `was_resized` forces CEF's renderer to re-layout.

### X11 stacking caveat

`was_hidden(true)` is sufficient on XWayland and most X11 compositors — the
embedded X window stops drawing and the now-active sibling becomes the visible
top child. On window managers that aggressively cache sub-window stacking, an
`XRaiseWindow` / `XConfigureWindow` follow-up might be needed; cef-rs 147
doesn't expose that, so we lean on `was_hidden` + `was_resized` and document the
gap rather than vendor xlib bindings.

`set_focus(true)` is enough for keyboard input to route to the new tab — CEF
dispatches synthesized focus events internally when the host's focus bit flips.

## Session restore

On startup `buffr` reads `~/.local/share/buffr/session.json` (resolved via
`directories::ProjectDirs("sh", "kryptic", "buffr").data_dir()`). When the file
exists, the first entry navigates the initial tab; the rest open in the
background. CLI `--new-tab <url>` URLs append after the session list. Each entry
is `{ url, pinned }`; the schema is versioned so a future format bump can ignore
stale files.

```jsonc
{
  "version": 1,
  "tabs": [
    { "url": "https://kryptic.sh", "pinned": false },
    { "url": "https://example.com", "pinned": true },
  ],
}
```

`--no-restore` skips the read (homepage opens in a single tab) and still writes
a fresh session on exit. `--list-session` prints the saved file's entries to
stdout (`*\t<url>` for pinned, `\t<url>` otherwise) and exits without launching
CEF. Schema version is printed on stderr for diagnostic clarity.

### Fresh installs

On the very first launch, `session.json` does not exist. The runtime opens a
single tab loading `general.homepage` from the user's TOML config (default
`about:blank`).

### `:q` semantics

`:q`, `:quit`, and `<C-w>c` all close the **active tab**. Only when the last tab
is closed does the application exit. There is no separate "force-quit the whole
app" command yet — close the OS window.

## Pinned tabs

Pinned tabs are marked with a leading `*` in the tab strip. The flag is purely
informational today: pin does **not** prevent close, only signals sort order to
chrome (the host stores tabs in user-visible order; pin-first sorting is left to
the renderer).

## Private mode

`--private` swaps the on-disk profile dirs for an ephemeral `TempDir`. With
multi-tab, **every** tab in a private launch shares that single temp profile —
there is no per-tab profile mixing. Session restore is skipped under
`--private`; the saved file is not read or rewritten.

## Per-tab session state

`TabSession` (find query + hint session) lives inside each `Tab` and restores
naturally when the tab regains focus. The injected hint JS is scoped to the
active main frame, so other tabs cannot see it. Find-in-page survives tab
switches because the query is stashed on the inactive tab's
`TabSession.find_query`.
