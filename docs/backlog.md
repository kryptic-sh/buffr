# Backlog

Everything left over from the full-codebase reviews (v0.14.6 at `3eb8840` and
2026-08-04 at `a38fa86`), plus what surfaced while fixing them. `code-review.md`
is gone — its still-open findings are consolidated below.

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
- **The 2026-08-04 render-path fixes (P2/P3/P6/N5) are unit-verified only.** The
  banded chrome upload, scratch recycling and acquire-before-paint run under a
  live compositor; no Wayland session was available to smoke them.
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
- **`BUFFR_DISABLE_ZYGOTE=1` in the Linux smoke job** is a precaution on a
  session-less runner, not a fix for the ICU crash it was once blamed for. The
  smoke test passes locally without it; worth retrying once the job has been
  stable for a while.
- **`docs/backlog.md` is deliberately not in `SUMMARY.md`.** The book is the
  public user-facing site; listing it would publish unfixed security findings
  with reproduction steps.

---

## 4. Release follow-ups

- **`buffr-modal` owes itself a minor bump.** The C3 fix changed
  `Keymap::bind_chords` from returning `()` to `Result<(), BindError>` and made
  `mode_map`/`mode_map_mut` return `Option` — breaking, and the crate is at
  `0.1.5`, so pre-1.0 rules make that a **minor** bump to `0.2.0`, not a patch.
  Nothing forced the issue at the time: every dependant is a path dependency
  with no version requirement, the workspace is `publish = false`, and the crate
  version is independent of the workspace version a release tag carries. Bump it
  whenever `buffr-modal` is next versioned for its own sake.

- **`buffr-bin` on the AUR is stuck at `0.14.6-1`** while the workspace is at
  `0.14.9` — three releases behind, for two _different_ reasons.

  `v0.14.7` failed on the ssh regression described below. That is fixed. But
  `v0.14.8`, cut specifically to prove the fix, failed before reaching it: the
  AUR was in maintenance and the git remote refused the push
  (`The AUR is down due to maintenance` →
  `fatal: Could not read from remote repository`, exit 128). A re-run of the
  failed job on 2026-08-02 hit the same banner, and so did the `v0.14.9` tag run
  on 2026-08-03 — the maintenance window has now spanned three attempts over two
  days. Everything else in all three releases published normally.

  **So the ssh fix is still unproven.** It has never been exercised against a
  live AUR — all three attempts died earlier in the job than the host-key check.
  Whoever cuts the next release should confirm the `aur-bin` job actually
  reaches and passes the push, not just that the tag went green.

  **Probe with `ssh aur@aur.archlinux.org help`.** The maintenance gate sits on
  every AUR **command** — `git-upload-pack`, `git-receive-pack`, `help`, all of
  them — and _not_ on authentication. So the split that matters is
  command-versus-no-command, and two probes that look like they test the remote
  report "up" throughout a window that still blocks publishing:
  - **HTTP.** `https://aur.archlinux.org/` returns `200` and the RPC serves
    package JSON throughout. That is how the 2026-08-02 re-run was triggered too
    early.
  - **The bare ssh handshake.** `ssh -T aur@aur.archlinux.org` sends no command,
    so it never reaches the gate: it authenticates the key and prints
    `Welcome to AUR, <user>! Interactive shell is disabled.` while every command
    behind it is refused. That is how the `v0.14.9` tag was cut into a closed
    window — the handshake was checked minutes before tagging and passed, and
    the job failed anyway.

  All three were confirmed side by side while the AUR was down: the same shell
  got the welcome message from `ssh -T`, and
  `The AUR is down due to maintenance. We will be back soon.` from both
  `ssh aur@aur.archlinux.org help` and
  `git ls-remote ssh://aur@aur.archlinux.org/buffr-bin.git`. Either of the
  latter two is a valid signal; `help` is the cheaper one — same gate, no repo
  and no clone.

  **The pending action is a re-run, not a new release.** `v0.14.9` published
  everywhere else, so the AUR is the only gap; re-run the failed
  `Publish buffr-bin to AUR` job on the `v0.14.9` tag run once `help` answers.
  Unlike `v0.14.7`, that tag's workflow already carries the ssh fix, so a re-run
  is a real test of it. Do not cut another version just to retry.

