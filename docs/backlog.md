# Backlog

Open items from the full-codebase reviews, plus what surfaced while fixing them.
Nothing here is a known-broken **build**: `main` is green on CI (all three
OSes), `cargo deny`, `cargo machete`, and the fuzz workflow. These are the items
that were deliberately **not** actioned, and why.

Grouped by what is actually blocking them.

---

## 1. Needs a product decision

Each of these has a real defect behind it, but two or more defensible
resolutions. None was touched, so the current behaviour is whatever is
described.

### E2 — closed shadow roots are invisible to edit mode

**Where:** `crates/buffr-core/assets/edit.js` (`deepTarget`);
`crates/buffr-cef/src/handlers.rs`, `on_load_end`.

Focusing a text field inside `attachShadow({mode: 'closed'})` does not enter
Insert mode. Open, nested and `delegatesFocus` roots all work — `focusin`
retargets to the host and `composedPath()[0]` recovers the real node — but a
closed root is opaque by design: `composedPath()` stops at the host and
`element.shadowRoot` is `null` from outside. Chromium's own autofill has the
same blind spot.

Covered by `tests/e2e/pages/shadow_closed.html`, which currently asserts
`normal` so the gap is pinned rather than silently tolerated. Flip the
expectation to `insert` when this is fixed.

The only real fix is to record roots as they are created, which needs a script
running at **document start**, before the page's own scripts:

1. **Renderer-process handler.** Implement `CefRenderProcessHandler` in
   `buffr-helper` and hook `OnContextCreated` to patch
   `Element.prototype.attachShadow`, keeping a side table of closed roots.
   Correct and complete, but it is a new process-boundary component: the helper
   currently has no CEF handler at all, and the side table has to reach the
   browser process.
2. **Leave it.** Closed roots are rare outside deliberately encapsulated
   widgets, and a component that closes its root has opted out of exactly this
   kind of introspection.

Doing nothing is defensible; what is not defensible is claiming shadow DOM
support without saying which kind.

### E1 — edit mode does not work inside iframes

**Where:** `crates/buffr-cef/src/handlers.rs`, `on_load_end`;
`crates/buffr-core/assets/edit.js`.

Clicking a text field inside any `iframe` never enters Insert mode. Not a
regression — `on_load_end` returns early for subframes and the comment there
states the intent plainly: iframes and subframes never get the listener, or the
nonce. That is what stops a third-party frame forging edit events for the top
frame (notably `{"type":"selection"}`, which feeds yank-to-clipboard).

The cost is real: embedded comment boxes, checkout and payment frames, search
widgets and any cross-origin login form are unusable in Insert mode. The user
sees "clicking this input does nothing".

Reproduced by `tests/e2e/pages/iframe_same_origin.html` and
`iframe_srcdoc.html`, both deliberately excluded from the e2e expectations until
this is decided.

The options, cheapest first:

1. **Leave it.** Document the limitation. Iframe text entry stays broken.
2. **Per-frame nonce.** Inject into every frame, each with its own nonce, and
   have Rust track which frame a nonce belongs to. A frame can then forge only
   events attributed to itself — so a hostile iframe can drive Insert mode for
   its own fields (which it already controls) but cannot impersonate the top
   frame or reach `selection`. Most work; closest to correct.
3. **Same-origin subframes only.** Inject where the frame's origin matches the
   top frame's. Cheap, and covers same-site embeds, but misses exactly the
   cross-origin cases that matter most (payments, third-party login).
4. **Main frame plus a `selection`-free nonce for subframes.** Subframes get a
   nonce that Rust accepts for `focus`/`blur`/`mutate` but never for
   `selection`. Narrower blast radius than 2, less bookkeeping.

Whichever way this goes, the two iframe pages should move into the e2e
expectations so the behaviour is pinned rather than assumed.

### W8 (webkit, remainder) — external schemes still launch `xdg-open` without a confirmation prompt

**Where:** `crates/buffr-webkit/src/platform/runtime.rs`, `on_decide_policy`.

The 2026-08-04 pass gesture-gated the launch (a scripted `location='foo://…'` no
longer pops a handler) and reaps the child, but the confirmation prompt Chromium
shows for external protocols is still absent. Doing it properly needs an
apps-layer prompt channel the webkit backend does not have. `needs-decision` on
whether to build that or accept the gesture-gate as sufficient.

---

## 2. Verification gaps

Things that are implemented and passing, but whose _runtime_ behaviour nobody
has observed.

- **No `cargo test` on Windows or macOS.** CI runs tests only on
  `ubuntu-latest`; the other two OSes get clippy and the smoke test. So every
  `#[cfg(windows)]` path is cross-compiled and linted but **never executed** —
  including this review's supervisor fixes (`STILL_ACTIVE` misread, non-zero
  exit handling, argument quoting, the environment block) and the rewritten
  `restart_on_crash_windows.rs`. Adding a Windows test job is the single
  highest-value CI change left.
- **`buffr-webkit` is still not built by CI.** It is excluded from the workspace
  and has no job, which is exactly how it bit-rotted into the W-series findings.
  It builds and passes clippy/tests locally now (needs `wpewebkit-2.0`), so a
  Linux job would keep it honest.
- **The 2026-08-04 webkit edits are inspection-verified only.** The W2/W8 fixes
  and the L19 removals in `buffr-webkit` were reviewed against the upstream
  headers and by grep, but the crate cannot compile in this environment
  (`wpewebkit-2.0` absent) and is not built by CI — the first real compile is
  still owed.
- **The 2026-08-04 render-path fixes (P2/P3/P6/N5) were smoke-tested headless on
  2026-08-13** under sway + llvmpipe — and the smoke caught a real bug: the
  scratch-recycling change (`e924f92`) never cleared the reused chrome buffer
  (`Vec::resize` is a no-op at unchanged length), so the loading animation's
  opaque browser-region fill occluded the page forever. Fixed in `267dd9a`
  (`take_cleared_chrome_buffer` clears before resizing; regression-tested). The
  banded chrome upload and acquire-before-paint were exercised and are fine; the
  startup `vkAcquireNextImageKHR` fence-in-use validation noise seen under
  llvmpipe is benign (renders correctly) and predates the perf pass.
- **`buffr-src:` allow-list (M13) is untested at runtime.** The scheme dropped
  `CORS_ENABLED | FETCH_ENABLED` and gained a scheme/host check. "View page
  source" needs a manual pass, including on a `buffr://` internal page.
- **Popup and HiDPI paths.** The popup logical/physical fix (M31) and the
  context-menu and pinned-close hit-test fixes (M30) are unit-tested at scale 1
  and 2 but never clicked at a real HiDPI scale.
- **CEF-originated context-menu coordinates.** `to_overlay`'s own comment says
  CEF sends doubled values at 2×. buffr's synthesized tab menu now converts to
  DIP; CEF's `request.x/y` are still used raw. Worth checking on a HiDPI display
  — outside M30's scope, so it was left alone.
- **`edit.js` rewire on soft navigation.** The teardown hook added for the H5
  nonce assumes CEF re-fires main-frame `on_load_end` for the same document.
  Worth exercising a heavy SPA.
- **The C1–C5 fixes are unit-tested, not exercised.** Each landed with a pure
  test that was proven to go red, but three of them have branches no test
  reaches, and the tests cannot reach them:
  - **C3's `<Esc>`-leaves-Insert half.** The keymap half (rejecting
    `[keymap.insert]`) is covered; the event-loop wiring that routes `<Esc>`
    back to the mode Insert was entered from is not. Needs a running browser
    parked in Insert with nothing focused.
  - **C4's withdrawal paths.** `sync_permissions_prompt`'s
    replace-a-withdrawn-prompt branch and `resolve_permission`'s
    `ResolveTarget::Stale` branch are both driven by the engine cancelling a
    prompt when its tab navigates away — a race no unit test stages.
  - **C5's call sites.** The geometry helper `popup_cef_rect_pure` is tested at
    1× and 2×, but a test of a pure function cannot observe a **call site**:
    reverting any of the three back to the full window height leaves every test
    green. What prevents that regression is structural — one helper feeding the
    paint rect and both `popup_resize` calls — not any assertion. Verified by
    mutating both: the helper goes red, a call site does not.

