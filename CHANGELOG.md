# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.16] - 2026-08-30

### Fixed

- A new tab's omnibar now opens empty instead of pre-filled with `about:blank`:
  the pre-fill was applied even when the new-tab URL was a placeholder, so `O`
  showed `about:blank` in the address bar, and typing a URL on top of it and
  pressing Enter loaded the page while the omnibar stayed open with the pre-fill
  and the typed input merged in its buffer. Pre-fill now applies only to real
  URLs (`o` on a tab whose URL is not `buffr:`/`about:blank`).
- The previous tab list is now restored on reopen by default:
  `startup.restore_session` defaults to `true` instead of `false`, so the
  session saved to disk on every tab change is loaded back on the next launch
  without a config edit.

### Changed

- ~60 transitive dependencies rolled forward to their current in-range versions
  (rusqlite, wgpu, uuid, tree-sitter and others). The CEF 151 experiment was
  reverted before shipping: 151 broke page loading on Linux Wayland
  (GPU/renderer subprocesses never spawned), so the pinned CEF wrapper stays on
  the 148 line.

## [0.14.15] - 2026-08-30

### Fixed

- A popup whose window or renderer failed to build is now closed instead of
  orphaned: the live CEF popup browser stayed registered, kept playing audio and
  running JS, and leaked until shutdown.
- Find results are now tagged with the emitting browser: a background tab's
  in-flight find stream no longer overwrites the active tab's statusline counts.
- The console-IPC path no longer converts every console line to a Rust String
  before the sentinel check, and sentinel-prefixed lines are capped at a
  512-byte log budget — a page looping `console.log(bigString)` can no longer
  flood the log or force an allocation per line in the browser process.
- The font glyph cache is bounded at 4096 entries; a page rewriting
  `document.title` with fresh exotic characters can no longer grow it without
  limit.
- A 32-popup burst no longer mispairs frame/view/url: `on_before_popup` cancels
  the new popup when the pending-alloc queue is full instead of evicting an
  alloc a live popup is about to claim.
- One errored audio stream no longer clears the whole browser's stream count.
- The single-instance forwarder treats an `ERR`/EOF ack as failure instead of
  accepting any ack as success; the server acks `ERR` when every forwarded URL
  is rejected.
- `flatten_top_level` no longer deletes skipped entries it could not move.
- `--private --smoke-test` no longer leaks `$TMPDIR/buffr-private-<pid>-*`: the
  tempdir is dropped before the smoke-test `_exit`.

### Changed

- The mode→label table now has one source (`PageMode::name()`); the four copies
  that had drifted into UPPERCASE/lowercase variants are gone.
- The JS-string and HTML escapers, the ureq agent config, the bookmark-import
  statements and regexes, the atomic JSON write, the CLI store openers, the
  xtask binary build, and the deadline-clamp idiom each deduplicated into a
  single shared helper.
- `buffr_core` gains `http::agent` (bounded timeouts, buffr UA, no redirects),
  `html::escape` and `js::escape`; `buffr-modal` gains `PageMode::name()`.
- `buffr-cef` no longer depends directly on `ureq`.
- `fetch-cef` bounds network stalls with per-phase timeouts (15s connect, 30s
  response, 60s body-read) instead of the timeout-less default agent.
- The favicon BGRA→RGBA repack uses `as_chunks` in both crates (newer clippy).

### Performance

- Bookmark import reuses prepared statements instead of compiling each query per
  row (~500k `sqlite3_prepare` calls on a 100k-bookmark export).
- The context-menu panel size is cached at construction instead of re-measured
  every label per dirty frame.
- View-source caches grammar load failures so a broken artifact isn't re-
  dlopened per request; the import regexes compile once per process.

## [0.14.14] - 2026-08-14

### Fixed

- Fixed the browser chrome (tab strip pills and the statusline) not rendering at
  all: the chrome-paint closure gated on optional overlays (`Some(...)` for the
  pinned-close confirm, permissions prompt, omnibar and context menu), which are
  `None` unless such an overlay is open — so the whole paint was a no-op on
  every normal frame and the strip + statusline showed as the transparent
  swapchain clear color. The closure now requires only the always-present strip
  widgets and passes the overlays through optionally. A new pixel-based render
  e2e suite covers it.

## [0.14.13] - 2026-08-13

### Fixed

- Fixed the Linux CI e2e/smoke jobs failing before the suite ran: nothing staged
  CEF's `chrome-sandbox` SUID helper into `target/debug`, so the jobs depended
  on a stale copy lingering in the target cache (a fresh build after a version
  bump had none). `buffr-cef`'s build script now copies it alongside the rest of
  the CEF runtime. This release also ships the frozen-screen render fix from
  0.14.12, whose tag CI failed before any package was published.

## [0.14.12] - 2026-08-13

### Fixed

- Fixed a frozen-screen bug where the loaded page never replaced the loading
  animation: the recycled chrome paint buffer was not actually cleared between
  frames (the perf work that reused the ~8 MB buffer made `Vec::resize(len, 0)`
  a no-op when the worker returned the buffer at the same length), so the
  animation's opaque fill of the browser region stayed in the chrome texture and
  occluded the page forever. Every dirty frame now starts with a fully zeroed
  buffer; regression tests cover the recycling.

## [0.14.11] - 2026-08-13

### Fixed

- Fixed a startup crash on macOS and Windows (and a racy crash on Linux) where
  every binary aborted with "Request for unsupported CEF API version 14800": the
  `cef` crate's wrapper had moved to the 148 line while the vendored libcef
  runtime was still 147. `xtask fetch-cef` now pins the same 148.x major as the
  crate, so the wrapper and the shipped runtime agree.
- Hint mode no longer misroutes one tab's hint overlay to another when a tab
  switch lands between `f` and the renderer's Ready round-trip: hint events are
  tagged with the emitting browser and applied to that tab, matching the
  edit-mode attribution fix.
- Closing the active tab no longer falls through to an unconfirmed close of a
  pinned tab when a close-confirmation for a _different_ tab is already pending
  — the guard now arms-or-blocks like the middle-click and context-menu paths.
  The three sites share one `arm_pinned_close` helper.
- Session restore now lands on the tab the user closed on when the scheme gate
  drops `javascript:`/`data:` entries from a hand-edited `session.json`: the
  saved active index is re-based onto the filtered tab list instead of being
  applied to the wrong slot.
- Netscape bookmark import no longer misreads `<H3>` markup inside an anchor
  label as a folder: each `<A>…</A>` is consumed as a single token, so inner
  markup can't push a phantom folder tag onto every later bookmark or pop the
  real folders one level early.
- Netscape bookmark import is now linear in the file size: folder depth
  contributes at most 16 tags per anchor and a `TAGS=` attribute at most 64, so
  a pathological file can no longer hang import or bloat the store with
  quadratic INSERTs.
- Live popup windows are capped at 32, matching the CEF-side pre-creation cap —
  a page that evades the popup blocker (gesture-triggered chain, popunder) can
  no longer grow unbounded windows, GPU surfaces and fds. Over the cap the
  browser is closed without a window.
- Config hot-reload keeps working after a watcher callback panics: the callback
  no longer runs while holding the transport mutex (so a panic can't poison it
  and silently stop all later reloads), and each invocation is isolated with
  `catch_unwind` — reloads continue and the panic is logged.

### Security

- The private-network fetch guard now also classifies the RFC 2544 benchmarking
  range (`198.18.0.0/15`) and the deprecated IPv6 site-local range (`fec0::/10`)
  as non-public — a hostname or literal resolving to either could previously
  reach a local service through `buffr-src:` or Copy Image.
- "View Page Source" on an internal page no longer leaks the per-launch auth
  token into history: the `buffr-src:`-wrapped loopback URL now takes the same
  skip path as the raw internal URL, closing the gap left by the 0.14.10 fix.
- Updated `quick-xml` to 0.41.0 (via `wayland-scanner` and `plist`), closing
  RUSTSEC-2026-0194 and RUSTSEC-2026-0195 — quadratic-time start-tag parsing DoS
  in the 0.39 line. The matching entries were dropped from `deny.toml`'s ignore
  list.
- The update-check HTTP client no longer follows redirects (`max_redirects(0)`,
  matching the `buffr-src:` and image-copy fetchers) — a 3xx from the pinned
  release URL now fails the status check instead of silently landing on an
  arbitrary hop.

### Performance

- The omnibar resolves the typed input once per submit and per paste instead of
  twice — `classify_input` and `resolve_input` were each parsing the input. The
  new `buffr_config::search::resolve` returns the URL and its branch kind from a
  single resolution.
- View-source syntax highlighting caches the grammar registry and the compiled
  grammars per process and renders spans straight into the output buffer — the
  second request for a language pays only a stat-walk.
- The downloads tick narrows to rows whose values actually changed and skips
  no-change writes, instead of rebuilding and rewriting every row per tick.
- Tab-strip badge metrics are hoisted out of per-frame recompute and the
  context-menu width is cached.
- Hint/edit nonce lookups are gated behind each sentinel's own prefix, so the
  common path skips the full parse.

## [0.14.10] - 2026-08-06

### Fixed

- Internal pages are no longer recorded in history: the raw
  `http://127.0.0.1:<port>/<token>/…` URL used to be persisted, leaking the
  per-launch auth token to disk despite its "never written to disk" design.
  (Session files already stored the user-facing `buffr://` form.)
- Emoji and other supplementary-plane characters typed via direct-text paths
  (compose, hex-input) are now inserted as text instead of being silently
  dropped — a CHAR key event carries a single UTF-16 unit, so a surrogate pair
  used to resolve to nothing.
- A pinned tab can no longer be closed without confirmation while a different
  pinned-close prompt is already pending: the middle-click and context-menu
  paths used to fall through to an unconfirmed close whenever
  `confirm_close_pinned` was already armed.
- Edit-mode script teardown now also removes the `focus` capture listener it
  registers, so a soft-navigation re-injection no longer leaks one stale
  listener per navigation.
- "View Page Source" on a `buffr://` internal page now works: the app used to
  build the target from the display URL (`buffr://new`), which the `buffr-src:`
  gate rejects; it now uses the loopback URL the tab actually navigated to,
  which the same-host exception covers.
- Context-menu "Close Tab" now counts tabs across all engines before deciding to
  exit — closing the active engine's last tab while another engine still has
  tabs used to trigger a full shutdown.
- Context-menu tab actions now resolve the clicked tab by id instead of by the
  slot recorded when the menu opened — a background `window.open` or another tab
  closing could shift indices and make "Close Tab" fire against the wrong tab.
- Restored sessions and CLI URLs can no longer carry `javascript:` or `data:`
  schemes — every other navigation entry point already rejects them, so a
  hand-edited `session.json` couldn't drive script execution at startup.
- Store connections now set an explicit 10 s `busy_timeout` (up from the bundled
  SQLite's incidental 5 s default), so a second buffr process sharing the
  profile waits for a transient lock instead of surfacing `SQLITE_BUSY`
  mid-write.
- Typing an out-of-range port (e.g. `localhost:99999`) no longer produces an
  unparseable `https://localhost:99999` that silently no-ops — it resolves as a
  search query instead, matching the resolver's "always a fully-qualified URL"
  contract.
- `[privacy] skip_schemes = []` now means "record every scheme" as the config
  docs promise; an empty list used to silently fall back to the five default
  skip schemes, making "record everything" impossible.
- View-page-source now honors its same-host exception on the fetch worker: the
  worker received the initiator host as a bare string (no scheme), which the
  host parser rejected, so every private-network source — including `buffr://`
  internal pages — was refused. The bare-host shape is now accepted.
- `buffr-src:` and Copy Image no longer follow HTTP redirects in the browser
  process. A redirect hop was fetched without re-running the private-network
  gate, so a public URL could 302 into a loopback or RFC1918 address; a 3xx now
  surfaces as an error page instead.
- A closed tab stashed for undo no longer keeps playing: CEF's `was_hidden` does
  not cut audio, so the tab's media — and the statusline's audio indicator —
  survived until the tab aged out of the undo stack. Closing now mutes the
  browser and injects a pause-all-media script, which stops the CEF audio
  stream; reopening a stashed tab restores its sound.
- Pressing Ctrl+C during the supervisor's restart cooldown now stops the
  restart: the shutdown check ran only around the child spawn, so a signal that
  landed while the loop slept still spawned a fresh child. The signal handler
  also stays armed for the supervisor's lifetime, so a second Ctrl+C terminates
  the supervisor instead of killing it and orphaning the new child.
- Edit-mode events now carry the browser that produced them, and the drain drops
  events from any browser other than the active tab's — a background tab's or
  popup's page-driven focus can no longer flip the active tab into Insert
  (capturing keystrokes into the wrong page) or yank its selection to the
  clipboard.
- `buffr-src:` and Copy Image now refuse hosts whose DNS resolves to a loopback
  or private address (e.g. `127.0.0.1.nip.io`): the guards classified hostname
  strings only, so a name statically resolving to loopback passed as public and
  let a page pivot the browser-process fetch into the local network. The guards
  now resolve the host and classify every address, failing closed on resolution
  errors.
- Profile directories are now owner-only: `cache` and `data` are chmod 0700 on
  every launch and `session.json` is written 0600, where a default umask used to
  leave them 0755/0644 — readable by any local user (history, bookmarks,
  permissions, session and cache).
- Closing a tab left of the active one no longer leaves the old active browser
  running foregrounded — timers, animation and audio — until the next tab
  switch: the stored active index is now fixed up before the new active tab is
  selected.
- The hint-mode statusline no longer renders a meaningless `(n/n)` counter —
  numerator and denominator were the same field, so it always read e.g.
  `f: as (3/3)`. It now shows just the typed prefix (`f: as`).
- The vim register prefix (`"<char>`) no longer swallows two keystrokes and
  discards them. Register state was captured but never threaded into actions, so
  `"ay` produced a plain `YankUrl`; `"` now falls through to the keymap like any
  other unbound key.
- Removed the never-produced `PageMode::Pending` state and its dead status-line
  and keymap arms; a `[keymap.pending]` config section is no longer accepted.
- `<C-c>` is now reliably `StopLoading`. It was bound twice — the later
  `YankUrl` row shadowed it, so stopping a load had no default key and the
  keyboard-accessibility audit falsely reported it covered. The shadow row is
  gone (`y` still yanks) and the audit now walks the built keymap instead of the
  static table, so a shadowed binding can no longer fake coverage.
- `classify_input` now delegates to `resolve_input` instead of hand-mirroring
  its branch order, so the omnibar's URL-vs-search telemetry can never drift
  from what the resolver actually does.
- `:open` now resolves its argument through the same `resolve_input` the omnibar
  uses, so `javascript:` and `data:` URLs typed after `:open` are treated as
  search queries instead of executing in the current page's origin.
- Occluding the window now arms the 200 ms occlude→sleep debounce instead of
  putting the paint pipeline to sleep immediately, so a workspace switch or
  overlay that flickers occluded-then-revealed no longer emits a spurious
  sleep/wake cycle. Reveal still wakes immediately.
- `startup.restore_session` and `startup.new_tab_url` are now read:
  `restore_session = true` reopens the previous session (opt-in, default
  `false`), and a fresh tab (`o`/`O`/`:tabnew`) opens `new_tab_url` instead of
  the homepage. The never-implemented `theme.mode` and `updates.channel` knobs
  were removed along with their validation and docs.
- Removed `buffr-engine`'s unused neutral event/state types (`EngineEvent`,
  `NavigationEvent`, `LoadState`, `CursorChanged`, `CursorKind`) — they had zero
  callers and their tests only tested themselves.
- Removed the dead native-compositing trait trio (`supports_native`,
  `set_native_parent`, `set_native_visible`) plus `set_internal_server` from
  `BrowserEngine` — none had callers, and the excluded `buffr-webkit` backend
  dropped its matching overrides and the `SetNativeRect` worker command.
  `is_using_native_compositing` stays; it gates the live pixel-pipeline path.
- Collapsed `buffr-history`'s eight `open*` constructors into a `HistoryBuilder`
  (with `open`/`open_in_memory` kept as thin delegators); a third option would
  otherwise have meant sixteen constructors.
- The Chromium renderer sandbox is now enabled: `no_sandbox` and the redundant
  `--no-sandbox` switch are gone, so renderer subprocesses run under the
  namespace sandbox on Linux instead of unsandboxed as the user. On hosts with
  unprivileged user namespaces disabled, CEF warns and continues without
  sandboxing (documented in `docs/packaging.md`).