- **The `v0.14.7` ssh regression itself.** The `aur-bin` job failed on the
  `v0.14.7` tag run with `No ED25519 host key is known for aur.archlinux.org`:
  the pinned `known_hosts` was written correctly, but `GIT_SSH_COMMAND` was
  supplied through an `env:` block containing a literal `~`, and neither git's
  `sh -c` (tilde expansion does not apply to the result of a parameter
  expansion) nor ssh itself expands it in a `-o` value.

  The tilde is **not** the regression — it has been there since the job was
  written, is present in the AUR jobs of every sibling repo (`gpur`, `hjkl`,
  `hodl`, `hrdr`, `inbx`, `krypt`, `pikr`, `sqeel`), and never mattered: under
  the previous `StrictHostKeyChecking=accept-new`, ssh trusts the host on first
  connect and never reads the file. The regression was flipping that to `yes` in
  `2702edc`, which made verification depend on a path that had always been
  broken. Confirmed against this repo's own runs — `v0.14.6` aur-bin=success
  with the same tilde, `v0.14.7` aur-bin=failure after the flip.

  Fixed in `ci.yml` by assigning `GIT_SSH_COMMAND` inside the step so `$HOME`
  expands first; reproduced and re-verified in an `archlinux:latest` container
  against `HOME=/github/home`. Reverting to `accept-new` would also go green,
  but on an ephemeral runner that is trust-on-first-use with no prior state — it
  accepts any key presented. Everything else in the release published — GitHub
  release, homebrew-tap, scoop-bucket, all 20 build/package jobs.

  The fix cannot be applied retroactively to `v0.14.7`: re-running that job
  checks out the workflow file as of the tag, which still has the bug, and the
  tag must not be moved. `v0.14.8` and later do carry the fix — re-running
  _those_ is worthwhile once the AUR is genuinely reachable.

- **The `known_hosts` pin was inert in all three publish jobs**, not just AUR.
  `brew-tap` and `scoop-bucket` survived the same flip only because the runner
  image appends github.com's keys to `/etc/ssh/ssh_known_hosts`
  (`images/ubuntu/scripts/build/install-git.sh` in `actions/runner-images`), so
  ssh verified against the global file while the per-job pin was never read. Now
  fixed alongside AUR, but it means the pin has never actually been exercised
  for github.com — a wrong pin there would still pass today.

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
  fails. Hit once during the refactor; `--all-targets` catches it.
- **Three abandoned branches were archived as tags, not deleted.** All local and
  remote branches except `main` were removed; the three carrying unmerged work
  are preserved as `archive/gpu-compositor-poc`, `archive/wl-subsurface-poc` and
  `archive/wxs-codepage-fix` (pushed; `archive/*` does not match the `v*`
  release trigger). All three predate the `apps/buffr` → `apps/buffr-app` rename
  (`bec1f30`) and need a real rebase, not a cherry-pick.
  `archive/wl-subsurface-poc` is the subsurface work the removed native
  compositing trio referred to.

---

## 7. Performance review, 2026-08-04 — shipped

A read-only pass over the current tree (2026-08-04) re-verified the 2026-08-02
findings (P1–P8, all still present then) and added N1–N5. **All of them are
fixed and pushed** — commits `8621a90`..`2caa56f`, each verified by the full
workspace gate. Summary of what shipped:

| Item     | Fix                                                                                                                                           | Commit    |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------- | --------- |
| P1       | omnibar bookmark search capped at 8 in SQL (`search_limited`)                                                                                 | `8621a90` |
| P4 + N1  | one `tabs_summary()` per tick; `refresh_tab_strip` returns `(tabs_changed, summaries)`; favicon pump shares them; `prev_tabs` clone-diff gone | `73dbb9e` |
| P2       | synthetic OSR frames send an empty buffer (`skip_pixels`), no UI-thread 8.3 MB memcpy                                                         | `6a2b0f8` |
| P6       | chrome texture uploaded as top+bottom strip bands (sub-rect `write_texture`), full buffer only when the animation/overlay paints the middle   | `a8ee2d8` |
| N5       | swapchain acquired before chrome paint/OSR clone — skipped frames waste nothing                                                               | `ce33978` |
| P3       | chrome paint buffer recycled via the stats channel (alloc + free on the UI thread)                                                            | `e924f92` |
| P5       | `Arc<GlyphEntry>` glyph cache (hit = refcount bump, no bitmap copy); one lookup per char in `draw_text`                                       | `7846958` |
| P7       | history FTS5 search + bookmark search use `prepare_cached`                                                                                    | `c976458` |
| P8       | 4 per-event allocs killed (tab_ids reuse, splash-deadline gate, cached context-menu entries + hit-test, `draw_char`)                          | `fb39033` |
| N2+N3+N4 | `outputs()` cached 1 s; paint-closure clones gated on dirty; `hint_status()` not polled outside hint mode                                     | `2caa56f` |

