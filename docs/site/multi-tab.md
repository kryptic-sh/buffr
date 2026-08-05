# Multi-tab architecture

[`BrowserHost`](../../crates/buffr-cef/src/host.rs) is a manager owning a
`Vec<Tab>` of CEF browsers. All tabs belong to the **same** window (the winit
window the embedder constructed); only the active browser is visible. Switching
tabs flips visibility and focus.

## Single `Client`, many `Browser`s

`buffr_cef::handlers::make_client` is called once per `open_tab`. Every client
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

There is no native child-window stacking to manage: every tab renders off-screen
(`windowless_rendering_enabled = 1`) and the app composites the active tab's
buffer itself, so visibility is entirely a matter of which buffer gets drawn.
See [`ui-stack.md`](./ui-stack.md).

`set_focus(true)` is enough for keyboard input to route to the new tab — CEF
dispatches synthesized focus events internally when the host's focus bit flips.

## Session restore

On startup `buffr` reads `~/.local/share/buffr/session.json` (resolved via
`hjkl_config::data_dir` — XDG on every platform, `buffr-debug` in debug builds).
When the file exists, the first entry navigates the initial tab; the rest open
in the background. CLI `--new-tab <url>` URLs append after the session list.
Crash-loop detection quarantines the session file and skips restore entirely.

Pinned and unpinned URLs live in **two flat string arrays**, not one array of
objects. The runtime tab order is `pinned ++ tabs`, and `active` indexes into
that combined list (`apps/buffr-app/src/session.rs`). The struct is
`#[serde(deny_unknown_fields)]`, so a hand-written file with extra keys is
rejected. The schema is versioned so a future format bump can ignore stale
files.

```jsonc
{
  "version": 1,
  "pinned": ["https://example.com"],
  "tabs": ["https://kryptic.sh"],
  "active": 0,
}
```

`--no-restore` skips the read (homepage opens in a single tab) and still writes
a fresh session on exit. `--list-session` prints the saved file's entries to
stdout, one per line, as `<flag>\t<url>` where `<flag>` is `*` for pinned and a
single space otherwise, then exits without launching CEF. Schema version is
printed on stderr for diagnostic clarity.

### Fresh installs

On the very first launch, `session.json` does not exist. The runtime opens a
single tab loading `general.homepage` from the user's TOML config (default
`buffr://new`).

### `:q` semantics

`:q`, `:quit`, `d`, and `<C-w>` all close the **active tab**. Only when the last
tab is closed does the application exit. There is no separate "force-quit the
whole app" command yet — close the OS window.

## Pinned tabs

Pinned tabs are marked with a leading `*` in the tab strip and are toggled with
`<leader>p` (a space plus `p` with the default leader). Pinning does **not**
prevent close; it does reorder — `enforce_pinned_ordering` moves pinned tabs
ahead of unpinned ones in the strip while keeping the active tab selected.

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

## OSR sleep on occlusion

Shipped in v0.3.0. When the buffr window is hidden behind other windows or on an
inactive workspace, CEF's paint scheduler pauses and the wgpu present pipeline
short-circuits — eliminating the CPU/GPU spin on hidden workspaces.

**Trigger:** `WindowEvent::Occluded(true)` from winit calls
`BrowserHost::osr_sleep`, which in turn calls `was_hidden(true)` on the active
tab's CEF browser host. The wgpu frame loop skips `get_current_texture()` and
`present()` while sleep is active.

**Heuristic fallback:** Hyprland and some other compositors do not fire
`Occluded` on workspace switches. A `present_us` watchdog kicks in after:

- 1 frame taking > 500 ms, **or**
- 3 of the last 5 frames taking > 100 ms.

When the heuristic trips, the render thread applies the same `osr_sleep` path as
a real `Occluded` event. Sleep clears on `WindowEvent::Occluded(false)` or on
any user input that reaches the window.

**`Ctrl+C` during sleep** is handled: the `ctrlc` crate dispatches
`BuffrUserEvent::Shutdown` via `EventLoopProxy::send_event`, waking winit
immediately rather than waiting for compositor activity.

Note: `was_hidden` on the active tab preserves audio playback (CEF 147 behaviour
on Linux). Background tabs already called `was_hidden(true)` at switch time; OSR
sleep is additive on top of that.