- `buffr-clipboard:read` (webkit backend) now serves only buffr's own `buffr://`
  internal pages: the scheme handler checks the requesting page's origin and the
  requested scheme, so a cross-origin or iframe page's fetch gets an empty body
  instead of the system clipboard.
- External schemes in the webkit backend now launch `xdg-open` only on a
  user-initiated navigation (a scripted `location = 'foo://…'` no longer pops a
  handler), and the spawned child is reaped instead of left as a zombie.
- `[privacy] clear_on_exit` now actually wipes `cache` and `local_storage`: the
  deletes resolved against `paths.cache`, but CEF's `root_cache_path` is
  `paths.data`, so they hit a directory CEF never populated and logged
  `dir absent — skipping`.
- Reopening a closed pinned tab no longer breaks the pinned-first tab ordering,
  so the tab strip's click hit-testing can no longer select the wrong tab.
- Switching tabs no longer re-presents a stale frame of the previous visit: the
  OSR freshness watermark is seeded from the new tab's current frame generation
  instead of reset to 0, so the loading animation shows until the new tab
  actually paints.
- Popup windows now render at the device scale: the popup's `OsrViewState` is
  seeded from the main view's scale at creation and kept in step by
  `set_device_scale`, so a popup on a HiDPI display is no longer laid out at 1×
  with doubled click offsets.
- `buffr-src:` no longer fetches loopback / RFC1918 / cloud-metadata through
  non-canonical host spellings (`2852039166`, `0177.0.0.1`, `::ffff:7f00:1`, …):
  the private-network guard parses hosts strictly and fails closed on
  numeric-shaped forms that glibc resolves to local addresses.