Verification: after each commit the full gate ran green — `cargo fmt --check`,
`clippy --workspace --all-targets -D warnings`, `build --workspace`,
`nextest --workspace` (1098 → 1107 tests as 9 new tests landed). The render-path
items and event-loop timing are compile/test-verified; runtime behavior under a
live compositor is not (no Wayland session) — see section 2.

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

## 9. Audit cleared + hardening, 2026-08-04

Things suspected during the same pass and disproved by tracing (worth recording
so they are not re-reported):

- **Script injection via `build_inject_script` / `build_poll_script`** — every
  interpolation is `serde_json`-escaped, splices land inside single-quoted JS
  literals, and `execute_java_script` feeds V8 directly (no HTML parser, so
  `</script>` is inert). Nonce separator/anchor handling is fail-closed and
  covered by tests.
- **SQL injection** — every query in the stores is parameterized; the FTS needle
  is a bound MATCH value; the bookmarks LIKE pattern is escaped with
  `ESCAPE '\'`. The netscape importer has no panic path on hostile input (byte
  offsets always char-aligned, regex is linear-time).
- **view-source XSS** — all interpolated text (body, spans, title) goes through
  `html_escape`; the only unescaped values are class names compiled from the
  bundled trusted query files.
- **Lock ordering** — no `tabs`+`active` inversion anywhere (M15 held); paint
  path takes only the per-host `osr_frame` mutex for the swap.
- **The double-swap guard / scratch-buffer length invariant** — watermark
  advances even on skipped frames; `last_osr_dims` is only written from
  gate-accepted frames, so `osr_scratch.len()` always matches.
- **`parse_console_event` panic paths** — bounds-checked prefixes, `serde_json`
  errors on hostile input; UTF-8 handled via char APIs, never byte slicing.
- **Context-menu / neutral-type mapping** — dead arm (`has_image_contents`
  without `media_type`) is unreachable from the only producer.

Hardening (correct today, fragile):

- `popup_close` uses `close_browser(0)` (host.rs:818) — a `beforeunload` handler
  on a popup may stall close, leaking the popup window + sinks until shutdown.
  Manual test owed with a `beforeunload` popup.
- `console_nonces` entries for popups are never forgotten (`on_before_close`
  removes frames/browsers but not the nonce) — permanent ~128-byte entries per
  popup ever opened. Call `console_nonces.forget(browser_id)` on close.
- `--private --smoke-test` exits via `libc::_exit` before the `_private_tmp`
  drop, leaking `$TMPDIR/buffr-private-<pid>-*` per smoke run (CI smoke does not
  use `--private`, so no current trigger).
- Config-watcher callback mutex can be poisoned by a panicking callback,
  silently skipping later reloads.
- `buffr-store::open_tuned` sets no `busy_timeout` — a second buffr process
  sharing the profile could surface `SQLITE_BUSY` mid-write.