---

## 3. Known-accepted limitations

Not bugs to fix — decisions with a stated rationale. Listed so they are not
rediscovered as findings.

- **The console-IPC nonce is not a boundary against the top frame.** The
  injected script runs in the page and injection happens at `on_load_end`, so a
  page that hooks `console.log` first can read the nonce and forge for itself.
  What it closes is cross-frame forgery and cross-load replay. The complete fix
  is a real `cef_process_message_t` channel. Documented in
  [`hint-mode.md`](./hint-mode.md).
- **Anchoring the sentinel parse is an availability trade.** A page that wraps
  `console.log` to prepend a format string now hides our payload too, so hint
  and edit mode stop working there. Accepted: on such a page the nonce is
  readable anyway.
- **`buffr-webkit`'s page nonce is per-tab, not per-load.** WebKit's UCM scripts
  are declarative and re-run with whatever nonce was baked in when they were
  added; rotating per load would mean tearing down every bridge mid-navigation.
  Cross-frame forgery is closed; replay confinement is weaker than CEF's. The
  hint nonce does rotate per session.
- **Nonce-bearing WebKit injections are top-frame only**, so edit-mode events no
  longer fire for inputs inside iframes. Commented at the injection site.
- **Three `deny.toml` advisories stay ignored** pending upstream: two for
  `quick-xml` (via `wayland-scanner` and `plist`) and one for `ttf-parser` (via
  `fontdb`/`fontdue`). Re-checked against the current lock; drop conditions are
  recorded in the file.
- **`docs/backlog.md` is deliberately not in `SUMMARY.md`.** The book is the
  public user-facing site; listing it would publish unfixed security findings
  with reproduction steps.

---

## 4. Release follow-ups

- **The `known_hosts` pin was exercised and held on `v0.14.11`** (2026-08-13):
  the brew-tap and scoop-bucket publish jobs ran against the per-job pins and
  both succeeded. The AUR publish also landed `0.14.11-1` — the ssh fix that the
  `0.14.6-1` hang blocked is proven end-to-end.

- **The sibling repos are fixed but unexercised.** `gpur`, `hjkl`, `hodl`,
  `hrdr`, `inbx`, `krypt`, `pikr` and `sqeel` all carried the same unexpanded
  `~` under `accept-new`, so none of them had real host verification either.
  Each now pins the host keys and uses `StrictHostKeyChecking=yes` with absolute
  `$HOME` paths. Their publish jobs are tag-gated too, so the change is unproven
  until each cuts its next release — worth watching the first one.

---

## 5. `main.rs` decomposition — partly done