- Copy Image (`<img>` context menu) now refuses loopback / private / link-local
  hosts (a page can't fetch those itself, so buffr would be the proxy) and caps
  response and `data:` payload sizes at 16 MiB.
- Multi-word history search now ANDs its tokens instead of matching them as one
  adjacent phrase — `"rust learn"` finds a row whose url and title contain both
  words in any order or column, as documented.
- A page can no longer grow the edit-mode event queue without bound: the sink
  drops the oldest entry at a 1024 cap, and oversized payloads are rejected at
  parse time.
- A first OSR frame skipped because the render worker was busy is now
  re-uploaded on the retry instead of leaving the previous tab's frame on
  screen; a background tab's popup can no longer steal the tab's OSR paint
  routing.
- Popup windows opened from a background tab no longer report the previous,
  aborted popup's URL in their address bar.
- `view-source:` uses only already-installed syntax-grammar artifacts — a cold
  cache no longer triggers a network clone + native compile from the render
  path.
- The Netscape bookmark importer no longer truncates URLs or mangles titles when
  an attribute value contains a literal `>`.
- The input bar's cursor and scroll position now follow real glyph advances, so
  CJK text no longer leaves the cursor inside the glyph.
- An empty keymap binding (`"" = "action"`) is now rejected instead of firing
  its action for every abandoned key prefix.
- `enter_mode("insert")` restores the mode you came from on Esc, like the
  built-in Insert entry; swipe gestures no longer bleed between the main window
  and popups; and the internal server survives transient `accept()` failures (fd
  exhaustion) instead of taking `buffr://` pages down for the session.

### Performance

- The omnibar's bookmark search is now capped at 8 rows inside SQL instead of
  scanning the whole bookmarks table per keystroke (`search_limited`).
- The per-tick (144 Hz) refresh no longer computes the tab list twice:
  `refresh_tab_strip` returns its `tabs_changed` diff plus the summaries, and
  the favicon pump consumes the same data instead of re-querying the engine —
  one `tabs_summary()` + one HashSet per tick instead of two of each, and the
  `prev_tabs` clone-diff is gone.
- Synthetic (between-paints) OSR frames no longer memcpy the full pixel buffer
  on the UI thread: when the generation is unchanged the renderer uploads an
  empty buffer and the GPU worker's generation dedupe skips the texture write.
- The chrome texture is now uploaded as two thin strip bands (top strips +
  statusline) instead of the whole ~8.3 MB texture every dirty frame; when the
  loading animation or a floating overlay paints into the browser region the
  full buffer is uploaded as before.
- `Renderer::frame` now acquires the swapchain texture before painting chrome or
  cloning OSR pixels, so a skipped frame (timeout, occluded, validation, stale
  size) no longer wastes the CPU paint and the 8.3 MB alloc.
- The chrome paint buffer is recycled instead of reallocated every dirty frame:
  the render worker hands the consumed `Vec` back through the stats channel and
  the UI thread reuses it, so the ~8 MB allocation is no longer freed on the
  worker thread.
- Glyph rendering now caches each rasterized glyph (metrics + bitmap + advance)
  behind an `Arc`, so a cache hit is a refcount bump instead of a bitmap copy,
  measuring and drawing share one lookup, and `draw_text` advances the pen from
  the same entry it drew from.
- The two per-keystroke SQLite queries — history's FTS5 search and the bookmark
  search — now use `prepare_cached`, so SQLite re-parses and re-plans them once
  instead of on every omnibar keystroke.
- Removed per-event allocations on the UI thread: the 250 ms URL poll reuses the
  tick's tab-id list instead of building `TabSummary` structs, the splash JS
  push skips its 3-lock URL read between period boundaries, the context menu
  hit-tests cached entries instead of re-cloning every label per mouse move, and
  the loading animation draws chars directly instead of allocating a String per
  cell.
- The event loop's pump period is computed from `outputs()` once per second
  instead of every tick, the chrome-paint closure clones the statusline and tab
  strip only when chrome is actually repainted, and `hint_status()` is no longer
  polled (two locks per tick) when hint mode is inactive.

## [0.14.9] - 2026-08-03

Five correctness fixes from the 2026-08-02 review pass, two of them
high-severity. Every one is reproducible on 0.14.8.

### Fixed

- Clicks in a popup window (OAuth flows, `window.open`) now land on the element
  under the cursor. The popup told CEF a viewport as tall as the whole window
  while the page image was painted into a rect one address-bar strip shorter, so
  the page was squashed vertically and the error grew toward the bottom edge —
  an "Authorize" button at the foot of the window received the click meant for a
  different element. Both `popup_resize` call sites and the paint rect now go
  through one helper, `popup_cef_rect_pure`, so the reported viewport and the
  painted quad cannot drift apart again.
- Answering a permission prompt can no longer grant camera, microphone,
  geolocation or any other capability to a site the prompt was not asking about.
  The prompt strip rendered the front of the permissions queue but remembered
  nothing about which request it was; the answer was applied to whatever sat at
  the front at keypress time. When the displayed request was withdrawn behind
  the user's back (the engine cancels a prompt when its tab navigates away) the
  next queued request silently took its place on screen — so allow-and-remember
  (`A` / `Y`) could write a **persistent** allow row for a different origin and
  a different capability than the one shown. The prompt now carries the identity
  of the request it is displaying, an answer is applied only to that request,
  and a withdrawn request's prompt is taken off screen and replaced instead of
  lingering. Answers that arrive after a withdrawal are discarded with a warning
  and leave the queue untouched, so the request the user never saw stays
  unanswered.
- Scrolling vertically with a touchpad no longer navigates back through history.
  Both swipe call sites passed the scroll delta as the horizontal component, so
  a downward scroll accumulated as a left-to-right swipe and fired `HistoryBack`
  after about 150 pixels.
- A transient multi-second GPU stall (`queue.write_texture` / `queue.submit`) no
  longer causes the supervisor to kill and respawn a healthy browser. The UI
  thread keeps marking liveness through a stall so the heartbeat thread's
  existing recovery path can fire; the heartbeat is now dropped only on a
  terminal socket write error.
- `<Esc>` always leaves Insert mode, even when no text field is focused. The
  engine can sit in Insert with nothing focused (a user-bound
  `enter_insert_mode`, or a field that went away), and in that state it answers
  every key with "edit mode active" before consulting the keymap — so no
  binding, `<Esc>` included, could fire and the keyboard was dead until the user
  clicked an input or closed the window. `<Esc>` now returns to the mode Insert
  was entered from; every other key behaves as before.
- A `[keymap.insert]` section is now a startup error naming the section instead
  of being silently installed into the **normal** keymap, where the binding
  fired while browsing and never in Insert mode. Insert mode forwards every key
  to the page by design, so it has no bindings; move the entry to
  `[keymap.normal]` and use `<Esc>` to leave Insert.

[0.14.9]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.9

## [0.14.8] - 2026-08-02

A release-pipeline fix. v0.14.7 published everywhere except the AUR, so
`buffr-bin` never left 0.14.6; this is the release that lets it catch up. No
changes to the browser itself.

### Fixed

- **`buffr-bin` publishes to the AUR again.** The publish jobs passed ssh
  options through a workflow `env:` block whose value held a literal `~`. That
  is never expanded — git runs the command via `sh -c`, tilde expansion does not
  apply to the result of a parameter expansion, and ssh does not expand `~` in a
  `-o` value either — so `UserKnownHostsFile` named a directory literally called
  `~`. It went unnoticed for as long as the jobs used
  `StrictHostKeyChecking=accept-new`, which never reads the file; v0.14.7
  switched to `yes`, and the AUR push failed with
  `No ED25519 host key is known for aur.archlinux.org`. `GIT_SSH_COMMAND` is now
  assigned inside the step so `$HOME` expands before git sees it.
- **Host-key pinning in the publish jobs is now actually in effect.** Because
  the path never resolved, `brew-tap` and `scoop-bucket` were verifying against
  the runner image's global `/etc/ssh/ssh_known_hosts` rather than the pin the
  workflow shipped. The pinned keys are re-verified against
  `https://api.github.com/meta` and `ssh-keyscan aur.archlinux.org`.

### Added

- The user guide is published from this repository at
  <https://buffr.kryptic.sh>, with the previous `/docs/` URLs redirecting to the
  new root.

[0.14.8]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.8

## [0.14.7] - 2026-08-01

A full-codebase review (see `docs/code-review.md`) and the fixes for everything
it turned up that needed no product decision. What is still open is listed in
`docs/backlog.md`.

### Security

- **The renderer → browser console-log IPC is now authenticated.**
  `on_console_message` matched three fixed, publicly-documented sentinel
  prefixes anywhere in any console line from any frame — including third-party
  iframes — so any page could overwrite the live `HintSession` and redirect the
  next hint keystroke to an element it chose, pin the platform idle inhibitor on
  so the screen never locked, or push text into the yank-to-clipboard path.
  Lines now carry a 128-bit nonce (`<sentinel><nonce>:<json>`) minted from the
  OS CSPRNG, rotated per main-frame load and per hint session, and the match is
  anchored at the start of the line. See `docs/hint-mode.md` for what this does
  and does not close.
- **`buffr-src:` no longer performs arbitrary fetches for web content.** It was
  registered `CORS_ENABLED | FETCH_ENABLED`, so any page could
  `fetch('buffr-src:http://127.0.0.1:8080/admin')` and have the browser process
  retrieve it outside Chromium's network stack — bypassing same-origin policy,
  CSP and private-network checks — then render the body. Those flags are gone,
  non-`http(s)` schemes are rejected, and loopback / link-local / RFC1918
  targets are refused unless the initiating frame is already on that host.
- **Windows "open on finish" no longer routes through `cmd.exe`.** Rust quotes
  arguments per `CommandLineToArgvW`, which `cmd.exe` does not follow — it
  re-parses `&`, `|`, `^`, `<`, `>` after unquoting — so a download named
  `report"&calc.exe&".pdf` executed a second command. Uses `explorer.exe`.
- **The single-instance socket and supervisor clean-shutdown flag moved to a
  `0700` per-uid directory**, verified for owner and mode after creation. They
  previously fell back to predictable paths in a shared temp dir, where another
  local user could bind the socket and silently swallow forwarded URLs (the
  victim exits 0 without opening), or pre-create the clean flag — checked with a
  symlink-following `exists()` — to disable the crash watchdog entirely.
  Forwarded payloads are now accepted only after an `SO_PEERCRED` uid check.
- **The internal server no longer reads unbounded headers.** A single
  newline-less header drove the browser to OOM; the 16 KiB guard only ran once a
  whole line had been buffered. Concurrent connections are capped (503 over the
  cap) — previously an unbounded thread per connection, each pinned for the full
  2 s read timeout. Responses gained `Referrer-Policy: no-referrer` (the auth
  token is in the URL path) and non-loopback `Host` headers are rejected.
- **The vendored CEF distribution is now verified.** The ~200 MB archive that
  becomes `libcef.so` in every shipped package was downloaded with no integrity
  check at all — the `sha1` from the remote index was parsed and then discarded.
  It is now checked, index entries whose name would escape `vendor/cef/` are
  rejected, and tar entries with a parent-dir component are refused on extract.
- Reopening a closed `buffr://` tab no longer exposes the internal server's auth
  token in the address bar and the session file.
- Page console messages are no longer logged verbatim by default; the blanket
  log is behind an opt-in env var and non-sentinel text is truncated. It
  previously wrote page-controlled text to disk even in private mode.
- CI hardening: `cargo-machete` pinned to a commit SHA instead of `@main`, a
  workflow-level `permissions: contents: read`, and pinned `known_hosts` for the
  publish steps (`accept-new` against an empty `known_hosts` on an ephemeral
  runner is equivalent to disabling host-key checking).
- New fuzz targets for the console-sentinel parsers and the Netscape bookmark
  importer — the most attacker-reachable parsers in the tree, previously
  unfuzzed while the three existing targets all took input the user typed into
  their own config.

### Fixed

- **The update checker always reported an update was available.** `buffr-core`
  pinned its own `0.7.0` instead of the workspace version, and `UpdateChecker`
  seeds the current version from `CARGO_PKG_VERSION`, so `0.14.6 <= 0.7.0` was
  false forever. The same string was also the `buffr_version` on every crash
  report and the update / image-copy `User-Agent`.
- **Three reachable panics.** `truncate_to_width` in the permissions prompt
  sliced before checking the char boundary, so a non-ASCII permission origin
  panicked the chrome; `parse_hex_rgb` guarded on byte length then sliced at
  fixed byte offsets, so a 7-byte `[theme]` value containing a multi-byte
  codepoint hard-panicked startup; and view-source highlighted the raw bytes
  while slicing the lossy-decoded string, so any non-UTF-8 source panicked
  mid-replacement-character.
- **A hung UI never restarted.** The heartbeat thread dropped its socket, the
  supervisor read that as a clean disconnect rather than a hang, and then
  blocked in `child.wait()` forever on a frozen browser.
- **Windows: a UI hang spawned a second browser.** `GetExitCodeProcess` was
  called without first checking the process had exited, so `STILL_ACTIVE` (259)
  was recorded as a crash exit. Windows also restarted on any non-zero exit
  (`buffr --bogus-flag` looped three times) and had no clean-flag support, so a
  segfault during teardown after a normal window close respawned the browser.
  Child arguments are now quoted per the MSVCRT rule instead of appended raw,
  and an explicit environment block replaces mutating the supervisor's own env
  while worker threads are live.
- **Chrome updates were silently dropped.** `Renderer::frame` returned `Ok` on
  six paths that never uploaded a pixel, and the caller retired the dirty
  generation anyway — so a keystroke typed while the wgpu worker was still
  presenting was lost until an unrelated event marked chrome dirty. Skipped
  frames also re-fed the previous frame's stats into the occlusion heuristic.
- **HiDPI hit-testing missed.** The context menu and the pinned-close confirm
  buttons were painted in logical space but hit-tested against physical
  coordinates, so at any scale > 1 clicks on the visible menu fell through to
  "clicked outside" and "Yes" did nothing.
- **A wedged renderer on a permission prompt.** `resolve` persisted the decision
  before firing the C++ callback, so a sqlite error returned via `?` and dropped
  the `MediaAccessCallback` un-invoked.
- A lock-order inversion between `tabs` and `active` in `BrowserHost` that could
  deadlock the UI once a second thread called any engine method.
- Two `window.open()` calls in one task overwrote a single-slot pending-popup
  allocation, so one popup got the other's frame and the second leaked a live
  CEF browser past `close_all_browsers`.
- Bookmark omnibar search issued a full table scan plus one query per row on
  every keystroke (~2001 SQL round-trips at 2000 bookmarks, for 8 displayed
  results). The match, rank and limit are now done in SQL.
- `import_netscape` stored HTML-escaped URLs verbatim, so a real Chrome/Firefox
  export of `?a=1&b=2` was saved as `?a=1&amp;b=2` and navigated elsewhere. It
  also committed one transaction per entry with no rollback on partial failure.
- Non-base64 binary `data:` images were unrecoverable — `percent_decode` pushed
  each byte as a `char` into a `String` that was then UTF-8 encoded, so every
  byte ≥ 0x80 became two.
- Full-width CJK text drew each glyph on top of the previous one, and every
  truncation and centring computation under-measured it by ~2×.
- IME composition passed UTF-8 byte offsets where CEF expects UTF-16 code units.
- Ctrl+A and friends reached the page as Alt+A on the WebKit backend.
- An abandoned multi-chord prefix never flushed, so `g`, wait, `j` was rejected
  instead of scrolling.
- Unmodelled keys (`Menu`, media keys, F13+) were injected into form fields as
  `SpecialKey::Insert`, toggling insert/replace mode.
- `InternalServer::drop` could hang the process forever; the accept loop now
  polls a shutdown flag as its comments already claimed.
- The idle inhibitor issued blocking sends from the winit event loop, so a
  wedged worker parked the browser UI thread and could hang shutdown.
- Crash reports were named with millisecond precision and written with
  `fs::write`, so two panics in the same millisecond lost one.
- The config watcher reloaded on any churn in the config directory, surfacing a
  spurious IO error when the file was momentarily absent.
- `--audit-keymap` printed `<leader>` chords against a hard-coded backslash
  while the actual default leader is a space.
- Several integer overflows guarded only after the fact: the favicon cache's
  `pixel_count * 4`, the tab-strip favicon bounds check, and the omnibar popup
  width, which underflowed for windows narrower than its clamp floor.
- `buffr-webkit` (excluded from the workspace, so nothing caught any of this):
  the internal server was never passed through, so the first tab could not load;
  download destinations were built from an unsanitized server-supplied filename,
  which `Path::join` lets escape the download directory entirely; `--private`
  still wrote a persistent cookie database; the OSR ingest loop read past the
  end of its buffer; and the downloads sink downcast could never match, so every
  download was silently invisible to the store.

### Changed

- Truncated chrome text now actually renders the `..` ellipsis its layout had
  always reserved space for.
- `validate()` rejects unparseable `[theme]` colours and a zero
  `crash_reporter.purge_after_days`. Both were silently accepted, so
  `--check-config` passed while the colour reverted to the built-in default and
  `--purge-crashes` deleted every report including the one just written.
- A store whose schema version is ahead of the running binary is now refused
  rather than silently ignored.
- Crash reports are named `<stamp>_<seq>.json` and written with `create_new`.
- The Linux smoke test fails (exit 4) when the wgpu renderer cannot initialise,
  instead of exiting the event loop and reporting success without painting.
- Member manifests inherit workspace dependency versions; the literals had
  already diverged (`buffr-webkit` pinned an incompatible `hjkl-clipboard`).

### Added

- `crates/buffr-store` — the sqlite open/tune boilerplate, migration runner and
  timestamp helpers that were duplicated verbatim across five stores.
- `docs/code-review.md` and `docs/backlog.md`.

### Removed

- The legacy CEF permissions queue, dead since Phase 8a: nothing pushed into it,
  yet it was threaded through `BrowserHost`, `BuffrClient`, `CefEngineSinks` and
  an `AppState` field, and drained at shutdown.
- Assorted dead constants, unused handler factories, the windowing parity shims
  for a backend that no longer exists, and three unused workspace dependencies.

[0.14.7]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.7

## [0.14.6] - 2026-05-25

### Fixed

- `0` / `-` / `=` page-mode bindings now actually zoom the page. The v0.14.5
  chord-build fix made the keys reach `dispatch_action` with `Resolved(ZoomIn)`
  / `Resolved(ZoomOut)` / `Resolved(ZoomReset)`, but `BrowserHost`'s
  `impl BrowserEngine` never overrode `zoom_in` / `zoom_out` / `zoom_reset` — so
  the calls hit the trait's default no-op `{}` stubs. `BrowserHost` had
  `adjust_zoom(±0.25)` and `reset_zoom()` helpers all along (used by an older
  `dispatch(action)` path), they just weren't wired to the trait. Three-line
  override on the BrowserEngine impl.

[0.14.6]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.6

## [0.14.5] - 2026-05-25

### Fixed

- Page-mode bindings `0` (ZoomReset), `-` (ZoomOut), `=` (ZoomIn) and other
  printable non-letter keys stopped working after the v0.14.2 wayr→winit revert.
  Root cause: winit's `KeyEvent.text` is sometimes `None` for digit /
  punctuation keys on Wayland xkb configs, and the bridge stuffed the
  logical-key character into `KeyCode::Named("0")` — the chord builder's
  text-first path returned None and the named-key path didn't recognise
  single-printable names. Fall back to `logical_key.Character` when `text` is
  absent so a single printable codepoint always reaches the chord builder.
  Affects the entire vim-style chord set, not just zoom.

[0.14.5]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.5

## [0.14.4] - 2026-05-25

### Fixed

- Ctrl (and other modifier keys) stayed "stuck" after release: subsequent key +
  pointer events behaved as if the modifier were still held. winit may dispatch
  `ModifiersChanged` _after_ the corresponding key-release on some backends, so
  the v0.14.0 strategy of syncing `self.modifiers` only from
  `KeyEvent.modifiers` missed the actual release transition (the release key
  event carried the pre-release cached state). The bridge windowing layer now
  surfaces a `WindowEvent::ModifiersChanged(Modifiers)` variant and `AppState`
  mirrors it into `self.modifiers` independent of key-event ordering. Main
  window + popup paths both updated.

[0.14.4]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.4

## [0.14.3] - 2026-05-25

### Fixed

- Cursor shape no longer updated on hover (link → hand, text → I-beam, etc.).
  The winit-backed `EventLoop::set_cursor` shipped as a no-op in v0.14.0; it was
  masked on Linux until v0.14.2 (where the wayr revert dropped the wayr
  re-export that had a real implementation). `pump_cursor_changes` now routes
  through the per-window `Window::set_cursor` on the main toplevel and every
  popup — winit silently ignores the request for non-focused windows, so
  whichever surface holds pointer focus picks up the cursor. The no-op
  `EventLoop::set_cursor` stub + its `pending_cursor` / `focused_window`
  scaffolding are deleted.

[0.14.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.3

## [0.14.2] - 2026-05-25

### Changed

- Linux backend reverted from wayr to winit. wayr was hanging under CEF on
  Linux; the pre-migration winit-on-Linux path was stable, so buffr-app now uses
  winit on all three platforms. wayr is parked — `apps/buffr-poc` keeps it for
  the future WPE WebKit embedding work, and `buffr-modal` retains its
  `wayr_adapter` module dormant.

### Removed

- `wayr` dependency from `buffr-app`. `buffr-modal` now built with only the
  `bridge` feature on every platform.
- The `BUFFR_WEBKIT_NATIVE` Wayland-native-handle extraction block (#151). Will
  return when WPE work and wayr both come back.
- Linux idle inhibitor wiring via `wl_display` / `wl_surface` pointers. winit
  does not expose raw Wayland handles independent of a window borrow in a shape
  compatible with the existing `unsafe new_inhibitor(...)` call, so both
  pointers now pass null on every platform. The inhibitor backend warns +
  swallows the null case, matching its prior behaviour on non-Wayland systems.

[0.14.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.2

## [0.14.1] - 2026-05-25

### Fixed

- Mouse wheel vertical scroll direction. The buggy commit `6967500` worked
  around inverted scroll by negating dy in the wayr → CEF helper; the root cause
  was wayr passing Wayland's "positive = scroll down" sign through unchanged.
  Wayr 0.2.1 now normalises to winit's "positive = scroll up" convention at the
  emission site, so the buffr-side negation is reverted and the helper just
  passes the signed delta straight to CEF.

### Changed

- Bumped wayr dep `0.2.0 → 0.2.1` for the scroll-sign fix above.

[0.14.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.1

## [0.14.0] - 2026-05-25

### Added

- Cross-platform support: buffr-app now builds and runs on macOS and Windows in
  addition to Linux. Wayr stays Linux-only (Wayland-only by design); macOS and
  Windows use winit 0.30 behind a `crate::windowing::*` re-export that mirrors
  wayr's API shape. A new `bridge` feature on `buffr-modal` introduces a
  toolkit-agnostic `KeyEvent` / `KeyCode` / `Modifiers` / `ScanCode` shape
  consumed on non-Linux; Linux keeps using `wayr_key_event_to_chord`.
- Hidden `--smoke-test [--smoke-test-timeout-ms <N>]` flag on `buffr-app`.
  Launches the windowing backend, paints once, exits 0 on first paint, exits 3
  if a watchdog timer fires. Wired into CI as a 3-OS event-loop smoke matrix.
- `BUFFR_DISABLE_ZYGOTE=1` env switch appends `--no-zygote` to the CEF command
  line on Linux. Off by default; intended for headless / containerized
  environments where Chromium's zygote subprocess fork can't pass the ICU file
  descriptor.
- Authoritative compositor occlusion via wayr 0.1.9 `Occluded` events: paint
  pacing now stops when the compositor reports the surface fully occluded
  instead of inferring from `present_us` heuristics.
- Damage tracking via wayr 0.1.11: chrome paints attach damage rectangles so the
  compositor can skip recomposite for unchanged regions.
- Focus existing window on `--new-tab` via `xdg_activation`: a second buffr
  invocation now raises the existing window before opening the tab instead of
  spawning a duplicate toplevel.
- Per-platform smoke jobs (Linux, macOS, Windows) cover the full event-loop
  startup path, including CEF init and first paint. Ubuntu smoke is marked
  `continue-on-error` while CEF's subprocess ICU fd-passing remains broken under
  pixman-headless sway on GHA runners (unrelated to the migration — affects real
  Linux desktops only in headless contexts).

### Changed

- Migrated buffr-app from winit to wayr on Linux. Wayr owns wl_display dispatch
  so wayr globals can be bound against the same display without the EAGAIN race
  winit's calloop integration causes.
- Workspace resolver bumped to `"3"` (matches edition 2024).
- Renderer decoupled from `winit::Window`: takes a `Surface` trait so wayr
  toplevels and winit windows go through the same path.
- All wgpu mutating calls run on a worker thread (`render_worker`); UI thread
  only does CPU prep + memcpy + `try_send`. Eliminates Wayland backpressure
  hangs when the compositor queues frames.
- CI smoke timeout bumped 30s → 120s for cold-runner CEF init; smoke success
  signal moved from `RedrawRequested` to first `paint_chrome` completion so
  headless Windows runners (no DWM → no WM_PAINT) still satisfy it.

### Fixed

- buffr-cef: collapse nested `if let` blocks for macOS clippy.
- buffr-app: keep `modifiers` in sync from `KeyEvent.modifiers` on every key
  event, not only on a dedicated `ModifiersChanged` event (wayr's model).
- buffr-app: ASCII-control text now routed through the named-key path so Ctrl-A
  / Ctrl-B etc. produce the right chord instead of garbled bytes.
- buffr-app: leak the renderer on shutdown instead of dropping (wayr 0.1.4
  wgpu-surface path is safe; the leak avoids a tear-down race that fired Vulkan
  validation warnings on exit).
- supervisor: respect an explicit clean-shutdown intent via env-var flag, and
  don't respawn when the child exits with a non-zero code if it was a clean exit
  (e.g., `--smoke-test`).
- Build a child process suspended on Windows via `&Path` not `&PathBuf` (clippy
  `ptr_arg`).
- Windows headless smoke: `request_redraw()` after window creation forces a
  paint cycle when the runner's WM never delivers WM_PAINT.

### Removed

- `[patch.crates-io]` overrides for wayr — published wayr versions now drive
  buffr's wayr surface directly.

[0.14.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.14.0

## [0.13.11] - 2026-05-19

### Fixed

- Windows packaging job failed in v0.13.10 because the GitHub Actions PowerShell
  wrapper propagates `$LASTEXITCODE` from the last external command — even when
  the script checks it manually and chooses to continue. `buffr-helper.exe`
  exits -1 by design (no `--type=` → cef returns -1), and that bled out as the
  step's overall exit code. Reset `$LASTEXITCODE = 0` after the manual check.
  macOS and Linux smoke (bash) are unaffected because the trailing `echo` resets
  `$?` to 0.

[0.13.11]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.11

## [0.13.10] - 2026-05-19

### Added

- CI smoke now also exercises `buffr-helper` on all three platforms (Linux
  tarball, macOS DMG, Windows zip). Helper has no clap CLI; bare invocation
  loads `libcef` then calls `execute_subprocess`, which returns `-1` for the
  no-`--type=` path (Rust exit 255 on unix, -1 on Windows). Catches framework
  rpath rot on macOS (`Buffr Helper.app/Contents/MacOS/Buffr Helper`), libcef
  RPATH on Linux, and DLL search-path regressions on Windows.

[0.13.10]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.10

## [0.13.9] - 2026-05-19

### Added

- Per-platform smoke tests in CI: every release job now extracts the produced
  artifact (Linux tarball, macOS DMG, Windows zip) and invokes both `buffr` and
  `buffr-app` with `--version` and `--help` before uploading. Clap exits before
  CEF init so no display server is required; catches PE/ELF/Mach-O link
  failures, missing `buffr-app.exe` suffix on Windows, libcef rpath rot on
  Linux, and dyld framework misses on macOS.

[0.13.9]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.9

## [0.13.8] - 2026-05-19

### Fixed

- Supervisor watchdog killed the browser before it could ever connect on
  cold-disk first runs (scoop / MSI install on a fresh Windows machine). Two
  stacking issues:
  - buffr-app called `heartbeat::Heartbeat::try_connect` ~600 lines into `main`,
    after `cef::load_library`, `Cli::parse`, tracing init, and SQLite db opens.
    Hoisted to immediately after the CEF subprocess short-circuit so the
    named-pipe / UDS connect happens before any heavy work.
  - Supervisor `CONNECT_GRACE` was 5 s on both Unix and Windows. Raised to 20 s
    — fits the worst observed cold-cache first run. The 20 s default is
    overridable via `BUFFR_CONNECT_GRACE_MS` so integration tests stay bounded
    (the `heartbeat_no_connect` integration test would otherwise wedge nextest's
    60 s per-test timeout).
- Supervisor watchdog error message pointed users at a non-existent
  `~/.local/share/buffr/crashes/` (or `%APPDATA%\buffr\crashes\`) path — the
  supervisor never wrote anything there. Replaced with a concrete command for
  capturing buffr-app's stderr directly. A proper supervisor-side child-stderr
  redirect remains a TODO.

[0.13.8]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.8

## [0.13.6] - 2026-05-19

### Fixed

- Windows supervisor (`buffr.exe`) couldn't locate `buffr-app.exe` either as a
  sibling or on `PATH` because the resolver joined the bare string `"buffr-app"`
  and `is_file()` is exact-name on Windows. Now joins `buffr-app.exe` when
  `cfg!(windows)`. Scoop / MSI installs launch correctly.

### Changed

- Scoop manifest now exposes only `buffr.exe` as a `bin` / shortcut.
  `buffr-app.exe` and `buffr-helper.exe` were also shimmed at v0.13.5, which let
  users double-click `buffr-app.exe` directly and land on a white screen (the
  supervisor is the only supported entry point — it owns the crash-restart +
  heartbeat watchdog).

[0.13.6]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.6

## [0.13.5] - 2026-05-19

### Added

- Scoop manifest pipeline. Windows packaging now produces a `.zip` payload
  alongside the existing `.msi` (same staged `buffr.exe` + `buffr-app.exe` +
  `buffr-helper.exe` + CEF runtime tree). A new `scoop-bucket` CI job renders
  `pkg/scoop/buffr.json.in` against the freshly-published zip sha256 sidecars
  and pushes the manifest to `kryptic-sh/scoop-bucket` so
  `scoop install kryptic/buffr` resolves. Requires `SCOOP_SSH_KEY` secret
  granted to the repo.

[0.13.5]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.5

## [0.13.4] - 2026-05-19

### Changed

- `no suitable wgpu adapter` error now names the actual fix: install a software
  Vulkan driver. Lists the package names for Arch (`vulkan-swrast`,
  `vulkan-intel`, etc.), Debian/Ubuntu (`mesa-vulkan-drivers`), and the
  `vulkaninfo --summary` verification command. Saves a round-trip to the issue
  tracker for users on un-configured Vulkan stacks.

[0.13.4]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.4

## [0.13.3] - 2026-05-19

### Fixed

- wgpu adapter selection now falls back through a ladder (HighPerformance →
  LowPower → software/llvmpipe) instead of bailing with
  `no suitable wgpu adapter` on the first miss. Machines with broken Vulkan +
  broken DRI2 + no usable GL (older hardware, drivers in transition) now boot
  via the software path rather than refusing to start. Logs the selected
  `AdapterInfo` so triage shows backend + driver at a glance.

[0.13.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.3

## [0.13.2] - 2026-05-19

### Fixed

- Linux packaging now ships every CEF runtime `.so` next to `libcef.so`:
  `libEGL.so`, `libGLESv2.so` (ANGLE), `libvk_swiftshader.so`, `libvulkan.so.1`
  (SwiftShader Vulkan fallback), plus `vk_swiftshader_icd.json`. Previously only
  `libcef.so` was packaged — on hardware without a system GLES library at the
  expected path, CEF emitted
  `Failed to load GLES library: /opt/buffr/libGLESv2.so` and the GPU process
  crashed before wgpu init. App then exited cleanly with no usable renderer.
  Affected: any machine where the CEF binary distribution's bundled
  ANGLE/SwiftShader libs weren't present in the install root. Fix lands in
  `xtask::collect_runtime_payload` + `stage_payload` so `.deb`, `.rpm`,
  `.tar.gz`, AUR all carry them.

[0.13.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.2

## [0.13.1] - 2026-05-19

### Fixed

- CEF cookie persistence on Linux. Login state + cookies + cache now survive
  browser restart. Two stacked fixes:
  - `--password-store=basic` propagated to every CEF subprocess via the new
    `on_before_child_process_launch` hook on `BrowserProcessHandler` (CEF does
    not auto-propagate user switches; each subprocess argv is rebuilt at spawn).
    Required when a Secret Service backend (pass-secret-service, KeePassXC,
    etc.) is detected over D-Bus but its unlock/auth dance fails — Chromium
    policy is to drop cookies rather than persist unencrypted.
  - Per-engine `CefRequestContext` creation dropped in `BrowserHost::new`. CEF
    Alloy runtime collapses any child `cache_path` to `Default/` anyway, and
    creating the per-engine context was silently blocking cookie persistence on
    top of that. Per-engine isolation tracked separately in #158 — needs Chrome
    runtime swap to be real.
- `session.json` URL fix and engine profile-dir relocation now have a fully
  working storage path under them.

### Changed

- `BackendOpenOptions.data_dir` is currently unused by the CEF backend (was
  wired into the deleted per-engine `CefRequestContext`). Will be re-wired when
  per-engine isolation lands.

[0.13.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.1

## [0.13.0] - 2026-05-18

### Removed

- X11 support on Linux. buffr now requires a Wayland session
  (`XDG_SESSION_TYPE=wayland`) and refuses to start otherwise. winit's X11
  backend is no longer compiled in. The X11 idle-inhibit backend
  (`buffr-core::inhibit::linux::x11`, 242 LoC) is gone.
- Browser-engine backends: `buffr-servo`, `buffr-ladybird`, `buffr-blitz`,
  `buffr-webkit-cocoa`, `buffr-webview2`. CEF is now the only user-facing engine
  on every platform. `buffr-webkit` (WPE WebKit) is retained on disk as an
  experimental Linux-only backend but is workspace-excluded — build standalone
  with `cargo build --manifest-path crates/buffr-webkit/Cargo.toml`.
- `--engine` CLI flag now accepts only `cef`.
- `apps/buffr-stub` (the `buffr` crates.io name-holder). All 13 published
  versions on crates.io have been yanked. Binary distribution moves entirely to
  GitHub release artifacts.
- CEF custom `buffr://` scheme registration. The scheme handler factory,
  `register_buffr_scheme`, `register_buffr_handler_factory`, and
  `BuffrSchemeHandlerFactory` are deleted.
- WebKit `data:text/html;base64,…` URL fallback for `buffr://*`.
- `buffr-engine::newtab::translate_internal_url` and `default_newtab_html`
  (data: URL helpers).
- `BrowserEngine::image_rotate` trait method (no use case).
- xvfb-based smoke test in CI (incompatible with Wayland-only Linux).

### Added

- `BrowserEngine::is_using_native_compositing` trait method to surface the
  runtime native-vs-OSR state.
- IME composition support in `buffr-webkit` via `WebKitInputMethodContext`.
- `BackendOpenOptions::internal_server` so backends receive the loopback HTTP
  server at construction. Avoids a race where the initial-tab navigation fires
  before the server can be wired post-hoc.

### Changed

- Repository structure: 12 ex-submodule crates (`buffr-bookmarks`, `buffr-cef`,
  `buffr-config`, `buffr-core`, `buffr-downloads`, `buffr-engine`,
  `buffr-history`, `buffr-modal`, `buffr-permissions`, `buffr-ui`,
  `buffr-view-source`, `buffr-zoom`) absorbed into the monorepo via
  `git subtree add` (history preserved). `.gitmodules` deleted.
  `[patch.crates-io]` dropped. Per-crate scaffolding (CHANGELOG, README,
  LICENSE, dependabot, ci.yml, deny.toml, rust-toolchain.toml, rustfmt.toml,
  .gitignore, .editorconfig) stripped — workspace-level files cover all members.
  `.editorconfig` hoisted to workspace root.
- CEF backend routes `buffr://*` through `InternalServer` (HTTP loopback)
  instead of a custom URI scheme. Mirrors the webkit pattern. CEF
  `on_register_custom_schemes` no longer touches `buffr://`; `buffr-src://`
  (view-source) keeps its custom handler.
- WebKit's `resolve_url` requires an `InternalServer` to translate `buffr://*`.
  Bind failure is now fatal at startup (was a silent data-URL fallback).
- CEF `tabs_summary` reports the display URL (`buffr://new`) instead of the
  engine-loaded `http://127.0.0.1:<port>/<token>/new`, so `session.json` saves a
  stable URL across restarts (the ephemeral loopback port no longer outlives the
  process).
- All workspace members marked `publish.workspace = true` → `publish = false`.
  No member is published to crates.io independently.
- All Wayland render path moved to an async worker thread (canonical Wayland
  backpressure fix): UI thread does CPU + memcpy + try_send, worker owns all
  wgpu mutating calls.
- Supervisor heartbeat decoupled from UI event loop (bg thread + atomic liveness
  counter). Survives sustained input and Wayland event-loop stalls.
- Loading animation now gates on the live pixel pipeline state, not the engine's
  `host_is_loading` flag (which could pin true forever).
- `--engine cef` skips engine selection state (single binary stays CEF-only).
- WebKit `prefer_native` (Wayland subsurface compositing) is now opt-in via
  `BUFFR_WEBKIT_NATIVE=1`; default is the OSR pixel-copy path. Native path
  remains experimental (chrome overlay layout + watchdog hang issues
  unresolved).
- Demoted `buffr::ui_path` per-frame traces from `info!` to `debug!`.
- crates.io: yanked all 14 buffr-\* component crates (58 versions total). Names
  remain reserved.
- GitHub: 12 submodule repos deleted under `kryptic-sh/buffr-*`.

### Fixed

- Wayland-only Linux: `wayland_globals` registry roundtrip gated behind
  native-compositing opt-in (avoided clobbering main-loop event routing on
  Wayland sessions).
- CEF OSR composite alpha: prefer Opaque over PreMultiplied to fix
  semi-transparent viewport on the OSR path.
- WebKit cookie DB path doubling (`engines/<id>/profile/engines/<id>/` →
  `engines/<id>/profile/`).
- WebKit `JSON.stringify` wrapping on `postMessage` UCM bridge
  (`[object Object]` payloads silently dropped before).
- WebKit RawDown handling: printable keys no longer emit duplicate events
  (typing produced `"Hh"`, `"Ee"`).
- WebKit `FocusFirstInput` + `ExitInsertMode` dispatch arms.
- WebKit resize-paint watchdog: `force_repaint_active()` reaches the worker so
  paint can recover after stuck-frame timeouts.
- Same-tab navigation no longer leaves a stale OSR frame: applies
  resize-wiggle + `needs_fresh` like cross-tab open does.
- wgpu OOM at surface acquire under rapid resize: poll the device every frame so
  lazy resource drops actually run (32-bit AMDGPU address-space pressure).
- Ctrl+C now works reliably on Wayland-stuck event loops via a three-layer fix
  (NewEvents check, UserEvent exit, 3s libc::\_exit abort).
- Splash gate no longer reads `host_is_loading` (could pin true forever).
- Heartbeat keeps alive under sustained input.

[0.13.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.13.0

## [0.12.0] - 2026-05-15

### Security

- **XSS chain closed at three layers.** `javascript:` and `data:` URIs are
  rejected by the omnibar resolver (treated as search queries), the IPC
  single-instance accept thread (dropped before dispatch), and the navigation
  router (`NavigationVerdict::DisallowedScheme`). Previously a crafted paste,
  malicious second-instance invocation, or attacker-controlled bookmark could
  reach `engine.navigate()` and execute arbitrary script.
- **view-source SSRF closed.** `spawn_view_source_fetch` validates the URL
  scheme is `http` or `https` before issuing the ureq fetch, blocking
  `file:///etc/passwd` and `http://169.254.169.254/` exfiltration paths.
- **IPC payload caps.** Forward payloads on the local socket are limited to 100
  URLs of 1024 bytes each before scheme validation runs.

### Fixed

- `WindowEvent::MouseWheel` no longer panics on `active_engine_dyn().unwrap()`
  during startup or after the last tab closes.
- `BlinkCdpEngine::close_all_browsers` uses blocking `send` for `Shutdown` so a
  full channel never silently leaks the worker thread + child process.
- blink-cdp page title now sourced from `Target.targetInfoChanged` (the previous
  `frame.name` read was almost always empty, blanking the tab strip after each
  navigation).
- blink-cdp `Mutex<EngineState>` lock failures recover from poison via a
  `lock_state()` helper instead of cascading panics to the UI thread.
- blink-cdp WebSocket `try_recv_text` returns `Ok(None)` for non-`Plain` (TLS)
  streams instead of blocking the worker forever.
- blink-cdp permission shim injects a `beforeunload` listener that resolves
  pending Promises with `denied` on navigation, eliminating per-navigation
  Promise leaks in SPAs.
- blink-cdp pending CDP commands carry an `Instant`; entries older than 30 s are
  swept and error-replied each iteration to bound memory under hung Chromium.
- CEF `on_dismiss_permission_prompt` removes the dismissed entry from the
  callback registry and neutral queue (slow-leak fix).
- CEF `sanitise_filename` rejects Windows reserved device stems (`CON`, `PRN`,
  `AUX`, `NUL`, `COM[1-9]`, `LPT[1-9]`) on every platform.

### Changed

- `BrowserEngine::can_go_back()` and `can_go_forward()` trait defaults changed
  from `true` to `false`. Both engines override these methods, so concrete
  behavior is unchanged.
- `EngineEvent`, `LoadState`, `CursorKind`, `MediaType` annotated with
  `#[non_exhaustive]` to allow additive variants without breaking downstream
  exhaustive matches.

### Removed

- `tracing::debug!` calls from CEF `view_rect` and `get_screen_info` (per-paint
  hot path, called at 60+ Hz).

### Dependencies

- `buffr-config` 0.4.1 → 0.4.2
- `buffr-engine` 0.1.2 → 0.1.3
- `buffr-blink-cdp` 0.1.4 → 0.1.5
- `buffr-cef` 0.1.1 → 0.1.2

## [0.11.1] - 2026-05-15

### Added

- `tracing::debug!` log when CEF backend silently drops `cache_dir` option —
  diagnostic for multi-engine on-disk layout comparison (buffr-cef 0.1.1).
- `BlinkError::CacheDirCreate` distinguishes pre-spawn `mkdir` failures from
  actual `Command::spawn` failures (buffr-blink-cdp 0.1.4).
- New unit test `migration_skips_when_new_path_already_exists` covers the
  both-old-and-new-exist case in `engine_migrate`, asserting the production
  guard preserves new-layout content and leaves old in place.

### Dependencies

- `buffr-cef` 0.1.0 → 0.1.1
- `buffr-blink-cdp` 0.1.3 → 0.1.4

## [0.11.0] - 2026-05-15

### Fixed

- CEF engine no longer spills its Chromium profile flat into `~/.cache/buffr/`
  (`Default/`, `ShaderCache/`, `chrome_debug.log`, etc. at the root). Each CEF
  engine instance now gets its own namespaced directory under
  `~/.cache/buffr/engines/<engine-id>/`. Phase 11a (#96).
- blink-cdp profile directory moved from `~/.local/share/buffr/blink-cdp/<id>/`
  to `~/.local/share/buffr/engines/<id>/profile/`. Existing profiles are
  migrated automatically on first launch via `fs::rename`. Phase 11a (#96).

### Added

- Startup migration shim (`engine_migrate`) moves blink-cdp profiles from the
  pre-11a layout to the new `engines/<id>/profile/` path. Warns on stale CEF
  flat state without auto-deleting. Migration is skipped in `--private` mode.
- `BackendOpenOptions.cache_dir: Option<&Path>` — ephemeral cache directory for
  backends that support a persistent/ephemeral split. Phase 11b (#96).
- blink-cdp now passes `--disk-cache-dir=<cache_root>/engines/<id>/` to headless
  Chromium. HTTP cache, shader cache, and code cache land in
  `~/.cache/buffr/engines/<id>/` while cookies, IndexedDB, and prefs remain in
  `~/.local/share/buffr/engines/<id>/profile/`. Phase 11b (#96).

### Dependencies

- `buffr-engine` 0.1.1 → 0.1.2
- `buffr-blink-cdp` 0.1.2 → 0.1.3

## [0.10.0] - 2026-05-15

### Fixed

- **blink-cdp audit Phase 10 — 23 bugs fixed** (P0-1 through P2-8). All
  blink-cdp backend behaviors that should match CEF now do. Highlights:
  - Address bar updates after in-page navigation (was frozen)
  - History back/forward, reload, hard reload, stop, and all scroll keybinds
    work (were silent no-ops)
  - Permissions/find/context-menu shims fire on the initial page load (were
    registered too late)
  - Download notices carry the right filename and path
  - `view-source:` no longer freezes the UI thread
  - Permission prompts show readable origin for `data:` URLs
  - `getUserMedia` requesting both audio + video now prompts for both
  - Tab strip loading indicator reflects real loading state
  - `open_tab_at` honors the insert index
  - Graceful engine shutdown via Drop impl (subprocess cleanup)

### Changed

- `BackendOpenOptions.find_sink` field added (buffr-engine 0.1.1).
- `BlinkCdpEngine::open_tab_internal` uses `about:blank` then explicit
  `Page.navigate` so all shims are registered before page content loads.
- Live tab title via `Target.targetInfoChanged` subscription.

### Dependencies

- `buffr-engine` 0.1.0 → 0.1.1
- `buffr-blink-cdp` 0.1.1 → 0.1.2

## [0.9.1] - 2026-05-15

### Changed

- **Promote `buffr-engine`, `buffr-cef`, `buffr-blink-cdp` to standalone repos +
  crates.io** (#72). All three new crates extracted to their own GitHub repos
  (`kryptic-sh/buffr-engine`, `kryptic-sh/buffr-cef`,
  `kryptic-sh/buffr-blink-cdp`) with full sibling boilerplate (README,
  CHANGELOG, LICENSE, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, deny.toml,
  rust-toolchain.toml, rustfmt.toml, .editorconfig, tag-driven CI). Published to
  crates.io as `buffr-engine@0.1.0`, `buffr-cef@0.1.0`, `buffr-blink-cdp@0.1.1`.
  Umbrella now references all three as submodules under `crates/`, mirroring the
  buffr-core / buffr-modal / buffr-config / buffr-ui pattern.
  `[patch.crates-io]` table unchanged — keeps in-tree resolution for umbrella
  builds.

  Cascade BCTPs landed before promotion to unblock standalone CI:
  - `buffr-modal` 0.1.4 → 0.1.5 — `PageAction::Engine(String)` variant
  - `buffr-config` 0.4.0 → 0.4.1 — engines table + instances + per-domain rules
  - `buffr-ui` 0.2.1 → 0.2.2 — tab-strip engine badge + hover outline
  - `buffr-core` 0.6.3 → 0.7.0 — **minor bump**, dropped all CEF integration
    code (~30 public types removed: `BrowserHost`, `Tab`, `OsrFrame`,
    `BuffrApp`, `AudioEvent`, `PopupQueue`, `PermissionsQueue`, etc.).
    buffr-core is now backend-agnostic.

  `buffr-core`'s standalone CI now resolves cleanly (`buffr-engine = "0.1"`
  available on crates.io). Apps layer unchanged — workspace builds via
  `[patch.crates-io]` paths.

## [0.9.0] - 2026-05-15

### Added

- **Phase 8: blink-cdp feature parity sprint** — seven sub-phases bring the
  blink-cdp backend up to the same baseline UX as CEF for everyday browsing.
  After this release, blink-cdp is viable as a default engine for typical
  workloads, not just experimental opt-in.
  - **8a — permissions** (#88). Geolocation, Notifications, Microphone, and
    Camera prompts now route through the same status-line prompt UI as CEF.
    Implemented via a JS shim injected with
    `Page.addScriptToEvaluateOnNewDocument` that wraps
    `navigator.geolocation.getCurrentPosition`,
    `Notification.requestPermission`, `navigator.permissions.query`, and
    `navigator.mediaDevices.getUserMedia`. The shim posts to a
    `Runtime.addBinding` named `__buffrPermissionRequest`; the CDP worker thread
    receives `Runtime.bindingCalled` events and pushes a neutral
    `PendingPermission` onto the shared `PermissionsQueue`. User answers resolve
    back to the page via `Runtime.evaluate` calling
    `__buffrPermissionResolve(<id>, granted|denied)`. The permission types
    (`PendingPermission`, `PermissionsQueue`, `PromptOutcome`, `Capability`)
    moved to `buffr-engine::permissions`; the queue is now a `BrowserEngine`
    trait method `permissions_queue()` so apps drain from both backends
    uniformly.

  - **8b — downloads** (#84).
    `Browser.setDownloadBehavior { behavior: "allow", downloadPath, eventsEnabled: true }`
    is configured per engine at startup. The worker subscribes to
    `Browser.downloadWillBegin` and `Browser.downloadProgress` events, mapping
    `inProgress`/`completed`/ `canceled` states onto the existing
    `buffr-downloads` SQLite store via
    `Downloads::record_started`/`update_progress`/`record_completed`/
    `record_canceled`. Completed downloads push to `DownloadNoticeQueue` for
    status-line surface (same as CEF). Per-engine download directory defaults to
    `<data_root>/blink-cdp/<id>/downloads` so each instance gets isolated
    storage.

  - **8c — find-in-page** (#83). `/`-keymap now works on blink-cdp tabs via an
    injected TreeWalker-based shim that wraps text-node matches in
    `<span class="__buffr-find-match">` with a current-match accent on
    `__buffr-find-current`. `start_find` / `find_next` / `find_prev` /
    `stop_find` on the `BrowserEngine` trait call
    `Runtime.evaluate("__buffrFindNext('query', false)")` etc.; the JS returns
    `{ current, total }` which the worker writes to the existing
    `FindResultSink`. No `eval`, no `innerHTML` — pure DOM mutation.

  - **8d — context-menu hit-test** (#87). Right-click on a blink-cdp tab now
    populates the same context menu as CEF (Copy Link, Save Image, Copy Image
    URL, Open Image In New Tab, etc.). Implemented via a capture-phase
    `contextmenu` event listener injected on every page; the listener walks the
    target ancestor chain for `<a href>`, `<img>`, `<video>`, `<audio>`,
    editable nodes, plus `window.getSelection()`, then posts the result to the
    `__buffrContextMenu` Runtime binding. The worker parses the JSON payload
    into a neutral `ContextMenuRequest` matching CEF's shape so the apps-layer
    menu builder is reused unchanged.

  - **8e — IME composition** (#86). Three new `BrowserEngine` trait methods
    (`ime_set_composition`, `ime_commit`, `ime_cancel`) route winit
    `WindowEvent::Ime(Preedit|Commit|Enabled|Disabled)` events to CDP
    `Input.imeSetComposition { text, selectionStart, selectionEnd }` and
    `Input.insertText { text }`. International input (Japanese, Chinese, Korean
    composition windows; dead-key combining accents) now works on blink-cdp
    tabs.

  - **8f — `buffr://` and `view-source:` schemes** (#81). Chromium's network
    stack rejects unknown schemes before CDP `Fetch` can intercept them, so the
    engine translates internal URLs at the navigation layer: `buffr://new` and
    `buffr://settings` route to `data:text/html;base64,<page_html>`;
    `view-source:<url>` sync-fetches the target via `ureq`, HTML-escapes it into
    a dark-themed `<pre>` envelope, and navigates to a data: URL.
    `EngineState.original_urls` stashes the human-readable URL per-tab so
    `active_tab_live_url` / `tabs_summary` return `buffr://new` instead of the
    opaque base64 blob. The `NewTabHtmlProvider` is now shared between CEF and
    blink-cdp through `BlinkCdpBackend::register_new_tab_handler`.

  - **8g — Picture-in-Picture** (#90, resolves #31). Picture-in-Picture now
    works on blink-cdp tabs via `Runtime.evaluate` invoking
    `HTMLVideoElement.requestPictureInPicture()`. The IIFE finds the most
    relevant video (preferring currently-playing, then unmuted, then first) and
    toggles between enter/exit. This closes #31 (the original CEF limitation)
    for users on the blink-cdp engine — switch a domain via `:engine blink-cdp`
    or a per-domain engine rule to get PiP on sites where CEF can't deliver it.

  Test count: 887/887. Tab-strip badge `BL` (blink-cdp) tabs now have full
  CEF-equivalent UX across permissions, downloads, find, context menu, IME,
  internal schemes, and PiP.

## [0.8.1] - 2026-05-15

### Fixed

- **CEF: per-engine on-disk cache isolation via `RequestContext`** (#79).
  `BrowserHost::new_with_options` now takes `data_dir: Option<&Path>`. When
  `Some`, builds a `cef::RequestContextSettings` with `cache_path = data_dir`,
  calls `cef::request_context_create_context`, and passes the resulting
  `RequestContext` to `browser_host_create_browser_sync` (the 6th arg slot that
  was previously `None`). The host holds the context in a `Mutex` so the
  `&self`-method `create_browser` can lock-borrow `as_mut()`. When `None`, CEF
  falls back to its global default cache. The apps-layer "advisory only in Phase
  3" warning is deleted — `data_dir` is now real. Two CEF engine instances
  configured with distinct `data_dir` no longer share cookies, cache,
  local-storage, or IndexedDB.

- **blink-cdp: default `--user-data-dir` lives under XDG data, not `/tmp`**
  (#89). The fallback when an `[[engines.instances]]` block has no explicit
  `data_dir` was `/tmp/buffr/blink-cdp/<instance-id>`, which was cleared on
  reboot and did not respect `--private` mode. It now resolves to
  `<AppState.data_root>/blink-cdp/<instance-id>`. `data_root` is captured at
  startup from `resolve_paths(cli.private)`, so in normal mode it's the XDG data
  dir and in `--private` mode it's the per-pid `TempDir` that gets dropped at
  exit. Private-mode blink-cdp profiles are now torn down with the rest of the
  temp tree; persistent-mode profiles survive across reboot.

## [0.8.0] - 2026-05-15

### Changed

- **Engine refactor Phase 6 — apps layer fully agnostic** (#95). The
  `cef_engines: HashMap<EngineId, Arc<BrowserHost>>` parallel map at the apps
  layer is deleted. All ~113 reach-through call sites now route through
  `Arc<dyn BrowserEngine>` via `self.engines`. Seven sub-phases shipped:
  - **6a** — `BrowserEngine` trait widened with 14 `popup_*` methods (sinks,
    resize, close, drain_address/title_changes, history_back/forward, OSR
    input). `PopupQueue`, `PopupCreateSink`, `PopupCloseSink`,
    `PendingPopupAlloc`, `PopupCreated` and their helpers move from `buffr-cef`
    to `buffr-engine`. `BlinkCdpEngine` gets no-op stubs (deferred to a future
    CDP popup pass).
  - **6b** — trait widened with 6 `hint_*` methods (`cancel_hint`,
    `backspace_hint`, `feed_hint_key`, `hint_status`, `is_hint_mode`,
    `pump_hint_events`). `HintStatus` moves to `buffr-engine::hint`.
    `HintAction` (formerly in `buffr-core`) re-exports through `buffr-engine`.
  - **6c** — trait widened with 22 frame/edit/media/JS/devtools/downloads
    methods (`frame_copy/cut/paste/paste_plain/redo/select_all/undo`,
    `media_play_pause/picture_in_picture/toggle_controls/toggle_loop/toggle_mute`,
    `image_rotate`, `run_js`, `run_main_frame_js`,
    `run_edit_attach/cycle/detach/focus`, `run_media_probe`, `start_download`,
    `show_dev_tools_at`). Default impls used heavily — void methods log a
    `debug!`, `Result` returners default to `Err(EngineError::Unimplemented)`.
    `BlinkCdpEngine` overrides `run_js` and `run_main_frame_js` via CDP
    `Runtime.evaluate` and `show_dev_tools_at` via the existing CDP inspector
    URL.
  - **6d** — trait widened with 8 clipboard/audio/cursor/favicon/context-menu
    methods. `ClipboardReader` becomes `Arc<dyn ClipboardRead>` (trait moved to
    `buffr-engine::clipboard`); `FaviconUpdate` is a new neutral type in
    `buffr-engine::favicon`. Audio drain fan-out switches from
    `cef_engines.values()` to `engines.values()`.
  - **6e** — `ProfilePaths` moves from `buffr-cef` to `buffr-engine::profile`.
    New-tab HTML constants (`NEW_TAB_HTML_TEMPLATE`, `NEW_TAB_KEYBINDS_MARKER`,
    `NEW_TAB_SPLASH_ART_MARKER`, `NEW_TAB_URL`, `SETTINGS_URL`) move to
    `buffr-engine::newtab`. Apps imports `buffr_engine::TabId` directly instead
    of routing through `buffr-cef`.
  - **6f** — `cef_engines` map and `active_host()` method deleted. 38 call sites
    migrated to `active_engine_dyn()`. New trait methods filled the last gaps
    (`dispatch`, `is_loading`, `can_go_back`, `can_go_forward`,
    `drain_context_menu_requests`).
  - **6g** — `Backend` trait introduced in `buffr-engine::backend`. `CefBackend`
    (owns `BuffrApp`, drives `cef::initialize`, message pump, scheme handlers,
    cookie store, device-scale, accessibility hints) and `BlinkCdpBackend`
    (no-op lifecycle, `open_engine` constructs `BlinkCdpEngine`) provide the two
    backends. Apps holds `Arc<dyn Backend>`; all 10 CEF lifecycle call sites
    (`cef_initialize`/`cef_shutdown`/`do_message_loop_work`/`load_cef_library`/
    `execute_process_for_subprocess`/`take_scheduled_message_pump_delay_ms`/
    `delete_all_cookies`/`set_force_renderer_accessibility`/
    `set_device_scale_factor`) route through the trait.

  Final state: `apps/buffr-app/src/main.rs` references only `CefBackend`,
  `CefEngineSinks`, and the CEF-specific permissions queue types from
  `buffr-cef`. All other engine interaction goes through `buffr-engine` trait
  surfaces. Test count: 820/820.

## [0.7.1] - 2026-05-14

### Fixed

- **macOS package build broken in v0.7.0**: `cef::Settings` binding in
  `buffr_cef::cef_initialize` was `let` not `let mut`, but the
  `#[cfg(target_os = "macos")]` block at `crates/buffr-cef/src/lib.rs:166`
  assigns to `external_message_pump`, `browser_subprocess_path`,
  `framework_dir_path`, and `resources_dir_path`. Linux cfg-gated the block out
  so the issue was invisible locally; macOS package CI caught it.

## [0.7.0] - 2026-05-14

### Added

- **blink-cdp: replace `captureScreenshot` poll with `Page.startScreencast`
  streaming** (#91). The 5 FPS `Page.captureScreenshot` poll loop is removed.
  The worker now sends `Page.startScreencast` (PNG, `everyNthFrame=1`, at
  viewport dimensions) when a session becomes active, and `Page.stopScreencast`
  when it is deactivated or closed. Chromium pushes frames at its native render
  cadence; backpressure is provided by the mandatory `Page.screencastFrameAck`
  reply which the worker sends immediately after decoding each frame. On resize,
  the worker stops and restarts the screencast with the new
  `maxWidth`/`maxHeight` so Chromium re-renders at the correct dimensions. Tab
  switches send stop on the old session and start on the new one. Interactive
  pages now feel as responsive as CEF tabs; idle tabs consume no screenshot
  bandwidth.

- **blink-cdp: `:devtools` opens Chromium inspector in system browser** (#82).
  `BlinkCdpEngine` now implements `open_devtools` on the `BrowserEngine` trait.
  When `:devtools` is invoked with the blink-cdp engine active, buffr builds a
  `http://127.0.0.1:<port>/devtools/inspector.html?ws=…/devtools/page/<target_id>`
  URL and hands it to the OS via the `open` crate so the user's default browser
  hosts the Chromium DevTools inspector. The debug port is tracked on
  `EngineState.debug_port` (set once at `BlinkCdpEngine::new` from the ephemeral
  port chosen by `pick_free_port`). `BrowserEngine::open_devtools` is a new
  trait method with a default no-op body so backends that don't support it
  return `Ok(())` without changes. `buffr-cef` overrides it to call the existing
  `show_dev_tools_at(None, None)` inherent method. The `dispatch_action` arm in
  `apps/buffr-app` routes `PageAction::OpenDevTools` through the active engine
  trait surface (same pattern as the zoom arms added in #85) so both CEF and
  blink-cdp receive the call correctly.

- **tab strip: engine-badge 2-char glyph + hover outline + status-line tooltip**
  (#94). The flat 4-px coloured band on non-default-engine tab pills is replaced
  by a wider badge column that renders a 2-character uppercase label derived
  from the engine id (e.g. `"BL"` for `blink-cdp`, `"WK"` for `webkit`, `"??"`
  for empty ids). Badge width is `font::text_width("WW") + 2 × BADGE_SIDE_PAD`
  so it adapts to the active font automatically. When the cursor hovers over a
  badged tab, a 1-px white outline is drawn around the badge rectangle and the
  engine id is written to the statusline as `"engine: <id>"`. The single-engine
  (CEF-only) path is unchanged — no badges, no tooltip. New
  `EngineRouter::badge_label_for` method and `badge_label_text` helper (6 new
  unit tests). `TabView` gains `engine_label: Option<String>` and
  `hovered: bool` fields; `Statusline` gains `engine_hint: Option<String>`
  rendered on the right-hand cell.

- **blink-cdp: zoom in/out/reset via `Runtime.evaluate` + per-tab tracking**
  (#85). `BlinkCdpEngine` now implements `zoom_in`, `zoom_out`, and `zoom_reset`
  (new default-no-op methods on `BrowserEngine`). Zoom is tracked per tab as a
  linear CSS factor (`1.0` = 100 %, step `0.25`, clamped to `[0.25, 5.0]`) and
  injected via `document.body.style.zoom` through `Runtime.evaluate`. The worker
  thread stores per-session zoom levels and re-applies them on every
  `Page.frameNavigated` event so navigation does not silently reset the scale.
  `active_zoom_level()` now returns the tracked factor (was always `0.0`). The
  step size (0.25) matches the CEF backend's `adjust_zoom` delta.

### Changed

- **blink-cdp: free-port probe replaces hardcoded 9222** (#92).
  `BlinkCdpEngine::new` now calls `pick_free_port()` — a
  `TcpListener::bind("127.0.0.1:0")` probe — instead of the fixed port 9222.
  Multiple engine instances can coexist without port conflicts, and starting
  buffr when 9222 is already in use by another process no longer prevents
  initialisation. New error variant `BlinkError::PortProbe(std::io::Error)` is
  surfaced when the OS cannot allocate an ephemeral port.

### Added

- **Engine refactor — Phase 5 (initial cut): `:engine` command, tab strip engine
  badge, `buffr://settings` scaffold** (#76, parent #54). Three user-visible
  pieces shipped in this pass:
  - **`:engine <id>` command** — rebinds the active tab to a different rendering
    engine (same URL, new engine). Implemented as `Command::Engine(String)` in
    `buffr-core/src/cmdline.rs` (parsed from the `:` command bar), dispatched to
    `PageAction::Engine(String)` in `buffr-modal/src/actions.rs`, and handled in
    `AppState::dispatch_action` in `apps/buffr-app/src/main.rs`. Pattern:
    snapshot URL → `open_tab` on target → `close_active` on source →
    `active_engine` swap. Unknown ids emit a `warn!` and are silently ignored
    (no crash). `"engine"` added to `COMMAND_NAMES` for omnibar completion.
  - **Tab strip engine badge** — a 4-px coloured band at the left edge of each
    unpinned tab pill, visible only when >1 engine is registered. Colour is a
    deterministic DJB2 hash of the engine id string mapped onto an 8-colour
    palette (red / orange / yellow / green / blue / violet / pink / grey). The
    primary `"cef"` engine id is exempt (no badge). Implementation:
    `show_badges()` + `badge_color_for()` methods on `EngineRouter`;
    `engine_badge: Option<u32>` field on `TabView`; badge painted in
    `TabStrip::paint` before the favicon/title. `refresh_tab_strip` populates
    the badge from the router on every tick.
  - **`buffr://settings` scaffold** — new route in the
    `BuffrSchemeHandlerFactory` (`crates/buffr-cef/src/new_tab.rs`). Requests to
    `buffr://settings` are served a minimal HTML page listing registered engines
    and active routing rules. `SettingsHtmlProvider` closure type +
    `settings_html(engines, rules)` helper added so callers can inject live
    router state without a restart.
    `register_buffr_handler_factory_with_settings` is the new preferred entry
    point; the original `register_buffr_handler_factory` retains its signature
    and falls back to a static placeholder. `SETTINGS_URL` constant exported
    from `buffr-cef`.
  - Sub-issues for unimplemented feature-parity work filed against #76 (see
    commit log for issue numbers).
  - Six new unit tests: four in `engine_router` for badge logic, two in
    `buffr-core/cmdline` for the `:engine` parser. 725/725 tests pass.

- **Engine refactor — Phase 4: blink-cdp second backend spike** (#75, parent
  #54). New `buffr-blink-cdp` workspace crate (`crates/buffr-blink-cdp/`)
  implements `BrowserEngine` via headless Chromium driven over Chrome DevTools
  Protocol. No Chromium binary bundled — system `chromium`, `google-chrome`, or
  `chromium-browser` required at runtime. Implemented in Phase 4: tab
  create/close (`Target.createTarget` + `Target.attachToTarget`), navigate
  (`Page.navigate`), OSR paint via `Page.captureScreenshot` polled at ~5 FPS
  (BGRA decode → `SharedOsrFrame`), mouse events (`Input.dispatchMouseEvent`),
  keyboard events (`Input.dispatchKeyEvent`), viewport resize
  (`Page.setDeviceMetricsOverride`), and tab bookkeeping. Stubbed with
  `EngineError::Unimplemented`: `duplicate_active`, `reopen_closed_tab`, and all
  popup/hint/find/zoom/devtools/scheme-handler/audio/video methods.
  `AppState::engines` changed from `HashMap<EngineId, Arc<BrowserHost>>` to
  `HashMap<EngineId, Arc<dyn BrowserEngine>>` (Phase 4 dyn-dispatch map); a
  parallel `cef_engines: HashMap<EngineId, Arc<BrowserHost>>` provides
  CEF-specific reach-through (popup*\*, hint*\*, drain_audio_events, etc.).
  `EngineError::Unimplemented { method: &'static str }` variant added to
  `buffr-engine`. Demo routing config at `examples/blink-cdp-demo-config.toml`.
  Architecture: one `std::thread` per engine instance owns the `tungstenite`
  WebSocket; trait methods send `Command` over an `mpsc::SyncSender` and wait on
  a response channel (sync→async bridge, no tokio).

- **Engine refactor — Phase 3: multi-engine runtime** (#74, parent #54).
  Multiple `BrowserHost` instances now live simultaneously inside one CEF
  process; each is registered under a unique `EngineId` in a
  `HashMap<EngineId, Arc<BrowserHost>>` in `AppState`. New
  `[[engines.instances]]` config table (`id`, `backend`, optional `data_dir`;
  advisory-only in Phase 3 — CEF cache is process-global; Phase 5+ isolation via
  `RequestContext`). When `instances` is empty a single
  `{ id: "cef", backend: "cef" }` instance is synthesised so single-engine
  configs need no changes. Config validation: unique ids, non-empty
  `id`/`backend`, `default` and rule `engine` fields must reference declared
  instances. Engine registry loop in `resumed()` replaces single-host
  construction; each instance gets its own OSR wake callback, frame rate, and
  device scale. Paint multiplexer: `active_host()` reads from
  `engines[active_engine]`; resize/scale fan-out to all engines; audio/video
  activity ORed across all engines. Cross-engine navigation:
  `classify_navigation()` pure helper + `check_cross_engine_nav()` called after
  each `pump_address_changes()` — opens a new tab on the target engine and
  closes the source tab. Tab-count exit checks fan across all engines.
  `active_engine: EngineId` field tracks focused engine; updated on cross-engine
  nav. `buffr-cef/README.md` added documenting `cef::initialize` singleton,
  global cache-path constraint, and multi-instance model. Four
  `classify_navigation` unit tests; eleven `engines.instances` config tests.
  715/715 tests pass. CEF global cache isolation (Phase 5+) and per-engine
  helper subprocess args tracked as follow-ups to #74.

- **Engine refactor — Phase 2: engine router + per-domain rules config** (#73,
  parent #54). New `EngineId` newtype in `buffr-engine` (serde transparent,
  `Display`, `Hash`, `Eq`). New `[engines]` table in `buffr-config`: `default`
  engine id + `[[engines.rules]]` with host-glob `match` and `engine` fields;
  validated at load time (non-empty fields required; registry check deferred to
  router). New `engine_router` module in `apps/buffr-app`: `EngineRouter`
  (backed by `globset` host-glob matching, case-insensitive, `url` host
  extraction; falls back to default for host-less URLs — `about:blank`, `data:`,
  `file://`); `EngineRouterBuilder` (register / default / rule / build);
  `RouterError` (`UnknownEngine`, `InvalidGlob`, `EmptyRegistry`). Router built
  after `BrowserHost` construction in `resumed()`; host wrapped in
  `Arc<dyn BrowserEngine>` and stored alongside `Arc<BrowserHost>` for the tab-
  spawn routing path. Three tab-spawn actions (`TabNew`, `TabNewRight/Left`,
  session + CLI tab restore) route through `routed_open_tab*` helpers; remaining
  sites unchanged (single-backend, behaviour identical). Shutdown:
  `engine_router` dropped before CEF shutdown to preserve `Arc<BrowserHost>`
  drop ordering. Nine `EngineRouter` + builder tests; three `buffr-config`
  engines-section tests. 704/704 tests pass. Groundwork for multi-engine runtime
  (Phase 3, #74).

### Changed

- **Engine refactor — Phase 1: `buffr-engine` trait + `buffr-cef` backend
  extraction** (#72, parent #54). All CEF integration moved out of `buffr-core`
  into a new `buffr-cef` backend crate; a new `buffr-engine` trait crate defines
  the engine-agnostic surface (`trait BrowserEngine`, neutral `TabId` /
  `NeutralKeyEvent` / `MouseButton` / `OsrFrame` / `OsrViewState` types).
  `buffr-cef::BrowserHost` now implements `buffr_engine::BrowserEngine` and
  exposes only neutral types at its public boundary — no `cef::*` leaks through
  `buffr-core`, `apps/buffr-app`, or `apps/buffr-helper`. CEF library load and
  settings construction encapsulated inside `buffr-cef`; helper binary routes
  through `buffr_cef::execute_subprocess()`. No user-visible behaviour change;
  686/686 tests pass. Groundwork for multi-engine routing (Phase 2, #73) and
  alternate backends (Phase 4, #75).

## [0.6.3] - 2026-05-13

### Added

- **Right-click context menu on tab-strip entries** (#37). Right-clicking a tab
  now opens a tab-specific menu — Reload Tab, Duplicate Tab, Pin/Unpin Tab, Copy
  Tab URL, Close Tab, Close Other Tabs, Close Tabs to the Right — instead of the
  page/selection menu. "Close Other Tabs" / "Close Tabs to the Right" are dimmed
  when there's nothing they'd close and skip pinned tabs (Chrome behaviour);
  closing a pinned tab routes through the existing confirmation prompt;
  Duplicate Tab inserts the copy right after the source. Keymap (`x`, `gt`/`gT`,
  pin toggle) stays canonical — the menu mirrors it for discoverability.

### Submodules

- `buffr-core` `0.6.2` → `0.6.3` (adds `ContextMenuTarget`, the tab
  `ContextMenuItem` variants, and `build_tab_context_menu_model`).

## [0.6.2] - 2026-05-13

### Added

- **Crash-loop detection** (#61). `apps/buffr-app/src/crash_guard.rs` tracks
  recent startup timestamps in `<data_dir>/launch.json`. Three startups inside a
  60-second window without a clean exit between them is treated as a crash loop:
  the saved `session.json` is moved aside to `session.json.crashed-<unix_ts>`
  and the new launch starts from the homepage instead of restoring the killer
  URL set. Graceful shutdown paths (last-tab-close, `:q`, window close, Ctrl+C)
  clear the tracker. Skipped under `--private`.

### CI

- Tag-gated the 5 heavy packaging jobs (`linux-package`, `macos-package`,
  `windows-package`, `flatpak`, `snap`) — main-push CI now runs only
  lint+test+smoke+deny+macos-cross (~3min) instead of the full ~17min matrix.
  Tag pushes still get the full pipeline.
- Added `~/.local/share/flatpak` + `flatpak/.flatpak-builder` caches to the
  Flatpak job (GNOME runtime is ~2-3GB per arch; primes at next tag).
- Fanned `bundle-macos-cross` out from behind `needs: [linux]` — runs in
  parallel with the linux build instead of waiting for it.
- Split `fmt` + `clippy` out of the `linux` job into a parallel `lint` job.
- Swapped `cargo test` for `cargo nextest run` (workspace tests run ~3× faster
  locally; doctests covered by a separate `cargo test --doc` pass).

## [0.6.1] - 2026-05-12

### Added

- **Favicon disk cache** (#71). Decoded favicon bitmaps now persist to
  `<data_dir>/favicons.sqlite` keyed by origin (`scheme://host[:port]`).
  Restored tabs paint their cached favicon on the first tick — before CEF's
  asynchronous `download_image` callback fires — and so do new tabs to any
  previously-seen origin (omnibar, hint, popup, middle-click, view-source). A
  per-tick runtime scan compares each tab's current URL against the one last
  cache-checked for that `browser_id`, enqueueing prefills on change without
  per-call-site wiring. CEF-delivered bitmaps unconditionally overwrite the
  cached entry for the tab's current origin (no staleness). Skipped under
  `--private` and when `[general] show_favicons = false`.

### Submodules

- `buffr-core` bumped `0.6.1` → `0.6.2` (adds `FaviconCache`, `CachedFavicon`,
  `origin_of`).

## [0.6.0] - 2026-05-12

### Changed

- **wgpu bumped 22.1.0 → 29.0.3** (#25). API churn across 7 major versions
  required `render.rs` rewrites: `ImageCopyTexture` → `TexelCopyTextureInfo`,
  `ImageDataLayout` → `TexelCopyBufferLayout`, new required fields on
  `RenderPassDescriptor` (`multiview_mask`), `RenderPassColorAttachment`
  (`depth_slice`), and `DeviceDescriptor` (`experimental_features`, `trace`).
  `adapter.request_device` lost its trace-path arg (folded into
  `DeviceDescriptor::trace`). `InstanceDescriptor` lost `Default` —
  `InstanceDescriptor::new_without_display_handle()` instead.
  `surface.get_current_texture()` now returns a `CurrentSurfaceTexture` enum
  with explicit `Occluded` / `Suboptimal` / `Outdated` / `Lost` / `Validation`
  variants — `OutOfMemory` is folded into `Lost`. All wgpu mutating calls remain
  on the async render worker thread.
- **hjkl-bonsai bumped 0.3.0 → 0.6.1** (#69) — picks up predicate/directive
  dispatcher (helix + nvim-treesitter parity over stock tree-sitter) and the new
  `XDG` resolver via `hjkl-xdg`. Consumer code passes `&ManifestMeta` to
  `GrammarLoader::user_default` and `Grammar::load`.
- **sha2 0.10.9 → 0.11.0** (#26).
- **signal-hook 0.3.18 → 0.4.4** (#68).
- **hjkl-config 0.2.0 → 0.2.1; nix 0.31.2 → 0.31.3** (#67 patch group).
- **`actions/download-artifact` 4 → 8** in CI (#66).
- **Workflow names switched to PascalCase.**

### Submodules

- **`buffr-view-source` extracted to its own repo + crates.io publication**
  (`buffr-view-source = "0.1"`). Previously an in-tree workspace member; now
  lives at `crates/buffr-view-source/` as a git submodule, matching the pattern
  of the other nine `buffr-*` crates.
- **All nine pre-existing `buffr-*` submodules bumped** to their latest tags +
  published to crates.io. The umbrella binary's behavior is unchanged from
  v0.5.3 (path-patched the whole time); this catches the published versions up
  to the code that's been shipping. Notable submodule bumps:
  - `buffr-core` 0.5.0 → 0.6.1 (`buffr-src:` CEF scheme handler, splash overlay,
    Chromium autofill / password-manager UI disabled, `hjkl-clipboard` 0.4 → 0.5
    wrapped in `Arc`)
  - `buffr-config` 0.3.0 → 0.4.0 (omnibar prefix dispatch — closes #47)
  - `buffr-ui` 0.2.0 → 0.2.1 (CI maintenance)
  - `buffr-modal` 0.1.2 → 0.1.3 (CI + CHANGELOG backfill)
  - `buffr-{bookmarks,history,permissions,zoom}` → 0.1.2 (CI maintenance)
  - `buffr-downloads` 0.1.1 → 0.1.3 (CI maintenance; 0.1.2 was a botched
    CHANGELOG cut)

### Documentation

- Web install page (`web/index.html`): removed install gaps, unified install
  styles, normalized sibling rails.

## [0.5.3] - 2026-05-06

### Changed

- **CI pipeline collapsed back into a single `ci.yml`.** The 3-stage
  `workflow_run` chain (`ci.yml` → `build.yml` → `release.yml`) had a
  fundamental flaw: `workflow_run`-triggered runs reset `head_branch` to the
  default branch, so a tag-push's ref does not propagate past the first hop.
  v0.5.2's release was skipped three times in a row before being shipped via
  manual `workflow_dispatch`. The new layout uses one workflow with job-level
  `needs:` + `if:` gates: PRs run lint+test only, pushes to `main` add the build
  matrix, and tag pushes add publish/AUR/brew. Cross-workflow
  `dawidd6/action-download-artifact@v6` was swapped for stock
  `actions/download-artifact@v4` since artifacts now live in the same run.

## [0.5.2] - 2026-05-06

### Added

- **Custom search-engine prefix dispatch in the omnibar** (closes #47).
  `[search.engines.<name>]` blocks accept an optional `prefix` shortcut. An
  omnibar input of `<prefix> <query>` routes to that engine instead of
  `default_engine` — `g rust closures` searches Google, `ddg vim folding`
  searches DuckDuckGo, plain `cats` falls through. Bare prefix words with no
  query fall through. Prefix collisions across engines are rejected at config
  validation time.

### Changed

- **CI split into a 3-stage pipeline** (`ci.yml` → `build.yml` → `release.yml`).
  `ci.yml` runs lint + test + smoke on every push/PR. `build.yml` fires via
  `workflow_run` after `ci.yml` succeeds on a push and produces every platform
  artifact. `release.yml` fires via `workflow_run` after `build.yml` succeeds
  and only proceeds when `head_branch` matches a `v*.*.*` semver tag, publishing
  the GitHub Release, AUR-bin, and Homebrew tap. PRs are filtered out of the
  build/release stages so untagged main pushes never publish.

## [0.5.1] - 2026-05-06

### Added

- **Animated splash on the new-tab page and during the loading window** (closes
  #35). Integrates `hjkl-splash` 0.2 to drive a cursor that traces each letter's
  spine of the `buffr` wordmark. The loading-anim path (when CEF hasn't painted
  yet) paints the wordmark into the chrome buffer; the new-tab page
  (`buffr://new`) renders it as a per-cell CSS grid (rectangles via
  `background: currentColor`) and the host pushes the cursor frame's HTML into
  `#buffr-splash` per tick via `execute_javascript`. Animation cadence is driven
  by `hjkl-splash`'s wall clock so paint rate (scrolling, etc.) cannot
  accelerate the wordmark.

### Fixed

- **wtype / Wayland virtual-keyboard typing into web fields** (closes #36).
  `wtype` and similar virtual-keyboard tools synthesize an xkb keymap that
  places characters on arbitrary scancodes (Escape, Backspace, Tab); the apps
  layer was forwarding those scancodes to CEF as `VK_ESCAPE` / `VK_BACK` /
  `VK_TAB`, dropping characters mid-string (a typed email address would lose its
  `.` and the password manager would jump fields). Forward via the _character_
  the text-input layer reports when it disagrees with the physical scancode;
  punctuation (`. , ; / ' [`, etc.) also now maps to the expected `VK_OEM_*`
  codes.

### Changed

- **Workspace `default-members` expanded** to all three app binaries
  (`apps/buffr`, `apps/buffr-app`, `apps/buffr-helper`). A bare `cargo build`
  now produces the full launchable set so the supervisor's sibling-binary lookup
  finds the dev `buffr-app` instead of falling through to `$PATH` and silently
  picking up the installed release binary. `cargo run` requires `-b <bin>` since
  the workspace now has multiple bins.
- `hjkl-splash` 0.1 → 0.2 (wall-clock-owned timing; consumers no longer call
  `Splash::advance` per paint).

## [0.5.0] - 2026-05-05

### Added

- **Crash + hang watchdog** (closes #28). Linux/macOS/Windows: a new `buffr`
  supervisor binary spawns the browser as a child and restarts it on crash or
  UI-thread hang. UI thread sends a 1 Hz heartbeat to the supervisor over UDS
  (Linux/macOS) or named pipe (Windows); supervisor kills + restarts after 8 s
  of no heartbeat (with 1.5 s post-connect grace) or non-zero exit. Backoff
  halts at 3 restarts within 30 s. Linux uses `setsid` + `killpg`; macOS bundles
  the supervisor as `CFBundleExecutable` inside `Buffr.app`; Windows uses Job
  Objects with `KILL_ON_JOB_CLOSE` and `SetConsoleCtrlHandler` for
  Ctrl+C/Break/close. The previous `buffr` binary is now `buffr-app`.
- **`buffr-view-source` renderer crate** (rounds 1–2 of #30) — bonsai-rendered
  `buffr-src:` scheme handler scaffolded in a dedicated crate and wired into the
  CEF resource handler. Tokyonight palette + line numbers (round 3) tracked in
  #30.

### Fixed

- **Hang on cross-app paste after copying from Facebook Messenger / other
  Wayland sites** (closes #34). Picked up `hjkl-clipboard` 0.5.3 with the
  self-paste self-pipe deadlock fix: when the buffr process owns the active
  data_source, `do_get` short-circuits to the cached payload instead of going
  through `offer.receive` + `read_fd_to_end`, which would deadlock the bg
  Wayland thread against its own `data_source.send` event.
- **Windows MSVC build** of the heartbeat client: `HANDLE` is `*mut c_void` (not
  `usize`); `GENERIC_WRITE` moved to `Win32::Foundation`; `hTemplateFile` takes
  `null_mut()`, not `0`.
- **Supervisor binary missing from Windows MSI** — CI was building only
  `buffr-app -p buffr-helper`; xtask `collect_windows_payload` requires all
  three exes. Added `-p buffr` to the Windows package step in `ci.yml` and
  `release.yml`; release.yml also dropped the dead `-p buffr-bin` package id
  left over from the pre-rename split.

### Changed

- **Binary layout: `buffr` is now the supervisor entrypoint, `buffr-app` is the
  browser.** PKGBUILD, deb/rpm, MSI, and macOS bundle install both binaries.
  Linux `.desktop` `Exec=buffr` continues to work (now invokes supervisor).
  Windows MSI shortcuts and `Start Menu` entry target `buffr.exe` (supervisor).
  `cargo run` from the workspace root launches the supervisor by default.
- `hjkl-clipboard` 0.4 → 0.5 (drops `Clone` from `Clipboard`; consumers wrap in
  `Arc` for cross-thread share). Bumped to 0.5.3 with the self-paste fix.
- Supervisor integration tests renamed `buffr-supervisor` → `buffr` to match the
  new binary name.
- CI gates `main` on macOS + Windows package jobs; `continue-on-error: true`
  removed from both, so Windows/macOS regressions now turn the pipeline red.

## [0.4.0] - 2026-05-05

### Added

- **Right-click context menu** (#23). Custom-rendered overlay
  (`buffr-ui::ContextMenuOverlay`) replaces Chromium's native menu while still
  driven by CEF's `ContextMenuHandler`. Items are bucketed by hit-test (page /
  link / image / media / editable / selection), with full action wiring
  including:
  - **Frame edit ops**: undo, redo, cut, copy, paste, paste-plain, delete,
    select-all — dispatched per-frame via CEF's `cef_frame_t` edit commands.
  - **Image ops**: copy URL, save image (off-thread fetch + PNG transcode for
    clipboard), open image in new tab, rotate clockwise / counter-clockwise.
  - **Media ops** (video / audio): play/pause, mute, loop, controls,
    picture-in-picture — JS injection via `document.elementFromPoint` with
    `querySelector` fallback for sites where the click target is a sibling (not
    ancestor) of the `<video>` element. Picture-in-picture currently no-ops on
    YouTube due to a transient-user-activation gap (tracked in #31).
  - **Page ops**: view source (`buffr-src:` prefix), reload, hard-reload, print,
    inspect element with hit-point.
  - **Tab strip click** now closes the omnibar overlay (parity with `gt`/`gT`
    once those gain non-Normal-mode bindings).
- **`buffr-src:` URL prefix** as the user-facing alias for Chromium's
  `view-source:`. Navigations to `buffr-src:<url>` rewrite to
  `view-source:<url>` at the CEF boundary; the omnibar / tab strip / session
  file see the buffr-flavored prefix uniformly.

### Fixed

- **Transparent OSR background on pages without a CSS body bg.**
  `BrowserSettings.background_color` was unset (= 0), which CEF treats as
  fully-transparent painting for windowless browsers; pages like `buffr-src:` /
  `view-source:` showed the wgpu clear colour through the OSR quad. Now opaque
  white, matching standard browser behaviour.
- **DevTools windows** now close on shutdown so the host process exits cleanly
  instead of hanging on the DevTools child window (#27).

### Changed

- `buffr-core` 0.4.0 → 0.5.0; `buffr-ui` 0.1.2 → 0.2.0.

## [0.3.0] - 2026-05-04

### Added

- **OSR sleep** when the buffr window is occluded. CEF's paint scheduler pauses
  via `was_hidden(true)` and the wgpu present pipeline short-circuits,
  eliminating the CPU/GPU spin on hidden workspaces. Driven by
  `WindowEvent::Occluded` plus a `present_us` heuristic that trips after 1
  frame > 500 ms or 3-of-5 frames > 100 ms (covers Hyprland and other
  compositors that don't fire `Occluded` on workspace switch).
- **Idle-inhibit** scaffold for issue #22 — keeps the screen awake while a video
  plays in the focused window. Trait + 4 platform backends:
  - Linux Wayland: `zwp_idle_inhibit_manager_v1`
  - Linux X11: `org.freedesktop.ScreenSaver` D-Bus
  - macOS: `IOPMAssertionCreateWithName` (`NoDisplaySleepAssertion`)
  - Windows: `SetThreadExecutionState(ES_DISPLAY_REQUIRED)` Config section
    `[idle_inhibit]` with `enabled`, `inhibit_audio_only`, `require_focus`.
    Currently inert pending the JS→Rust read-back path for
    `__buffr_video_active`.
- **JS media-activity probe** with five signal sources via the
  patched-constructor pattern:
  1. `navigator.mediaSession.playbackState === 'playing'`
  2. fullscreen `<video>`
  3. silent / muted `<video>` or `<audio>` via patched
     `HTMLMediaElement.prototype.play`
  4. WebRTC: any `RTCPeerConnection` with non-closed `connectionState`
  5. Screen Wake Lock: any un-released `WakeLockSentinel` Init script injected
     once at `on_load_end`; poll script writes `window.__buffr_media_active` and
     `window.__buffr_video_active` each ~2 s tick.
- **Audio detection** via CEF `AudioHandler` — `BrowserHost` exposes
  `any_audio_active()` and `drain_audio_events()` for embedder policies.
- Debug builds use `buffr-debug` as the in-app `APPLICATION` constant (via
  `cfg(debug_assertions)` in `buffr-config`), so `cargo run` and release
  installs no longer share `~/.cache/buffr/` and `~/.local/share/buffr/`.

### Changed

- **Render thread architecture: all wgpu mutating calls run on a dedicated
  `wgpu-render` worker thread.** The UI thread now does only
  `surface.get_current_texture()`, the chrome paint closure (CPU only), and an
  OSR pixel memcpy before sending a `RenderCommand` over a capacity-1 mailbox
  channel. `queue.write_texture`, `queue.submit`, and
  `surface_texture.present()` all happen on the worker. Fixes the multi-second
  UI freeze on Hyprland workspace switches (compositor backpressure used to
  block the UI thread inside `present()`).
- `buffr-core` dep bumped from `"0.3"` to `"0.4"`; `buffr-config` from `"0.2"`
  to `"0.3"`.

### Fixed

- **Ctrl+C now responsive** while the window is occluded — shutdown is
  dispatched via `EventLoopProxy::send_event(BuffrUserEvent::Shutdown)` from the
  `ctrlc` handler, so winit wakes immediately instead of waiting for compositor
  activity.
- **No more `wgpu` panics** on shutdown or resize during occlusion:
  - `frames_in_flight` counter gates `surface.get_current_texture()` — only one
    outstanding acquired SurfaceTexture at a time
    (`desired_maximum_frame_latency = 1`).
  - `Renderer::drop` wraps `surface` and `device` in `ManuallyDrop`; when the
    worker is mid-`present()`, the wgpu state is leaked (process is exiting)
    instead of triggering "Surface cannot be destroyed because is still in use".
  - `resize()` defers `surface.configure()` into a `pending_resize` slot when
    the worker holds an outstanding SurfaceTexture; the next `frame()` applies
    it once the worker drains.
- **`idle_inhibitor` drops before `window`** in `AppState` so the Wayland
  backend's worker doesn't reference a freed `wl_display` during shutdown.
- **Idle-inhibit Drop** uses `recv_timeout(100ms)` instead of an unconditional
  `thread::sleep(100ms)` — shutdown returns the moment the worker exits cleanly.
- **`RTCPeerConnection` patched-constructor** in `media_probe_init.js` aliases
  the `.prototype` property directly so `pc instanceof RTCPeerConnection` keeps
  working on the page.

## [0.2.1] - 2026-05-03

### Changed

- Dropped `publish-stub` job from release workflow. `buffr` on crates.io stays
  at 0.1.28 as a permanent pointer to GitHub releases; no need to bump it on
  every umbrella tag.

## [0.2.0] - 2026-05-03

### Added

- `buffr --help` now renders an ASCII-art banner (figlet "ANSI Regular" font)
  with the package version inline. Banner lives in `apps/buffr/src/art.txt`,
  embedded via `include_str!`. Regenerate with
  `figlet -f "ANSI Regular" buffr > apps/buffr/src/art.txt`.
- CLI smoke tests: `--version` returns `CARGO_PKG_VERSION`, long-form help
  contains the embedded art block and the version string.

### Changed

- **XDG-everywhere paths via `hjkl-config` 0.2 (breaking on macOS/Windows).**
  Bumps `buffr-config` 0.1.1 → 0.2.0 and `buffr-core` 0.2.0 → 0.3.0. Eliminates
  the macOS/Windows split-brain where `buffr-config` already wrote to
  `~/.config/buffr/config.toml` while `buffr-core` resolved cache + data via
  `directories::ProjectDirs` (`sh.kryptic.buffr` Bundle ID). All buffr dirs now
  honor `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` / `$XDG_CACHE_HOME` on every
  platform; Linux paths unchanged. macOS users move from
  `~/Library/Application Support/buffr/` + `~/Library/Caches/sh.kryptic.buffr/`
  to `~/.config/buffr/` + `~/.cache/buffr/`. Windows users move from
  `%APPDATA%\buffr\` + `%LOCALAPPDATA%\kryptic\buffr\cache\` to
  `~/.config/buffr/` + `~/.cache/buffr/`. See `crates/buffr-config/CHANGELOG.md`
  and `crates/buffr-core/CHANGELOG.md` for per-crate detail.

## [0.1.28] - 2026-05-03

### Added

- **Loading animation across all main-frame navigations.** Plays the buffr ASCII
  anim during reload + every navigation gap until the first contentful CEF frame
  commits, driven by a new `BrowserHost::is_loading()` flag set on
  `LoadHandler::on_load_start` and cleared by `OsrPaintHandler::on_paint`.

### Fixed

- **Ctrl+V hang in CEF input fields.** Reading the system clipboard on the main
  thread self-deadlocked: `hjkl-clipboard`'s Wayland `offer.receive` blocks on a
  pipe whose `wl_data_source.send` callback runs on CEF's UI thread (= main
  thread). Ctrl+V intercept now reads on a worker thread and posts the result
  back via `EventLoopProxy` as a `ClipboardPasteText` user event, then injects
  via `execCommand('insertText', ...)`.
- **Letterbox / "two sizes behind" CEF paint after rapid resize.** OSR frames
  now carry a `needs_fresh` flag, set by `osr_resize` and cleared only on
  successful main-frame paint. The freshness gate now requires a post-resize
  paint before re-presenting the OSR buffer, preventing persisted stale dims
  from sticking after a resize burst.
- **Stale-size swapchain texture acquired during rapid resize.** `render.rs`
  drops textures whose dims don't match the current surface, reconfigures, and
  retries up to 2× before skipping the frame.
- **Image clipboard paste no longer spams "MIME type not supported".** Ctrl+V
  intercept falls through to CEF when the clipboard isn't text. Image paste is
  still unsupported in OSR (tracked in #19).

### Changed

- `hjkl-clipboard` 0.3 → 0.4 (Wayland data-control protocol, multi-MIME reads).
  Subsequent bump to 0.4.8 picks up Wayland thread respawn fix.
- Workspace `Cargo.toml` routes all `buffr-*` path overrides through
  `[patch.crates-io]` so consumers without the submodule init get crates.io
  versions automatically — matches the pattern used in hjkl.
- `apps/buffr` no longer depends on `hjkl-clipboard` directly; clipboard reads
  go through the new opaque `buffr_core::ClipboardReader::read_text()`.

## [0.1.27] - 2026-05-03

### Fixed

- Surface-size drift on rapid resize self-heals: the wgpu surface now
  reconfigures when its cached dims diverge from the window's current size.
- `single_instance` test module compiles clean on Windows (`dead_code` allow for
  `socket_path`) and Linux clippy.

## [0.1.26] - 2026-05-03

### Added

- **Single-instance launching with URL forwarding.** A second `buffr <url>`
  invocation forwards the URL to the existing process via Unix socket / Windows
  named pipe and exits. The running window opens the URL in a new tab. Disabled
  by passing `--no-single-instance`.
- **Per-monitor HiDPI scaling.** Live device-scale plumbing: scale changes (drag
  between displays, fractional scaling toggle) propagate to CEF via
  `RenderHandler::screen_info` without restart.
- **ASCII-only loading frames** rendered when the CEF buffer is unusable
  (initial paint, between navigations, surface-drift recovery).
- Bilinear favicon scaling in the tab strip (replaces nearest-neighbour).

### Fixed

- **Resize flicker / stale CEF paints rejected.** Generation-based OSR freshness
  gate; CEF paints at stale dims are dropped instead of presented. Watchdog
  forces a repaint when CEF skips a post-resize paint.
- Resize debounce recomputes target dims at flush so terminal sizes settle to
  the most recent window size, not an intermediate one.
- `resync_cef_rect` now routes through `osr_resize` so the freshness gate fires
  on programmatic resizes (not just user drags).
- Forced chrome repaint when the loading animation deactivates so the modeline /
  tab strip doesn't render against a stale frame.
- Unified `.desktop` file under `pkg/buffr.desktop` (was duplicated).

### Changed

- `/vendor` tree gitignored entirely.

## [0.1.25] - 2026-05-02

### Added

- ASCII loading animation when the CEF buffer is unusable.

## [0.1.24] - 2026-05-01

### Added

- Use the buffr.kryptic.sh website icon in the desktop entry.

## [0.1.23] - 2026-05-01

### Added

- AUR `buffr-bin` runtime tarball now bundles the `.desktop` entry and icon so
  `pacman -S buffr-bin` produces a launcher in the application menu.

## [0.1.22] - 2026-05-01

### Fixed

- AUR auto-publish: ssh options injected via `GIT_SSH_COMMAND` env var
  (previously dropped by the wrapper).

## [0.1.21] - 2026-05-01

### Fixed

- AUR auto-publish: `accept-new` host-key policy so the first push from a fresh
  runner doesn't fail on missing `known_hosts`.

## [0.1.20] - 2026-05-01

### Fixed

- AUR auto-publish: corrected `su` option ordering so `makepkg` actually runs as
  the build user.

## [0.1.19] - 2026-05-01

### Fixed

- AUR auto-publish: provision build user, ship `LICENSE` + `.gitignore`,
  obfuscate maintainer email in PKGBUILD per AUR convention.

## [0.1.18] - 2026-05-01

### Fixed

- **Linux release binaries find their own libcef.** `RUNPATH=$ORIGIN` baked into
  the binary at link time, so the runtime tarball works without setting
  `LD_LIBRARY_PATH`.

## [0.1.17] - 2026-04-30

### Added

- **AUR auto-publish on release.** `buffr-bin` PKGBUILD bumps and pushes to the
  AUR on every `v*` tag.

## [0.1.16] - 2026-04-30

### Fixed

- Release: extract tarball with `--strip-components` for the Flatpak + Snap
  bundle steps.

## [0.1.15] - 2026-04-30

### Added

- **Flatpak + Snap bundles** ship on every release alongside the existing
  `.deb`, `.rpm`, `.tar.gz`, `.dmg`, `.msi` artifacts.

## [0.1.14] - 2026-04-30

### Removed

- **Intel Mac (`x86_64-apple-darwin`) release leg.** GitHub Actions `macos-13`
  runner pool is heavily contended (1–2 hour queue times blocking the publish
  pipeline). Apple stopped selling Intel Macs in 2023; the cost wasn't paying
  for the user count. Source still builds clean against `x86_64-apple-darwin`,
  the support is just absent from the release pipeline.

## [0.1.13] - 2026-04-30

### Added

- Linux release emits a portable `.tar.gz` of the runtime tree alongside the
  `.deb` / `.rpm` packages.

## [0.1.12] - 2026-04-30

### Added

- **Expanded release matrix:** Linux aarch64, Windows arm64, and (briefly,
  reverted in 0.1.14) Intel Mac binaries.

## [0.1.11] - 2026-04-30

### Added

- Linux release now produces a `.rpm` package (Fedora / RHEL / openSUSE).

## [0.1.10] - 2026-04-30

### Removed

- AppImage build. The `.deb` + `.tar.gz` outputs cover the same ground without
  the AppImage runtime overhead.

## [0.1.9] - 2026-04-30

### Changed

- **Windows now uses OSR rendering**, matching Linux and macOS. Brings
  cross-platform parity for the wgpu compositor + overlay UI; the previous
  Windows-only windowed-CEF path is gone.

## [0.1.8] - 2026-04-30

### Added

- **Linux HiDPI scale forwarded to CEF** via env vars at child-process spawn
  (`GDK_SCALE`, etc), so fractional scaling on Wayland renders pages at the
  right DPR instead of CEF's default 1.0.
- Auto-publish the `buffr` crates.io stub from the release pipeline once all
  platform artifacts upload.

### Changed

- `apps/buffr-stub/target` no longer tracked in the build artifact gitignore.

## [0.1.7] - 2026-04-30

### Fixed

- **macOS DMG preserves the executable bit** on `Contents/MacOS/buffr`.
  Previously dropped by the staging step, leaving the `.app` non-runnable.

## [0.1.6] - 2026-04-30

### Fixed

- Windows MSI: suppress ICE64 alongside ICE38/ICE91 in `light.exe` for the
  per-user install.

## [0.1.5] - 2026-04-30

### Fixed

- Windows MSI: suppress ICE38/ICE91 in `light.exe` so the per-user install
  validates.

## [0.1.4] - 2026-04-30

### Fixed

- **Windows MSI bundles the full CEF runtime** (libcef, paks, locales) via
  `heat.exe`. Previously the MSI was missing libcef and refused to launch.

## [0.1.3] - 2026-04-30

### Fixed

- **Windows MSI is now per-user** (no UAC prompt) — `cargo install`-style
  no-admin install on Windows.

## [0.1.2] - 2026-04-30

### Added

- **Per-tab favicons** rendered in the tab strip via CEF `download_image`.
- **Native cursor forwarding.** CEF cursor changes (text, pointer, resize, …)
  now propagate to winit on the main window and popup windows.
- **Live find highlight** with 300ms debounce — search results highlight as you
  type, not just on enter.
- **Per-mode chrome theming** via HSL hue rotation off a single accent colour
  (`--accent`). `[theme] accent = "#7aa2f7"` drives modeline, selection, hint
  chips, and prompt cursor.
- `[general] show_favicons` config toggle (default `on`).

### Fixed

- **Hint mode keeps matched hints bright** while dimming non-matches; the typed
  prefix is struck through on the matching hint chip.
- Favicon list iterates raw `cef_string_list_t` instead of cloning (clone
  produced an empty list on the cef-rs 147 binding).
- macOS CEF init uses `external_message_pump` and a dev keychain workaround for
  codesigning identity discovery.

### Changed

- **Workspace crates extracted into standalone submodule repos** (matches the
  hjkl pattern). `[patch.crates-io]` overrides used for local dev; fresh
  checkouts without submodule init resolve against crates.io.
- Buffr is now an install-instructions stub on crates.io (`apps/buffr-stub`);
  the real binary at `apps/buffr` was renamed to `buffr-bin` to dodge the
  package-name collision. The produced binary is still `target/<profile>/buffr`.
- hjkl-\* deps caret-pinned (no longer exact-version pinned).
- Submodule checkouts in CI are now recursive.

## [0.1.1] - 2026-04-29

### Added

- **Ctrl+V paste** in the omnibar / command line / find overlay (clipboard text
  pushed into the input buffer, CR/LF stripped) and in CEF-focused page inputs
  (JS-injected via `document.execCommand('insertText', ...)`). Needed because
  CEF on Wayland can't read the system clipboard itself even when the keystroke
  reaches it.

### Fixed

- **Clipboard reads on Wayland.** `clipboard_text()` now falls back to
  `wl-paste -n` when arboard returns empty under `WAYLAND_DISPLAY` — symmetric
  to the existing `wl-copy` write fallback. Same root cause: arboard's
  wl-data-source ownership lives in a worker thread that doesn't reliably serve
  other clients.

### CI

- Tag-driven `release.yml` builds Linux AppImage + .deb, macOS .dmg, and Windows
  .msi on `v*` tag pushes and uploads them to the GitHub Release with sha256
  sidecars. crates.io publishing intentionally manual (11 workspace crates would
  hit the new-crate rate limit).
- `ci.yml` now also accepts `workflow_dispatch:` for manual re-runs.

## [0.1.0] - 2026-04-29

First tagged release. Multi-tab browsing with OAuth-capable popups, modal vim
keybindings, GPU-accelerated chrome compositor, and per-origin data layers
(history / bookmarks / downloads / permissions / zoom) all wired and persisted.

### Added

- **Popup windows.** `window.open(...)` and other `NEW_POPUP` / `NEW_WINDOW`
  dispositions now render in a dedicated buffr winit window with a read-only
  address-bar strip at top (no tab strip, no statusline). Preserves CEF's native
  `window.opener` reference so OAuth flows that `postMessage` back to the opener
  work end-to-end. Multiple concurrent popups supported, each with its own
  browser, history, and lifecycle. JS-driven `window.close()` and opener-driven
  `popup.close()` shut the popup window down cleanly.
- **Two-finger horizontal swipe → back / forward.** Touchpad PixelDelta events
  accumulate horizontally; once the gesture crosses 150 px while staying ≥ 2×
  more horizontal than vertical, fire HistoryBack (swipe right) or
  HistoryForward (swipe left). Works in the main window and in popup windows;
  popups navigate their own browser history.
- **`target="_blank"` and Ctrl+click open in new tabs.** Disposition-aware
  `LifeSpanHandler::on_before_popup` plus a new
  `RequestHandler::on_open_urlfrom_tab` route `NEW_FOREGROUND_TAB` /
  `NEW_BACKGROUND_TAB` through our tab queue while leaving popup dispositions to
  CEF's native handling.

### Fixed

- **Wayland top-edge resize artifacts.** Eliminated black bars / bottom-bar gap
  during interactive top-edge drags on Hyprland. CEF is notified on every winit
  `Resized` event (no debounce); the renderer GPU-stretches whatever frame CEF
  most recently emitted to fill the live browser_rect.
- **Popup focus on click.** Wayland doesn't reliably emit `WindowEvent::Focused`
  on click, so we explicitly call `set_focus(true)` on the popup's CEF browser
  when a press lands inside the OSR content area, ensuring DOM caret state and
  keyboard input route correctly.
- **Popup scroll speed.** Popup wheel handler now uses the same
  `winit_wheel_to_cef_delta` helper as the main window (10× scale on PixelDelta)
  so touchpad scrolling feels identical across windows.

### Changed

- **Resize pipeline simplified.** Dropped ~145 LOC of debounce / throttle /
  double-slot logic. Single OSR texture, GPU-stretched on dim mismatch, CEF told
  the size on every Resized event.

### Documentation

- Workspace READMEs polished to match the hjkl reference style: per-crate
  badges, public-API tables, architecture overviews. New READMEs for
  `apps/buffr`, `apps/buffr-helper`, `buffr-config`, `buffr-core`,
  `buffr-modal`, and `buffr-ui`.

### Changed (workspace deps)

- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.25` to
  `=0.0.26`. Pulls in hjkl Phase 5 trait extraction (`spec::*` re-exports,
  optional `ratatui` on `hjkl-engine`, new ratatui-free Editor methods). Buffr
  does not yet depend on `hjkl-editor` and uses no `Rect`-flavoured APIs, so
  this is a transparent pin bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.26` to
  `=0.0.28` — adopts canonical Buffer impl (0.0.27) plus sticky_col + iskeyword
  hoist (0.0.28). Buffr only uses editor-level accessors, so the
  `hjkl_buffer::Buffer` API breaking change in 0.0.28 is transparent here.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.28` to
  `=0.0.29` — picks up Patch B, which wires the `Host` trait through `Editor`.
  The Host surface itself is unchanged and `BuffrHost` already implements all 10
  SPEC methods; the back-compat `Editor::new` shim wraps `DefaultHost`, so no
  Buffr source changes are required. Migration to
  `Editor::with_host(km, BuffrHost::new())` is left for a follow-up.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.29` to
  `=0.0.30` — picks up Patch C-α, which relocates the motion vocabulary out of
  `hjkl_buffer::Buffer` inherent methods into the `hjkl_engine::motions` module.
  Buffr only consumes editor-level APIs, so the consumer-side change is a pin
  bump only — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.31` to
  `=0.0.32` — picks up Patch C-β (partial): deprecated aliases dropped,
  `_xy`/`_xywh` asymmetries resolved (`mouse_click_in_rect`,
  `mouse_extend_drag_in_rect`, `cursor_screen_pos_in_rect`,
  `install_ratatui_syntax_spans`, `intern_ratatui_style`), and the additive
  `FoldProvider` trait shipped. Buffr has no call sites against the renamed or
  removed symbols, so this is a transparent pin bump — no source changes
  required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.32` to
  `=0.0.33` — picks up Patch C-γ (partial). Buffr has no source migration to
  perform, so this is a transparent pin bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.33` to
  `=0.0.34` — picks up Patch C-δ.1, which relocates `Viewport` ownership from
  `hjkl_buffer::Buffer` onto `hjkl_engine::Host`. `BuffrHost` now carries a
  `viewport: Viewport` field and implements the new `Host::viewport` /
  `Host::viewport_mut` accessors. A `set_viewport_size(width, height)` helper is
  exposed for the eventual resize wiring; until edit-mode is plumbed into the
  CEF/winit page lifecycle in `apps/buffr`, the viewport stays at zero-size and
  the engine's scroll math no-ops. No `buffer().viewport*()` reaches in buffr,
  so the migration is contained to `BuffrHost`.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.34` to
  `=0.0.35` — picks up the search FSM migration from `hjkl_buffer::Buffer` onto
  `hjkl_engine::Editor`. Buffr does not drive search through the Buffer API per
  the consumer audit, so this is a transparent pin bump — no source changes
  required. First of a 5-patch path toward hjkl 0.1.0.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.35` to
  `=0.0.36` — picks up the named-marks consolidation, relocating mark storage
  and operations from `hjkl_buffer::Buffer` onto `hjkl_engine::Editor`. Buffr
  does not interact with the marks API directly, so this is a transparent pin
  bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.36` to
  `=0.0.37` — relocates `spans` and `search_pattern` out of
  `hjkl_buffer::Buffer` onto `hjkl_engine::BufferView`, which now carries the
  `spans` and `search_pattern` fields. Buffr does not consume these fields
  directly per the consumer audit, so this is a transparent pin bump — no source
  changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.37` to
  `=0.0.38` — introduces the `FoldOp` / `FoldProvider::apply` pipeline on
  `hjkl_engine`, threading fold operations through the editor host. Buffr does
  not implement a fold provider and consumes only editor-level APIs, so this is
  a transparent pin bump — no source changes required.
- Bump `hjkl-engine` and `hjkl-buffer` workspace pins from `=0.0.38` to
  `=0.0.39` — adds `Query::dirty_gen` for cache invalidation on the syntax query
  layer. Buffr consumes only editor-level APIs, so this is a transparent pin
  bump — no source changes required.

[Unreleased]: https://github.com/kryptic-sh/buffr/compare/v0.14.16...HEAD
[0.14.16]: https://github.com/kryptic-sh/buffr/compare/v0.14.15...v0.14.16
[0.14.15]: https://github.com/kryptic-sh/buffr/compare/v0.14.14...v0.14.15
[0.14.14]: https://github.com/kryptic-sh/buffr/compare/v0.14.13...v0.14.14
[0.14.13]: https://github.com/kryptic-sh/buffr/compare/v0.14.12...v0.14.13
[0.14.12]: https://github.com/kryptic-sh/buffr/compare/v0.14.11...v0.14.12
[0.14.11]: https://github.com/kryptic-sh/buffr/compare/v0.14.10...v0.14.11
[0.14.10]: https://github.com/kryptic-sh/buffr/compare/v0.14.9...v0.14.10
[0.12.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.12.0
[0.11.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.11.1
[0.11.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.11.0
[0.10.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.10.0
[0.9.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.9.1
[0.9.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.9.0
[0.8.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.8.1
[0.8.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.8.0
[0.7.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.7.1
[0.7.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.7.0
[0.6.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.3
[0.6.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.2
[0.6.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.1
[0.6.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.6.0
[0.5.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.3
[0.5.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.2
[0.5.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.1
[0.5.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.5.0
[0.4.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.3.0
[0.2.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.2.0
[0.1.28]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.28
[0.1.27]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.27
[0.1.26]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.26
[0.1.25]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.25
[0.1.24]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.24
[0.1.23]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.23
[0.1.22]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.22
[0.1.21]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.21
[0.1.20]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.20
[0.1.19]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.19
[0.1.18]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.18
[0.1.17]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.17
[0.1.16]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.16
[0.1.15]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.15
[0.1.14]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.14
[0.1.13]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.13
[0.1.12]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.12
[0.1.11]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.11
[0.1.10]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.10
[0.1.9]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.9
[0.1.8]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.8
[0.1.7]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.7
[0.1.6]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.6
[0.1.5]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.5
[0.1.4]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.4
[0.1.3]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.3
[0.1.2]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/buffr/releases/tag/v0.1.0