- The Animation paint arm does not advance `last_osr_generation` — the gate can
  accept the same generation twice during a dim-mismatch animation (harmless
  today; defeats the double-swap guard's intent).

---

## 10. Code review 2026-08-04 (a38fa86) — open findings

Fresh full-codebase pass at low depth (correctness only; no style) on a clean
tree. Three of its seven findings were fixed the same day and are not listed: 2
(`buffr-src:` numeric-host guard bypass — `7bb805f`), 5 (`<C-c>` bound twice —
`7cefca1`, the M39 decision), 6 (edit-mode console sink unbounded — `447d448`).
The rest are still open:

### 1 HIGH — closing a tab left of the active tab leaves the old active tab running as if foregrounded

**Where:** `crates/buffr-cef/src/host.rs:1663` (`close_index`); the "hide the
previous tab" guard at `set_active_index` (`host.rs:1430-1437`).

`close_index` removes the tab and then calls `set_active_index(new_idx)` while
the stored `active` index still points at the removed tab's slot, so the guard
(`prev < tabs.len()`) never fires and the previously-active browser is never
hidden.

Repro: tabs [A, B, C], active = 2 (C visible). `close_index(0)` → [B, C], stored
`active` is still `Some(2)`, `new_idx = 0`; in `set_active_index` the guard
`prev < tabs.len()` is `2 < 2` → false, so C keeps `was_hidden(0)` with focus
and its timers, animations and audio run as if foregrounded until the next tab
switch. (The per-tab OSR frame is per-`BrowserHost`, so it is C's own render
budget that keeps burning.)

**Fix:** when `idx < old_active`, decrement the stored active index before
calling `set_active_index`, or hide by tab id rather than raw index.

### 3 MEDIUM — context-menu "Close tab" exits the app on the active engine's count alone

**Where:** `apps/buffr-app/src/context_menu.rs:631-636`.

Every other tab-close path — keyboard `close_active_tab_or_exit`
(`main.rs:2688`) and the pinned-close resolution (`main.rs:2728`) — sums
`tab_count()` across **all** engines before deciding to exit. The context-menu
path counts only the active engine, so closing the last tab of the active engine
while another engine still has tabs triggers `shutdown_flag`.

Repro: engines E1 (1 tab) and E2 (3 tabs); E1 active. Right-click the E1 tab in
the strip → Close Tab: `host.close_tab(id)` → E1 now 0 tabs → `host.tab_count()`
= 0 → `save_session_now` + `shutdown_flag` → the browser exits while E2 still
has 3 tabs.

**Fix:** sum `self.engines.values().map(|e| e.tab_count())` like the other two
paths.

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

### 7 LOW — context-menu tab target resolved by stale slot index

**Where:** `apps/buffr-app/src/context_menu.rs:713-724`.

`resolve_tab_target` indexes the _current_ tab list by the slot recorded when
the menu opened. Any tab-list change while the menu is open (a page's
`window.open` landing a background tab, another tab closing) shifts indices and
the action fires against the wrong tab. Narrow: most interactions dismiss the
menu first, and the pinned-close confirmation covers the misaim it cares about.

Repro: open the context menu on the tab at index 1; a background `window.open`
adds a tab at index 1 (shifting the target to 2); choose Close Tab — the tab now
at slot 1 is closed.

**Fix:** resolve by tab id at dispatch time, or re-locate by id.

---

## Corrections to the review itself

Three findings in the v0.14.6 code review were **wrong** and were deliberately
not "fixed". Recorded here so nobody re-files them:

- **L39** — `TYPEFLAG_FRAME` does have a caller
  (`crates/buffr-core/src/context_menu.rs` — there are now three files by that
  name), so it was kept while the genuinely-dead constants around it were
  removed.
- **L21** — `ActivationError` is live via `request_activation`; only the rest of
  the windowing parity surface was dead.
- **L46 (the cef half)** — the `cef` crate at 148.x wraps **libcef 147.0.14**,
  and `xtask` pins `CEF_VERSION_PREFIX = "147."`, so the docs' "cef-147" was
  already correct. Only the wording was clarified.

---

## Shipped 2026-08-04

Everything in this session was fixed and pushed, each item one commit verified
by the full workspace gate:

- **All 14 open code-review items** — the decision rows (H6, M39, M48, M49, L18,
  L19, L23, L36, L37, L38, L40, L41, W2, W8): commits `0c7ee4a`..`6a0efdd`.
  Sandbox enabled, `<C-c>` = StopLoading with a trie-walking audit, `:open`
  through `resolve_input`, startup knobs wired / dead knobs deleted, dead types
  and trait methods removed, history builder, gesture-gated clipboard and
  `xdg-open`.
- **All 12 perf findings** (P1–P8, N1–N5): commits `8621a90`..`2caa56f`, see
  section 7.
- **CHANGELOG**: every fix recorded under `[Unreleased]` in the same commit.