`apps/buffr-app/src/main.rs` was 11,628 lines. Seven slices took it to 6,994,
each a separate commit verified with `cargo fmt --all`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` (1072
passed, 0 failed throughout) and a diff against the previous commit proving the
move was pure. CI is green on the result (`e374b37`). The C1–C5 fixes have since
grown it back to 7,301, which is the number to beat. The 2026-08-04 perf pass
grew it further with the paint-path and tick changes; the extraction target
still stands.

Extracted so far: `cli` (the clap `Cli`, dispatch, and every `run_*`
subcommand), `cef_translate`, `chrome_paint`, `paint_policy`, `event_loop`,
`context_menu`.

**Still in `main.rs`:** `AppState` and its main `impl` (~4,000 lines), plus
`mod tests` (1,556). The remaining method groups are _interleaved_ in source
order rather than contiguous, so each needs a commit that gathers scattered
methods instead of cutting a range. The natural groups:

| Group                 | Representative methods                                                         |
| --------------------- | ------------------------------------------------------------------------------ |
| Overlay / omnibar     | `open_omnibar`, `refresh_overlay_suggestions`, `dispatch_command`, `apply_set` |
| Hint mode             | `hint_mode_handle_key`, `handle_hint_action`, `exit_hint_mode`                 |
| Edit mode             | `drain_edit_focus_events`, `expire_pending_blur`, `edit_mode_handle_key`       |
| Permissions / confirm | `sync_permissions_prompt`, `resolve_permission`, `confirm_handle_key`          |
| Popup windows         | `paint_popup_window`, `handle_popup_window_event`                              |
| Chrome painting       | `paint_chrome_with`, `paint_chrome_inner`, `resync_cef_rect`                   |

- **Two modules use a `crate::*` glob** rather than explicit imports:
  `event_loop` and `context_menu`. Deliberate and noted in both module docs —
  they reach most of the crate root while `AppState` still lives there. **Narrow
  both once `AppState` moves out**; leaving the globs is how a module boundary
  quietly stops meaning anything.
- **`mod tests` should follow its subjects.** It still tests functions that now
  live in `paint_policy`, `cef_translate` and `chrome_paint`, reaching them
  through `use super::*`. Splitting each module's tests into that module is
  mechanical but was deliberately deferred so the moves stayed reviewable.
- **`wayr_key_to_planned` keeps a stale prefix.** The `wayr_`/`_to_winit`
  prefixes were dropped from the `cef_translate` functions (both backends now
  sit behind `windowing`), but this one is a method on `AppState` and was out of
  that slice's scope. Rename when the edit-mode group moves.

---

## 6. Found while cleaning up, not actioned

- **`--private` profile directories are never reaped.** Each
  `buffr-app --private` launch creates `$TMPDIR/buffr-private-<pid>-<rand>/` and
  leaves it behind on exit; 49 had accumulated from three e2e suite runs alone.
  The `TempDir` returned by `resolve_paths` is supposed to own that lifetime, so
  either it is being leaked (`std::mem::forget`, or an early `_exit` that skips
  destructors — note `main` calls `libc::_exit(0)` on unix) or the drop happens
  on a path the shutdown never takes. **User-visible:** someone who browses
  private daily silently fills `/tmp`. Not a test artefact — worth confirming
  against a normal `--private` run before deciding the fix.
- **Bare `cargo test` under-reports this workspace: 244 tests vs 1072 for
  `cargo test --workspace`.** A green bare run covers less than a quarter of the
  suite. Everything in this repo should use `--workspace`, matching the rule
  already in `AGENTS.md`.
- **`cargo clippy` without `--all-targets` misses the test target.** Removing an
  import used only by `mod tests` leaves the bin target compiling clean while CI
  fails. `--all-targets` catches it (the workspace gate already runs it).
- **Three abandoned branches were archived as tags, not deleted.** All local and
  remote branches except `main` were removed; the three carrying unmerged work
  are preserved as `archive/gpu-compositor-poc`, `archive/wl-subsurface-poc` and
  `archive/wxs-codepage-fix` (pushed; `archive/*` does not match the `v*`
  release trigger). All three predate the `apps/buffr` → `apps/buffr-app` rename
  (`bec1f30`) and need a real rebase, not a cherry-pick.
  `archive/wl-subsurface-poc` is the subsurface work the removed native
  compositing trio referred to.
- **`cef` stays on the 148 line.** 151.3.0 is the latest release, but the
  vendored libcef runtime and `xtask fetch-cef` pin 148.x and wrapper/runtime
  must agree — bumping means a coordinated runtime upgrade, not a routine dep
  bump. Revisit when the runtime is fetched at 151.

---

## 8. Working practice, learned the hard way

Not repo defects. Recorded because each one cost real time or real work, and
none of it is recoverable from `git log`.

- **Never remove a canary edit with a git restore.** `git checkout-index -f --`
  and `git checkout HEAD --` both restore from a source that does **not**
  include a sub-agent's unstaged work, and doing that once destroyed a completed
  `buffr-engine/src/permissions.rs` change — `PromptIdentity`, `ResolveTarget`,
  `resolve_target`, `take_front_matching` and eleven tests — which only survived
  because the agent had kept its own scratchpad copy. The flow that replaced it:
  **stage the agent's output first**, then add the canary, then remove the
  canary by targeted string replacement. An empty `git diff --stat` against the
  index then proves the canary is gone without any restore being involved.
- **rust-analyzer diagnostics went stale repeatedly** during the decomposition,
  reporting freshly-wired functions as "never used" and inventing missing struct
  fields. Every instance was contradicted by
  `cargo clippy --all-targets -- -D warnings`. Trust the compiler, not the
  editor, and re-check rather than acting on the squiggle.
- **A regex that widens struct fields will also hit function parameters.**
  Broadening visibility with `^(    )(\w+: )` during the `paint_policy` slice
  produced roughly forty syntax errors by matching multi-line call signatures.
  Regenerate from the committed copy and edit named blocks after reading them.

---

## 9. Hardening, 2026-08-04

- `popup_close` uses `close_browser(0)` (host.rs:818) — a `beforeunload` handler
  on a popup may stall close, leaking the popup window + sinks until shutdown.
  Manual test owed with a `beforeunload` popup.
- `--private --smoke-test` exits via `libc::_exit` before the `_private_tmp`
  drop, leaking `$TMPDIR/buffr-private-<pid>-*` per smoke run (CI smoke does not
  use `--private`, so no current trigger).

---

## 10. Code review 2026-08-04 (a38fa86) — open findings

### 4 LOW — webkit `move_tab` lands rightward moves one slot short; `MoveTabRight` is a no-op

**Where:** `crates/buffr-webkit/src/platform/runtime.rs:4559-4570`.

`insert_at = if to > from { to - 1 } else { to }` treats `to` as a pre-move
index, but the trait contract (`buffr-engine/src/engine.rs:84`), the CEF backend
(`host.rs:1745-1746`) and every caller use `to` as the final position.
Experimental backend only, and not built by CI.

Repro: tabs [A, B, C], `MoveTabRight` on B → `move_tab(1, 2)`; `remove(1)` → [A,
C]; `insert_at = 2 - 1 = 1` → [A, B, C] (unchanged). Expect [A, C, B].

**Fix:** `tabs.insert(to, entry)` unconditionally (and mirror it in the
`engine_state` branch at `runtime.rs:4569-4570`).

---

## 11. Code review 2026-08-05 (8744e18) — findings

### 3 MEDIUM — right-clicking inside a popup window shows the menu on the main window and acts on the active tab

**Where:** `crates/buffr-cef/src/handlers.rs:1668-1777` (every browser's
right-click lands in the shared `context_menu_sink`, `browser_id` populated),
`crates/buffr-core/src/context_menu.rs:221-223` (browser_id documented "routes
dispatch to the right tab" — never used),
`apps/buffr-app/src/event_loop.rs:1494-1532` (drained from the active engine,
rendered over the main window), and every arm of
`apps/buffr-app/src/context_menu.rs:243ff` dispatches against
`self.active_engine_dyn()`.

```
Repro: right-click on an image inside a popup window
Expect: menu on the popup window; "Copy Image"/"Back" act on the popup's page
Actual: menu renders on the main window; items act on the active tab, with
       coordinates interpreted in main-window space
```

### 5 MEDIUM — `any_video_active` only ever reflects the active tab; background-tab video releases the screen-lock inhibitor

**Where:** `crates/buffr-cef/src/host.rs:1582-1585` (documents any-tab
semantics), `host.rs:2746-2761` (`run_media_probe` injects into the active tab
only), `apps/buffr-app/src/event_loop.rs:1386-1395` (probe runs on the active
engine, and only while occluded), `handlers.rs:1131-1135` (the only writer).

```
Repro: tab A plays a video; switch to blank tab B and occlude the window
Expect: screen stays awake (documented any-tab semantics)
Actual: B's probe emits {video:false}, video_active flips false, the idle
       inhibitor releases and the screen locks while A's video keeps playing
```

### 6 MEDIUM — Windows: heartbeat pipe thread is neither cancellable nor joined; each connect-timeout leaks a thread blocked in `ConnectNamedPipe`

**Where:** `apps/buffr/src/main.rs:1297-1299` (spawn, `JoinHandle` discarded, no
cancel flag), `1658-1680` (blocking `ConnectNamedPipe`, NULL overlapped; only
exits are connect/error), `1329` (`drop(hb_rx)` does not wake it). The Unix side
has the explicit answer this lacks: `hb_cancel` + `accept_deadline` + join
(main.rs:519-528, 565-570, 708-782).

```
Repro: Windows; child fails to connect within connect_grace() (main.rs:1313-1321
       kills + reaps it); restart's healthy child connects to the same pipe name
Expect: timed-out iteration frees its thread + pipe instance; next child connects
Actual: one blocked thread + pipe instance leaks per timeout; the kernel can match
       the next connect to the stale instance, so the healthy child times out,
       is killed and counted as a crash — repeating to the crash limit with no
       browser ever running
```

The per-timeout leak is certain from the code; the starvation depends on named-
pipe instance matching (not runtime-verified — no Windows host; consistent with
§2's note that no Windows test job exists). Windows-only.

### 8 MEDIUM (multi-engine configs only) — session restore applies pin/favicon-prefill to the active engine instead of the engine that opened the tab

**Where:** `apps/buffr-app/src/main.rs:2880-2896` (`host` captured once from the
active engine at 2829; `host.set_pinned(id)` and `host.tabs_summary().last()`
run against it), `2240-2250` (`routed_open_tab_background` opens on
`router.engine_for(url)`), `engine_router.rs:117-122`.

```
Repro: [engines] routes *.example.com → webkit; saved session has a pinned
       example.com background tab
Expect: tab created + pinned on webkit; prefill registered for webkit's browser_id
Actual: the tab opens on webkit but set_pinned and the prefill run against the
       active (cef) engine — a foreign TabId and the wrong browser_id — and the
       tab never appears in the strip (refresh_tab_strip reads the active engine)
```

### 9 LOW-MEDIUM (multi-engine configs only) — session save persists only the active engine's tabs

**Where:** `apps/buffr-app/src/main.rs:2769-2805` (`save_session_now` reads
`host.tabs_summary()` from `self.active_engine_dyn()` only).

```
Repro: 1 tab on cef + 1 tab on webkit, then quit
Expect: both tabs saved and restored
Actual: every save drops the non-active-engine tabs; each restart loses them
```

### Hardening

---

## 12. Audit 2026-08-05 (8744e18) — findings

### 2 MEDIUM — a page can overwrite the system clipboard at any time via a forged edit-bridge `Selection` event (pastejacking)

**Where:** `apps/buffr-app/src/main.rs:4752-4763` (`Selection` →
`clipboard_set_text` unconditionally, drained every tick, no mode or gesture
gate), reachable because the page nonce is page-readable
(`crates/buffr-core/src/edit.rs:175-178`: "a hostile top frame can emit
authentic edit lines"), accepted by `parse_console_event` (edit.rs:198-204),
value capped at 256 KB (edit.rs:184). `window.__buffrEmitSelection` is also a
page-callable global (`crates/buffr-core/assets/edit.js:449-453`).

```
Repro: any hostile page runs console.log('__buffr_edit__:<nonce>:' +
       JSON.stringify({type:'selection', value:'https://evil.example/'}))
       (the nonce is readable by hooking console.log, per §3 known-accepted)
Expect: clipboard writes require a user gesture / Visual-mode yank
Actual: the user's system clipboard is replaced at any time; a later paste lands
       attacker-chosen content in a terminal or form
```

### 3 MEDIUM — page-driven focusin force-enters Insert mode; the documented gesture gate is dead code (keyboard hijack)

**Where:** `apps/buffr-app/src/main.rs:1594-1599` (field doc: "pages can't drag
us into Insert via autofocus or programmatic `.focus()`"), `4684-4717` (any
`Focus` enters Insert; `insert_intent_at` merely cleared at 4701). Grep shows
`insert_intent_at` is written (event_loop.rs:958, main.rs:2549), cleared (4701)
and initialised (2115) — **never read as a gate**.

```
Repro: a page with a hidden <input> calls el.focus() (or autofocus) on load
Expect: keystrokes keep going to the keymap (per the field's doc)
Actual: Insert mode engages; every vim keystroke (:, /, o, y, d…) goes into the
       page's field — keystroke capture with no user gesture needed
```

The any-Focus-enters-Insert behavior itself is documented as deliberate at
main.rs:4684-4697 (a caret must accept typing); the defect is the dead
`insert_intent_at` field whose doc promises the opposite, and the keystroke-
hijack impact being accepted without a security note. Same root as the
edit-bridge misattribution — this is the same-tab variant. Fix: implement the
gesture gate or delete the field + doc and document the tradeoff.

### 5 MEDIUM (Windows-only) — predictable named-pipe heartbeat lets another local user force a kill/restart loop and the 3-strike supervisor exit

**Where:** `apps/buffr/src/main.rs:1029-1032` (`\\.\pipe\buffr-supervisor-<pid>`
— pid is enumerable), `1526-1543` (`CreateNamedPipeW` with a null security
descriptor at 1541 — Microsoft documents the default pipe DACL as granting
Everyone read access), `1658-1679` (first connection treated as the child),
`1320` (no pings → `kill_and_reap`), `1407-1416` (3 hangs / 30 s → exit 1).

```
Repro: Windows; another local user enumerates buffr's pid and either opens the
       pipe with GENERIC_READ or pre-creates their own instance of the name
Expect: the watchdog only ever talks to the real child
Actual: the attacker's handle completes ConnectNamedPipe → watchdog starts →
       no pings → the browser is killed and restarted; 3× in 30 s → supervisor
       exits 1, browser left dead
```

Unix is not affected (0600 socket in a 0700 per-uid dir, verified §11). Distinct
from §11 item 6 (same pipe, thread-leak angle) — this is the cross-user DoS
angle. Fix: per-launch random pipe-name suffix, an explicit creator-only DACL,
or pass the handle via inheritance. **Caveat:** DACL facts are from Microsoft's
documentation, not a local run; the connect-race semantics are inference.

### 6 LOW-MEDIUM — server-chosen download filename is written to `default_dir` verbatim; an existing file is silently replaced

**Where:** `crates/buffr-cef/src/handlers.rs:1295-1296`
(`target_dir.join(safe_name)`), `:1310`
(`show_dialog = if ask_each_time {1} else {0}`), `:1333` (`callback.cont`),
`crates/buffr-config/src/lib.rs:565` (`ask_each_time` defaults **false**),
`sanitise_filename` (handlers.rs:1822-1844 — traversal-safe but never
uniquifies).

```
Repro: a page triggers a download whose suggested name already exists in
       default_dir (e.g. "report.pdf")
Expect: Chrome-style uniquification ("report (1).pdf")
Actual: the existing file is replaced (Chrome uniquifies at this point)
```

**Caveat:** CEF's exact overwrite-vs-uniquify behaviour at this `cont` call
cannot be verified from this tree — confirm against CEF before treating as
exploitable. If CEF writes verbatim, add a `file (n)` pass or force
`ask_each_time`.

### 8 LOW — Windows single-instance IPC has no peer authorization; the forward allow-list even includes `file:`/`chrome:`

**Where:** `apps/buffr-app/src/single_instance.rs:569-584` (`peer_is_us` on
Windows returns `true` unconditionally — pid is only logged), `76-87`
(`ALLOWED_FORWARD_SCHEMES` includes `file`, `about`, `chrome`, `view-source`,
`mailto`; only `javascript:`/`data:` are excluded), pipe name derivable from the
cache path (single_instance.rs:236-246). Unix is properly `SO_PEERCRED`-gated
(542-565).

```
Repro: Windows; another local user opens the named pipe and sends {"urls":
       ["file:///C:/Users/victim/secret.txt"]}
Expect: the peer is verified as the same user before anything is honoured
Actual: accepted unconditionally; the URL opens in the victim's browser
       (content shown on the victim's screen, not exfiltrated)
```

Reachability depends on the pipe's default DACL (unverified here); the check
itself is a no-op either way. Fix: real peer-credential check or a restrictive
pipe security descriptor.

### 13 LOW — console-IPC nonce falls back to a non-cryptographic RNG when `getrandom` fails

**Where:** `crates/buffr-core/src/console_nonce.rs:66-81` (`new_console_nonce`),
`84-107` (`fallback_entropy`: splitmix64 seeded from wall-clock ^ counter ^
stack address). Documented as deliberate (59-65).

```
Repro: a broken sandbox where the OS CSPRNG is unavailable (the one environment
       where the nonce is the only cross-frame boundary)
Expect: cross-frame forgery stays closed
Actual: the 128-bit token is seeded from guessable-ish values; a subframe could
       forge hint/edit/media events
```

Fallback-only and explicitly a documented tradeoff ("degrades the hardening
rather than bricking"); listed for completeness, not as a regression.

### Hardening

- `password-store=basic` — cookies/passwords plaintext in the profile dir;
  deliberate (app.rs:153-161) but worth a config escape hatch to a real keyring.
- Downloads `u64 → i64` casts round-trip losslessly today but any future
  `ORDER BY` on them would see negatives.
- `resolve_default_dir` returns a relative `default_dir` verbatim (config
  lib.rs:585-587) despite the doc claiming absolute-only — downloads land
  relative to cwd.
- Config `homepage`/`new_tab_url`/search-engine `url` templates never
  scheme-validated at parse (same-user trust; navigation guards live elsewhere).
- Store DB paths opened without `SQLITE_OPEN_NOFOLLOW` — a symlinked DB path is
  followed (same-user trust).
- Windows `%TEMP%\buffr` created with no owner/ACL verification
  (main.rs:1168-1173); per-user TEMP makes it same-user-only today.
- Unix signal-forwarding pgid-reuse window between child reap and
  `child_pid_slot.store(0)` (main.rs:571) — milliseconds, same-uid.
- `InternalServer::set_routes` silently drops a poisoned-lock update
  (internal_server.rs:242) — routes freeze, no crash.
- `WaylandNativeHandles` unsafe `Send`/`Sync` on raw pointers (types.rs:42-43) —
  documented lifetime invariant, enforced nowhere; consumed by buffr-webkit.
- The page→app bridge gate (findings 2+3) is the top security item.

---

## 13. Tidy 2026-08-05 (8744e18) — cleanups

Every candidate below was verified against its call sites (`rg` across the whole
workspace — a `pub` item in a lib crate is not linter-flagged as dead, so grep
is the only oracle). Ranked by size of win. The entire "Dead code (delete)"
subsection shipped 2026-08-12 (five commits: fa752f5, d9790ae, 1a1caca, 82f4a78,
e7cd174) — media_js, the four BrowserHost methods, BrowserHost::new,
make_client, insert_intent_at, Mode, PendingPopupAlloc, TabOptions/TabSession,
pop_front/ peek_front, set_routes, row_at/InputBar::paint,
KeyChord::new/Keymap::leader/ HintAlphabet::is_empty, Engine::count, edit_sink,
the windowing accessors, the paint-path dead computes, `_phantom`, the three
unreachable "space" arms, the fuzz no-op loop and cef_minimal_url are all gone.

### Duplication (extract a helper)

- **`run_heartbeat_loop` duplicated across unix/windows** (apps/buffr-app
  heartbeat.rs:184-255 vs 722-783) — same ~60-line loop, differing only in the
  stream type (`UnixStream` vs `std::fs::File`, both `io::Write`) and
  "socket"/"pipe" in log strings. Unify as `run_heartbeat_loop<W: Write>`; the
  identical `Heartbeat` struct + `mark_alive`/`is_fatal`/`tick` (69-169 vs
  623-718) can share a non-cfg block. Caveat: the Windows arm is not compiled
  locally; a Windows CI round-trip proves it.
- **Atomic-write helper duplicated** — session.rs:147-164 and
  crash_guard.rs:137-148 are the same create-dir-all → `to_string_pretty` →
  `.json.tmp` → rename. Extract
  `write_json_atomic(path, &impl Serialize, what)`.
- **"Last tab gone → graceful exit" ×3** — main.rs:2689-2703, main.rs:2729-2738,
  context_menu.rs:638-644 all run
  `save_session_now(); mark_clean_shutdown(); shutdown_flag.store(true); request_redraw();`.
  Extract `fn request_exit`.
- **Deadline-clamp idiom ×9** — event*loop.rs:2024-2027, 2033-2036, 2039-2042,
  2058-2061, 2065-2068, 2070-2073, 2077-2080, 2084-2087, 2090-2093, each
  `let deadline = match self.X { Some(at) if at < deadline => at, * => deadline }`→`let deadline = deadline.min(self.X.unwrap_or(deadline))`.
- **`cli.rs` `open_*_for_cli` ×5** (cli.rs:46-52, 88-94, 130-136, 156-162,
  191-197) — identical "profile_paths → create_dir_all(data) → store open"
  scaffolding differing only in the store constructor. One helper taking
  filename + `impl FnOnce(PathBuf) -> Result<T>`.
- **chrome_paint modal-popup paint blocks** (chrome_paint.rs:224-263, 282-321) —
  both destructure `ModalPanel`, fill border, fill inner, paint content. Extract
  `paint_modal_panel`.
- **Statusline right-pen cells ×7** (buffr-ui lib.rs:206-263) — `private`,
  `update_indicator`, `find_query`, `hint_state`, `engine_hint`, `count_buffer`,
  `zoom` repeat the same build-string → width → `right_pen -= w` → draw → `-= 8`
  block. Extract `right_cell(...)`.
- **`cargo build -p buffr -p buffr-app -p buffr-helper` block** (xtask:664-682
  vs 1044-1059) — copy-pasted; extract
  `build_binaries(workspace, release, target)`. The
  `if release { "release" } else { "debug" }` profile string also repeats at
  658/1028/1532/1811.

### YAGNI / over-abstraction

- **`Keymap::audit_default_bindings(_leader)`** (buffr-modal keymap.rs:227) —
  dead parameter (named `_leader`), and the doc claims it "renders the resolved
  `<leader>` chord" which the code does not do. Drop the param, the caller-side
  leader resolution (cli.rs:375-389), and fix the doc (keymap.rs +
  docs/site/keymap.md).
- **`buffr_cef::new_tab` re-export module** (cef lib.rs:82-85) — no users; apps
  import from `buffr_engine::newtab` (main.rs:156-161). The file's own comments
  frame it as a deliberate Phase-6e compat shim — owner's call whether the shim
  still earns its keep.
- **Never-constructed enum variants** (flag, not action — documented future
  surface): `MediaType::Canvas/File/Plugin` (buffr-engine types.rs:83-85),
  `CertState::Secure/Insecure` (buffr-ui lib.rs:54-55).
- **`Statusline::progress`** (buffr-ui lib.rs:101, paint block :289-305) —
  write-only today (stays 1.0, bar never renders); documented Phase-3
  placeholder for CEF's `OnLoadingProgressChange`. Lower confidence: deleting is
  behavior-preserving but removes a planned-feature placeholder — wire it or
  delete it, don't leave the write-only field.

### Minor

- `set_min_size`/`set_max_size` near-identical
  (windowing/other/window.rs:36-57); `run_check_for_updates`/`run_update_status`
  differ in one call (cli.rs:347-363); `omnibar_suggestions` history/bookmark
  loops share a dedup+display+cap body (main.rs:4303-4339). (The
  `two_char_px`/`badge_content_w` hoist and the `GUTTER` module-scope move
  shipped 2026-08-12 — 01fe932, 82f4a78.)

## 14. Performance review 2026-08-05 (8744e18) — findings

### 1. Per-glyph mutex lock, twice per char, on every chrome frame

**Where:** `crates/buffr-ui/src/font.rs:39-58` (`glyph_entry` takes
`lock_ignore_poison(&f.cache)` + Arc clone per glyph), and every chrome path is
measure-then-draw — `text_width`/`char_width` (font.rs:155-166, 137-142) then
`draw_text` (font.rs:168-188) re-look-up each char.

**Why hot:** `TabStrip::paint` (tab_strip.rs:257-369), `Statusline::paint`
(lib.rs:189-287), `InputBar::paint_at` (input_bar.rs:357-397) — per dirty frame
(keystroke, hover, load progress, cursor blink). A 20-tab strip × ~30-char
titles ≈ 1200 locked lookups/frame ≈ 50-80 µs — ~4 ms/s of one core at 60 fps.
The cache is only ever touched by the UI paint thread, so the mutex serializes
single-threaded access.

**Fix:** `thread_local!` + `RefCell` cache (rasterize-on-miss is already outside
the lock, font.rs:43-58), or take the lock once per text run.

### 2. Per-tick tab-strip resync: 3-4 O(N) passes, two HashSet builds, deep compare, every tick even when nothing changed

**Where:** `apps/buffr-app/src/main.rs:2973-3015` (`refresh_tab_strip`) and
`3110-3132` (favicon pump), both called unconditionally each tick
(event_loop.rs:1799, 1807).

**Why hot:** the idle path at 60 Hz. `tabs_summary()` (host.rs:1487-1492) locks
`tabs` and clones 2 Strings per tab (`display_title`, `display_url_for` — the
latter another per-tab mutex + clone, host.rs:612-614); a `HashSet<i32>` of live
ids is built and `favicons.retain` against it every tick (2977-2979); the whole
`Vec<TabView>` is rebuilt (second `title.clone()` per tab, 3003) and
deep-compared char-by-char (3015); the favicon pump builds a second HashSet,
retains again, and compares URLs (3111-3118). ~4-6N small heap allocs per tick
even with zero state change.

**Fix:** a host-side `tabs_generation: AtomicU64` bumped on title/URL change,
open/close/select/move/navigate — skip the whole resync on idle ticks. Failing
that, throttle the two `retain` passes (~30 s) and track `pinned_count`
incrementally (also covers finding 9).

### 3. Insert-mode keystrokes ship the entire field value over console-log IPC, re-parsed twice

**Where:** `crates/buffr-core/assets/edit.js:319-333` (`onInput` emits the full
`valueOf(el)`), `227-231` (`emit` → `console.log` over the whole string),
`crates/buffr-core/src/edit.rs:210-268` (`parse_payload` runs two full
`serde_json` passes — `TypeTag` at :214 then `RawMutate` at :243), capped at 256
KB (edit.rs:184) that JS never knows about.

**Why hot:** one `input` event per keystroke while typing in a field; the full
field value crosses renderer→browser IPC and is tokenized twice per keystroke —
a 100 KB textarea re-serializes ~100 KB + IPC + two parses per keypress.

**Fix:** (a) one-pass tagged deserialization (custom `Visitor` or a
`#[serde(tag="type")]` enum) — halves per-keystroke JSON cost, trivial win; (b)
JS-side coalescing of `mutate` events (~100 ms) or a delta for the common append
case — protocol complexity, optional.

### 4. `tick_splash_js_push` polls the active URL every tick when the user is NOT on the new-tab page — 3 locks + String clone per tick

**Where:** `apps/buffr-app/src/main.rs:5720-5756`. The gate (5727-5731) only
short-circuits while `splash_js_next_push` is armed — which is set only on the
new-tab page. On any normal page it stays `None`, so every tick runs
`engine.active_tab_live_url()` (5735) → host.rs:635-649: locks `tabs`, locks
`active`, then `display_url_for` (third lock + String clone), forever.

**Fix:** drive it off the events that already exist — `pump_address_changes`
(event_loop.rs:1437) and the tab-switch path: set a `splash_recheck_pending`
flag and return immediately when clear. Alternative without touching the host:
poll at a 500 ms cadence off the new-tab page.

### 5. Per-paint mutex + Arc clones on the OSR routing path

**Where:** `crates/buffr-cef/src/osr.rs:359` (`is_registered_popup` takes the
`popup_frames` mutex per call), reached from `resolve_dims` (350-384) via
`view_rect` (144-153, "called on every frame") and `screen_info` (155-181,
"every frame paint"), and from `resolve_frame_view` (388-412) per `on_paint` —
~3 uncontended mutex acquisitions + 2 Arc clones per paint at 60/s, even with
zero popups open.

**Fix:** once `main_id` is set and equals the painted id, cache a "main
confirmed" `AtomicBool` and take a lock-free fast path; the A3 invariant
(registration precedes any popup paint) makes this safe.

### 7. Measure-then-draw re-walks every glyph; the input bar walks its buffer ~4× per keystroke

**Where:** `crates/buffr-ui/src/lib.rs:270`+`:287` (`truncate_to_width` then
`draw_text`), `tab_strip.rs:368-369`, `input_bar.rs:376-397` (visible-char
budget with a lock per char, cursor/total counts, a `visible` substring
`collect`, `text_width` of the prefix, then `draw_text`). `truncate_to_width`'s
early bail only triggers on overflow (lib.rs:346-364), so every fitting char is
measured once and drawn once — 2 cache lookups/char regardless.

**Fix:** a single-pass `draw_text_budget` that measures as it draws and returns
the end pen position, eliminating the separate walk and the intermediate
`String` collect. For titles/URLs stable between frames, caching the truncated
result keyed by `(title, max_px)` is the memory-for-speed alternative.

### 8. `__buffrHintFilter` rebuilds every hint's DOM on each hint keystroke

**Where:** `crates/buffr-core/assets/hint.js:145-168` — for every still-matching
hint (N up to 256), `textContent=''` + `createElement` + `createTextNode` + two
`appendChild`s per keystroke, including hints whose match state did not change.
Fired by Rust per hint `Filter` key (host.rs:2311-2365).

**Fix:** retain per-overlay state (last `typed.length` seen) and skip overlays
whose class/text would not change; rebuild the strike-through span only when the
typed prefix grew. A few bytes of state per overlay.

### 10. `open_pending_tabs` re-materializes the full tab list per restored tab — O(M×N) at startup

**Where:** `apps/buffr-app/src/main.rs:2846-2904` — the restore loop calls
`host.tabs_summary()` (each O(N) with per-tab locks + clones) just to read the
last entry's `browser_id` (2869, 2891, 2900).

**Fix:** an accessor for "the tab that was just opened" — the open calls already
return the new `TabId`; expose `browser_id_of(id)` (one lookup) or capture the
summary from the `open_tab` return. Startup-only, ~1-10 ms for a large session.

### 14. Internal-server per-request waste: whole `Routes` table cloned per connection, String alloc per lookup

**Where:** `crates/buffr-engine/src/internal_server.rs:298-303` (per-connection
`routes.lock()` … `g.clone()` cloning every handler Arc + content-type String)
and `146-158` (`lookup` → `normalize_path` allocates a fresh String for the
HashMap key; paths already start with `/`).

**Why hot:** per navigation to `buffr://new`/`buffr://settings` — not per-frame,
but the clone is unnecessary per-request work.

**Fix:** Arc'd `RouteEntry`s and key lookup on `&str` without the normalize
alloc.

### 16. `hit_test_tab_strip` runs its mutex + O(N) pinned scan on every pointer move

**Where:** `apps/buffr-app/src/event_loop.rs:589` (per `PointerMoved`, up to ~1
kHz on high-polling mice) and per tick via `refresh_tab_strip` (main.rs:2994);
the scan is `download_notice_queue_len` mutex + `filter(| t| t.pinned).count()`
(main.rs:4048-4049).

**Fix:** cache `pinned_count` in `AppState` (update on pin toggle) and reuse the
last hit-test result; absorbed by finding 2's gating.

### 17. Minor per-frame allocations

- `crates/buffr-ui/src/lib.rs:225-262` — `format!`/`format_hint`/`format_find`
  allocate 0-4 Strings per statusline frame.
- `crates/buffr-ui/src/tab_strip.rs:256` — `pinned_glyph` allocates a String per
  pinned tab per frame; `:293-299` — the badge `chars().take(2).collect()`
  allocates per badge tab per frame.
- `crates/buffr-permissions/src/lib.rs:244, 267, 283` — `as_storage_key()`
  allocates even for unit variants; rare (per prompt).

---

## 15. Robustness audit 2026-08-05 — follow-ups from TODO.md

### Open — need a decision

#### 15-1. Internal-server token: panic on CSPRNG failure vs graceful fallback

**Where:** `crates/buffr-engine/src/internal_server.rs:548` —
`getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable")`.

The comment explains the panic is intentional — a system without an OS CSPRNG is
broken beyond recovery. But on minimal containers/CI the `getrandom` syscall can
fail, and this is the per-launch auth token for the loopback server, so a panic
bricks the whole browser. **Decision needed:** keep the panic, or fall back to a
mixed entropy pool (wall-clock ^ counter ^ stack address, the same tradeoff
`console_nonce` already documents at §12 audit finding 13).

#### 15-2. `buffr-webkit` / `buffr-poc` code-quality review deferred

**Where:** `Cargo.toml:55-67` — both excluded from the workspace (and CI) for
the missing `wpewebkit-2.0` system deps; the webkit crate carries significant
`unsafe` blocks.

Deferred as experimental, not production code. Overlaps §2's "`buffr-webkit` is
still not built by CI" verification gap — building it in a Linux job would be
the first step toward reviewing it at all. **Decision needed:** keep both out of
the workspace (status quo), or add a Linux-only CI job to at least compile them.

#### 15-3. `BUFFR_CONNECT_GRACE_MS` has no bounds

**Where:** `apps/buffr/src/main.rs:225-231` (unix) and `1010-1016` (windows
twin) — `std::env::var("BUFFR_CONNECT_GRACE_MS")` parsed straight to a
`Duration`, no clamp.

An extreme value (0, or millions of ms) makes the supervisor time out
immediately or wait effectively forever — harmless in production (the env var is
a test override only, and never gates a security property: the accept thread's
own `ACCEPT_DEADLINE_SLACK` deadline at main.rs:223 derives from the same grace,
so it just scales with it), but a foot-gun for anyone scripting tests.
**Decision needed:** leave it (documented test-only override), or clamp to a
sane range (e.g. 10 ms..=5 min) at parse time.

## 16. Code review 2026-08-06 (63b8a25) — findings

### Hardening

- **WebAudio-only audio on a stashed tab.** `pause_media.js` pauses only
  `<video>`/`<audio>` elements; an `AudioContext` keeps producing audio. Whether
  a muted CEF browser still emits `OnAudioStreamStopped` (which clears the media
  indicator) is a CEF runtime detail this tree cannot verify.
- **Stale `EditFocus` after tab round-trip.** While tab B is active, tab A's
  `Blur`/`Focus` events are dropped, so returning to A can show Insert mode
  against a field the page has since blurred; no edit-state resync on tab
  switch. Narrower than the misattribution bug it replaced; self-heals on the
  next real focus.

## 17. Audit 2026-08-06 (63b8a25) — findings

### 2 LOW — edit-bridge browser attribution collides across engines: webkit `TabId` and CEF `browser.identifier()` share one `i32` space

**Where:** `apps/buffr-app/src/main.rs:4701-4712` (`drain_edit_focus_events`
trusts `browser_id`);
`crates/buffr-webkit/src/platform/runtime.rs:893, 1348-1353` (events tagged
`ctx.tab_id.0 as i32`); webkit mints `TabId(st.next_id)` starting at 1
(`worker.rs:81`, `runtime.rs:3878`). CEF identifiers also start at 1 and
increment per browser.

The edit-attribution fix compares a page's tagged `browser_id` against the
active engine's active-tab `browser_id` as a single flat integer. Both backends
number tabs from 1, so in a cef+webkit config a webkit tab's id collides with a
CEF tab's id. A page on a **background webkit tab** can forge a `Focus` event
(the page nonce is page-readable — known accepted limitation, §3) tagged with
its own tab id; if that id equals the active CEF tab's identifier, the event is
accepted and the CEF tab flips into Insert — the exact cross-tab keystroke-
capture the fix closed, restored across the engine boundary.

Caveat: cef-only configs (the production default) are sound — all browsers share
the CEF id space. Only multi-engine configs with webkit are exposed; webkit is
experimental and not built by CI; the attacker cannot choose which ids its tabs
receive. Fragile but real.

**Fix:** namespace the attribution (`engine_id + browser_id`), or gate
webkit-tagged events to webkit-active-tabs at the engine level.

### Hardening

- Internal server accepts a request with no `Host` header
  (`internal_server.rs:444`): defence-weakening only, not exploitable — the
  128-bit per-launch token is still required, browsers always send `Host`, and a
  raw-socket attacker who already holds the token gains nothing from omitting
  the header.
- `tick_splash_js_push` gate `url.starts_with("buffr://new")` is loose but the
  scheme is unreachable from page content and the pushed HTML is static —
  cosmetic only.
- Webkit URI-scheme clipboard handler (`runtime.rs:1400-1569`): gated to pages
  whose URI starts with `buffr://`; all such documents are app-served. The
  10k-line webkit FFI/worker/wpe_subclass code otherwise remains §15-2's
  deferred item — not audited beyond the scheme handler and id-space check.

## 18. Tidy 2026-08-06 (63b8a25) — cleanups

### Dead code (each verified: whole-workspace `rg` shows zero callers)

1. **`crates/buffr-webkit/src/platform/engine.rs:380-381` —
   `set_newtab_html_provider` and the `newtab_html_provider` field are
   write-only** (field :59, written :332/:381, never read; setter has no
   callers). Experimental crate, excluded from the workspace, review deferred
   (§15-2) — reported for completeness, not expected to be actioned with the
   rest. **Action:** delete setter + field, or wire the read.

## 19. Performance review 2026-08-06 (63b8a25) — findings

Both findings shipped 2026-08-12 (c12da19): the registry+loader pair and loaded
grammars are cached per process (the second request for a language pays only the
`lookup_only` stat-walk), and `render_spans` writes straight into the output
buffer with a bounded `match` on the palette's capture names — zero per-span
heap traffic on the common path.

## 21. Audit 2026-08-11 (63b8a25..HEAD) — findings

### Hardening

- homepage/`--homepage` not covered by the scheme gate (initial navigation,
  trusted input); gating it for defence-in-depth is an open decision.
- `session.json` persists the raw loopback URL for `buffr://` pages — the same
  token class, pre-existing and documented (`host.rs:388-393`), out of the
  delta. Noted so a future "fix the token in history" doesn't stop at
  history.sqlite.

## 22. Sweep 2026-08-28 (63b8a25..3bdd7ec verified) — four-pass, whole codebase

Full-codebase sweep (tree clean at 3bdd7ec): four parallel read-only agents
(engine boundary / app layer / stores+xtask / ui+modal+config+inhibit), each
running review + audit + tidy + perf, briefed against §10-21. Every finding
below was re-verified line-by-line at the cited file by the sweep owner;
candidates that died under verification are listed under Cleared with the
reason. Excluded per §15-2: `buffr-webkit`, `buffr-poc`.

### Cross-cutting (flagged by two or more areas)

1. **`html_escape` exists in four crates** (merged area-A tidy + verified
   wider): `crates/buffr-cef/src/html.rs:15`,
   `crates/buffr-view-source/ src/lib.rs:286`,
   `apps/buffr-app/src/main.rs:1218`, `crates/buffr-core` escaping inside
   hint.rs — all the same five-metacharacter escaper (html.rs:3's comment even
   says it consolidated "two"). Extract one `pub` helper (buffr-core, already a
   dep of all four users' crates' siblings). Verified: same match arms in each
   copy.
2. **mode→label table is a sync-by-comment quad, two case-variants** (C-T1,
   verified wider than reported): `buffr-ui/src/lib.rs:397-405` and
   `apps/buffr-app/src/main.rs:5811` return UPPERCASE;
   `buffr-modal/src/ keymap.rs:406-414` and `buffr-config/src/lib.rs:990-998`
   return lowercase. Fix: one source in buffr-modal (`PageMode::name()`),
   uppercase at the two call sites that need it (`.to_uppercase()` on a
   `&'static str` is `Cow`-free for ASCII; or keep two thin wrappers over one
   match).

### Review findings (correctness)

- **A-R2 LOW — find results are untagged; a background tab's in-flight find
  stream overwrites the active tab's statusline counts.** Where:
  `crates/buffr-cef/src/handlers.rs:552-585` pushes into the one-slot
  `FindResultSink` with no browser id; consumed undiscriminated at
  `apps/buffr-app/src/main.rs:3306-3327`. Same class as the hint misattribution
  fixed by 96f9b54 (`TaggedHintEvent`, hint.rs:432-449); `FindResultSink` never
  got the tagging. Fix: tag with browser_id and drop non-active-tab results at
  the drain (apps half).

### Audit findings (security)

- **C-A1 MEDIUM — font glyph cache is unbounded and is fed by page-controlled
  text.** Where: `crates/buffr-ui/src/font.rs:39-58` — `glyph_entry` inserts
  every first-seen `char` into a process-lifetime `HashMap` (`FACE` OnceLock,
  font.rs:65); no eviction, no cap (verified: no clear/retain/remove anywhere in
  font.rs). Tab titles and the URL statusline are page-controlled and walked per
  painted frame, so a page rewriting `document.title` with fresh exotic
  characters inserts ~20-50 new glyphs per dirty frame — hundreds of MB over a
  long session. Truncation bounds chars-per-title per frame, slowing but not
  capping.
  ```
  Repro: setInterval(() => document.title = <fresh random chars>)
  Expect: bounded cache
  Actual: every distinct char ever measured is retained for the process
  ```
  Fix: cap/flush the map past ~4k entries (re-rasterizing evicted glyphs is
  cheap) or switch to an LRU.
- **A-A2 LOW (caveated) — `on_console_message` converts every console line to a
  Rust String before any prefix gate.** Where: `handlers.rs:1005-1006`
  (`let text = message.to_string();`), sentinel fast-path and redaction only
  afterwards. A page looping `console.log( bigString)` allocates the full
  UTF-16→UTF-8 copy per line in the browser process. Caveat: Chromium may cap
  console message length itself — not verifiable from this tree; A2 is also A1's
  amplifier. Fix if confirmed: length-check `message` (UTF-16 length is cheap)
  before converting.

### Tidy (behaviour-preserving)

- **A-T1 — three copies of the ASCII-forcing JS-string escaper:**
  `crates/buffr-core/src/hint.rs:556-584` (`json_string_inner`), the inline
  labels escaper hint.rs:508-531, `crates/buffr-cef/src/host.rs:2845-2872`
  (`json_string_literal`). Promote one to `pub` in buffr-core, delete the other
  two bodies.
- **A-T4 — third near-identical ureq agent config** (timeout/UA/
  `max_redirects(0)`): updates.rs:104-114, image_copy.rs:111-118,
  view_source_scheme.rs:521-531. One `buffr_core` helper returning a configured
  Agent keeps the load-bearing redirect-off policy from being re-defaulted by a
  future fourth fetch.
- **C-T2 — buffr-modal's edit-mode layer (~540 lines) has zero callers outside
  its own crate** (whole-workspace rg; apps' own comment says "no Rust
  EditSession", apps/buffr-app/src/main.rs:828): `edit_mode.rs` `EditSession`
  and `host.rs` `BuffrHost`/`BuffrEditIntent`/`BuffrBufferId`. The crate is
  published, so this is API surface — wire it (Phase-2 intent) or drop it in the
  next breaking window (c7b861f shows the pattern). Owner's call.

### Performance

- **D-P3 (noted, deliberate) — `buffr_lower` full-scans with two `to_lowercase`
  allocations per row per omnibar keystroke** (bookmarks/ src/lib.rs:566-580):
  the M40 tradeoff, documented in the crate; recorded so it is not re-derived.

### Cleared (suspected and disproved — highlights; full lists per area in the task transcripts)

- **§17-3 hardening item is FIXED by 8d8847c** — `handle_connection` now returns
  400 before the token check when `Host` is absent (internal_server.rs:436-441),
  `host_is_ours` rejects no-port/wrong-port/ foreign-name/empty forms (tests
  :986-1040), pinned by `rejects_missing_host_header` (:722-733).
- A-R3 (internal server "empty request gets 405") — refuted:
  `parse_request_line` returns `None` for a 0-2-token line (empty line →
  `parts.next()` on raw_path is `None`, internal_server.rs:492) → 400 at
  :421-424; a 3-token non-GET line is exactly what 405 is for.
- 61c2702 nonce-gating is behaviour-preserving — each branch still runs the full
  sentinel+nonce parse; the prefix gate only skips nonce-table lookups.
- ba8ffd9 redirect-off is complete and nothing needed redirects — the only
  request is the pinned api.github.com URL; a 3xx body fails release-JSON
  parsing → `NetworkError`.
- InflightGuard acquire is not off-by-one (fetch-add-then-check yields exactly
  MAX_INFLIGHT; rejected caller decrements).
- 267dd9a recycled-buffer zeroing covers the resize-vs-swapped-Vec hole
  (osr.rs:269-284 re-allocates then copy_from_slice).
- 9906066 watcher fix is complete: inner `catch_unwind` wraps only the callback;
  nothing left to poison after the commit removed the Arc<Mutex>; outer catch
  covers loop machinery; canary test fails pre-fix.
- 4f6122c, 01fe932, 3e3f6b2 verified behaviour-preserving (details in the Area C
  transcript): empty-head guard survives the host_head extraction; the
  panel-width cache cannot go stale (entries immutable for menu life); widgets
  are stateless-by-contract so paint-every-frame holds no stale state.
- 81aa343 no-change skip cannot skip a needed write (writes only received/total
  bytes, skip compares exactly those, terminal gate precedes); 15cb0a5 caps
  bound the parse linearly; ef0d8f4 anchor-as-one- token holds; c12da19 grammar
  cache is sound (single-lock check+insert, Sync via tree-sitter/libloading
  impls, all span slices bounded).
- bea7ec4 bounds are genuinely mid-stream: download cap checked per chunk before
  continuing; extraction sums header sizes and bails before unpacking the
  offending entry; header-only bomb test proves no write precedes the bail.
- SQL across all five stores parameterized; the only string-built SQL is the
  count-derived `IN (?,…)` list (bookmarks :718-725); `escape_like` + FTS
  quote-doubling correct; migrations transactional with TooNew guard.
- view-source/image_copy SSRF: 3xx not followed, worker re-validates with DNS,
  same-host exception only fires when the navigating page is already on that
  host (CEF supplies frame.url); integer/odd IPv4 caught fail-closed; `data:`
  images memory-bounded pre/post decode.
- Downloads `sanitise_filename` traversal-safe; `open_path` never routes through
  a shell (regression-tested for cmd.exe re-parsing).
- Favicon blit rejects bogus CEF-supplied dimensions (checked_mul + length
  guard); bilinear taps clamped.
- `truncate_to_width` unicode-safe; InputBar `+1` scroll is the cursor-slot
  reservation, not an off-by-one; Statusline right-pen underflow cannot panic
  (draw_text clips).
- keybinding.rs slicing panic-free (quote-anchored ASCII slices behind len
  guards); fuzz targets cover both parsers.
- `mpsc::SyncSender` in WorkerInhibitor compiles green where cfg-compiled
  (current-toolchain `SyncSender: Sync`).
- Inhibit FFI: wayland null-checks both pointers; windows return-0 checked;
  macOS constants match IOKit typedefs; channel-full drops recovered by each
  worker's disconnect cleanup.

### Hardening (correct today but fragile)

- **fetch-cef ureq calls have no timeouts** (xtask/src/main.rs:222, 382 —
  default agent, `timeouts = None` in ureq 3.4.0): a hung CDN stalls CI until
  the runner timeout. Same fix shape as view-source's 10 s.
- Tar symlink entries: `unpack_in` blocks writes THROUGH an outpointing symlink
  but the symlink itself is created unvalidated — dangling links remain in
  `vendor/cef/` (same trust as the archive; cosmetic).
- Permissions origin PK stored verbatim (handlers.rs:1435-1437): a missing
  origin becomes `""` and would satisfy precheck for later no-origin requests;
  case variants duplicate sticky rows. CEF canonicalizes today.
- `UpdateChecker` interpolates `github_repo` into the URL unvalidated (same-user
  trust; a `^[A-Za-z0-9_.-]+/...` check would close it).
- Popup re-routes carry page-supplied URLs into `open_tab` with no scheme gate
  (f479118 covers session/CLI only) — Chromium blocks renderer- initiated
  `file:`, so defence-in-depth.
- Internal server: consider `Cross-Origin-Resource-Policy: same-origin`.
- Theme colours never reach ConfirmPrompt/PermissionsPrompt/InputBar (fixed
  COLOUR_* constants) — `theme.high_contrast` skips the prompts and omnibar;
  needs wiring when theme hot-apply lands (main.rs:802-825).
- `pub` fields with documented-but-unenforced invariants: `InputBar:: cursor`
  (char boundary), `ContextMenuOverlay::selected` (bounds).
- inhibit: wedged-worker with held inhibitor leaves `active=true` with no
  recovery (accepted never-block design); macOS constants transcribed from
  headers, not verified against an installed SDK; macOS/Windows bodies have had
  zero compilations (cfg-gated, no CI) — name/ABI drift possible.
- Known §12 hardening items (SQLITE_OPEN_NOFOLLOW, u64↔i64 casts) still present
  — cited, not re-reported.

### Coverage

- Area A (buffr-core/-engine/-cef + assets): read end-to-end incl. all injected
  JS. Not reviewed: inhibit platform bodies (moved to C),
  context_menu.rs:300-702 lower arms (entry traced), vendored cef crate
  internals.
- Area B (buffr-app/buffr/buffr-helper): headline findings verified by the sweep
  owner; the agent's FULL report was lost to delivery truncation — its
  cleared/hardening detail beyond the headline list is unrecoverable and
  constitutes the sweep's main coverage GAP (main.rs is 7.7k lines; only the
  traced slices are covered by verified findings).
- Area C (buffr-ui/-modal/-config + inhibit): all 24 files read end-to-end. Not
  reviewed: dependency sources (fontdue/notify/wayland read via docs, not
  installed copies), tests/e2e/render_* beyond the commit diff.
- Area D (stores/xtask/fuzz/CI/packaging): all read end-to-end incl. all 2825
  xtask lines and all five fuzz targets. Not reviewed: pkg/homebrew + pkg/scoop
  manifests, docs-site, vendor/.
- Platform-gated (never compiled locally or in CI): buffr-webkit (excluded
  §15-2), buffr-core/src/inhibit/{macos,windows}, all `#[cfg(windows)]` arms,
  buffr-app windowing `other` non-Linux paths. Static review only.
