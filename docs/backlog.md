# Backlog

Everything left over from the full-codebase reviews (v0.14.6 at `3eb8840` and
2026-08-04 at `a38fa86`), plus what surfaced while fixing them. `code-review.md`
is gone — its still-open findings are consolidated below.

Nothing here is a known-broken **build**: `main` is green on CI (all three
OSes), `cargo deny`, `cargo machete`, and the fuzz workflow. These are the items
that were deliberately **not** actioned, and why.

Grouped by what is actually blocking them.

---

## Shipped 2026-08-06

Worked from this backlog one slice at a time, each commit verified by the full
workspace gate (fmt --check, clippy --workspace -D warnings, build, nextest):

| Item                                                         | Commit    |
| ------------------------------------------------------------ | --------- |
| §16-1 same-host exception dead on the view-source worker     | `ac9b395` |
| §16-2/§17-1 redirects past the private-network gate          | `f630585` |
| §11-10 `skip_schemes = []` reverted to the defaults          | `1f8db30` |
| §11-12 out-of-range port emitted an unparseable URL          | `beb4824` |
| §9 `open_tuned` set no `busy_timeout`                        | `794fca2` |
| §12-7 session-restore/CLI URLs carried `javascript:`/`data:` | `f479118` |
| §10-7 context-menu tab target resolved by stale slot         | `86382ab` |
| §10-3 context-menu Close tab exited on active engine's count | `c70aed7` |
| §17-3 view-source of `buffr://` pages always error page      | `a6b7482` |
| §11-13 edit.js teardown leaked the `focus` listener          | `76a4b12` |
| §11-15 pinned-close confirm bypassed while another was up    | `8705de9` |
| §11-16 surrogate-pair chars dropped on the direct-text path  | `f183ad9` |
| §12-9 internal-server auth token persisted to history        | `b3b8aac` |

Each fix shipped with a test that was proven red on the old code where a unit
test was writable; the rest (CEF/js runtime paths) are compile-verified and
noted in their sections. The 2026-08-06 finding sections below still list items
that remain open.

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

---

## 11. Code review 2026-08-05 (8744e18) — findings

Fresh full-codebase pass at low depth (correctness only) on a clean tree, split
across four partitions (browser core, main app, engine/ui/supervisor,
data/tooling). Every finding below was re-traced at its cited lines after the
sub-agent pass. **One item in §10 was re-confirmed still open** (the
`close_index` HIGH — see the correction under Cleared), not re-filed.

### 1 MEDIUM — edit-mode events carry no browser attribution; a background or popup tab's page-driven field focus flips the ACTIVE tab into Insert mode

**Where:** `crates/buffr-cef/src/handlers.rs:795` (edit.js injected into every
main frame on `on_load_end`, no hidden/popup gate), `handlers.rs:1113-1117`
(event authenticated against that browser's own nonce but pushed into the one
shared sink), `crates/buffr-core/src/edit.rs:98-127` (`EditConsoleEvent` has no
browser id), `apps/buffr-app/src/main.rs:4700-4717` (drain applies every `Focus`
to the active engine + Insert).

`drain_edit_focus_events` (main.rs:4664) runs unconditionally every tick
(event*loop.rs:1321) and, for a `Focus`, calls `run_edit_attach(&field_id)` on
the active engine with a field id from \_another* tab's DOM. The event carries
no browser id, so attribution is impossible.

```
Repro: Ctrl+click / F-hint background-open a link to a page that autofocuses an input
Expect: active tab stays in Normal mode
Actual: the background tab's Focus puts the ACTIVE tab into Insert (keys pass
       through to the page; last_focused_field points at the background tab's element)
```

Same path for popup-window focus events and for a hidden tab that focuses a
field while not active.

### 2 MEDIUM — middle-click closing the last tab exits without saving the session or marking a clean shutdown

**Where:** `apps/buffr-app/src/event_loop.rs:894-902` vs the sibling paths
`main.rs:2685-2703` (keyboard) and `context_menu.rs:638-644` — both call
`save_session_now` + `mark_clean_shutdown` + set `shutdown_flag` before
`event_loop.exit()`; the middle-click path calls only `event_loop.exit()`.

```
Repro: 3 tabs, middle-click each tab in the strip to close them all
Expect: graceful exit — session saved, crash_guard launch.json cleared
Actual: session.json keeps the stale list (restores the just-closed tab on next
       launch); crash_guard never cleared, so two such exits within 60 s plus a
       third launch trips LOOP_THRESHOLD (crash_guard.rs:69) and quarantines a
       graceful session as a "crash loop"
```

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

### 4 MEDIUM — closing a tab does not stop its media; the stashed browser keeps playing until stack eviction

**Where:** `crates/buffr-cef/src/host.rs:1686-1720` (`close_index` stashable
branch only calls `was_hidden(1)` + `set_focus(0)`; `close_browser` deferred to
eviction), and the note at `host.rs:1531-1535` that `was_hidden(1)` does _not_
cut audio. `any_audio_active` (host.rs:1569-1571) stays true.

```
Repro: play a song, `d` (close tab)
Expect: playback stops
Actual: the hidden "closed" browser keeps playing; statusline media indicator and
       idle-inhibit stay engaged until CLOSED_STACK_CAP more tabs are closed or
       the app exits; "reopen tab" resurrects a page that played on its own
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

### 7 MEDIUM — Unix: SIGINT/SIGTERM during the restart cooldown is ignored; a fresh child spawns after the user asked to quit, and the second signal orphans it

**Where:** `apps/buffr/src/main.rs:624-630` (shutdown check only after a child
outcome), `669` (250 ms `RESTART_COOLDOWN` sleep), `474-516` (loop top re-binds
and spawns with no shutdown check), `855-866` (single-shot signal thread —
`signals.forever().next()` then the thread exits and drops the `Signals`,
unregistering the handlers). `child_pid_slot` is cleared to 0 at 571, so during
the cooldown the handler logs "no live child" and returns.

```
Repro: Unix; child crashes; Ctrl+C lands inside the 250 ms cooldown window
Expect: supervisor stops without spawning
Actual: the loop wakes and spawns a fresh browser (setsid session leader,
       main.rs:816-822); a second Ctrl+C now hits the default disposition,
       killing the supervisor and leaving the new browser running orphaned and
       unsupervised
```

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

### 11 LOW — Netscape importer: `<H3>`/`</DL>` markup inside an anchor label desyncs the folder stack

**Where:** `crates/buffr-bookmarks/src/lib.rs:401-403` (independent regexes),
`414-429` (byte-position token sort), `442-448` (push/pop).

```
Repro: <A HREF="https://ok.example/">lbl <H3>x</H3></A><DT><A HREF="https://next.example/">Next</A>
Expect: next.example carries no spurious tag
Actual: "x" is pushed as a folder and never popped (no </DL> belongs to it);
       every later anchor is tagged "x" and the file's real </DL>s pop one level
       too early. Fires only on malformed/hostile input — real Chrome/Firefox
       exports escape `<` as &lt; — and the fuzzer can't catch it (nothing
       asserts on tags).
```

### 14 LOW — hint `Ready` events are single-slot and untagged; a tab switch before the drain misroutes them

**Where:** `crates/buffr-cef/src/host.rs:2247-2283` (`enter_hint_mode`),
`handlers.rs:1095-1100` (single-slot `HintEventSink`, no browser id),
`host.rs:2287-2308` (`pump_hint_events` applies the slot to the _currently
active_ tab).

```
Repro: press `f` on A, switch to B before A's Ready round-trips through the
       CEF message loop and is drained
Expect: A's hint session receives A's hints
Actual: A's Ready is applied to B — dropped if B has no session (A's hint mode
       then dies: next key → Cancel), or B's hint list is replaced with A's
       element ids (next hint key clicks the wrong element)
```

### Cleared

Verified by re-reading the cited lines (✓) or traced by the sub-agent during the
same pass (the rest):

- ✓ Hint label generator is prefix-free — BFS pushes a parent's children to the
  tail before advancing `offset`, so a parent is never in the candidate slice
  with its own child (`hint.rs:173-226`).
- ✓ IME UTF-16 selection range — byte→UTF-16 with clamping and reversed-pair
  normalisation (`host.rs:2911-2934`).
- ✓ `paint_buffer_len` / OSR FFI trust boundary — null buffer and non-positive
  dims rejected before any pointer use; overflow-checked (`osr.rs:311-319`).
- ✓ `render_spans` — every slice goes through `str::get`; out-of-order,
  inverted, past-end and non-boundary spans are skipped without losing
  surrounding text (`view_source lib.rs:147-187`).
- ✓ Favicon BGRA→`0xAARRGGBB` packing — explicit pack matches the documented
  compositor layout (`handlers.rs:1221-1231`).
- Permission callback exactly-once — `take()` in `resolve`, `disarm()` on CEF
  dismissal, `Drop` fires `cancel`/`DISMISS`; all arms covered
  (`permissions.rs:181-318`).
- Two `window.open()` in one task (FIFO alloc) — front-of-queue consumption
  correct (M16); the residual stale-alloc URL reuse inside the 2 s TTL after an
  _aborted_ popup is cosmetic (URL bar only).
- `__buffrUserGesture` vestigial flag — written but never read in-page; no
  behavioral effect.
- `feed_hint_key` background-commit fallback — `OpenInBackground` committing as
  a same-tab click is a documented, logged limitation (`host.rs:2323-2331`).
- Tab-strip hit test — `i32::saturating_sub` saturates at `i32::MIN`, not 0, so
  negative region-relative y survives and the reconstruction is exact (the
  initial suspicion was wrong).
- `scroll_to_cef_delta`'s discarded `is_pixel` flag — the distinction is encoded
  in the delta magnitude (×10 vs ×120) before the flag is dropped.
- Winit pointer units — physical pixels throughout; no unit mismatch.
- OSR freshness gate / scratch swap / `chrome_upload_bands` / `ModalPanel`
  centring / swipe detector — traced, self-consistent, edge cases unit-tested.
- Single-instance socket — flock + atomic rename + peer-cred check hold up; the
  reject-path write is 4 bytes and can't block.
- `truncate_to_width` — walks `char_indices` + `len_utf8`, never byte-slices
  mid-codepoint.
- `blit_favicon` OOB/wrap — `checked_mul` + len guard, clamped Q16 sampling.
- `classify` precedence (supervisor) — clean-flag overrides signal exit; hang
  always restarts; non-zero normal exit propagates; all covered by tests.
- History FTS5 quoting — bare `"`, `""""`, `"*"`, `OR AND` needles all return
  `Ok`/0 rows from SQLite 3.53; `""` doubling + wrapping is the correct escape.
- Bookmarks LIKE escaping — `escape_like` pairs with `ESCAPE '\'`; trailing
  backslash and literal `50%`/`a_b` match literally.
- Migration runner — `TooNew` check precedes all writes beyond the no-op
  `CREATE TABLE IF NOT EXISTS`.
- Downloads terminal-freeze / idempotent `record_started` — `update_progress`
  guarded by `status='in_flight'`; double `OnBeforeDownload` reuses the row id.
- Fuzz targets — no reachable panic in `parse_action`, `parse_keys`,
  `import_netscape`, config round-trip; harnesses sound.

**Correction to a sub-agent's Cleared claim:** one agent listed the
`close_index` active-index math as cleared. It is not — I re-traced
`close_index(0)` with `active = Some(2)` on [A, B, C]: `set_active_index(0)`'s
guard `prev < tabs.len()` is `2 < 2` → false, so the old active (C) is never
hidden. §10's HIGH finding remains open in the current tree; it was not re-filed
above.

### Hardening

- `close_index`'s stashable branch and `set_active_index`'s hide-previous guard
  interact exactly as §10 item 1 describes — the highest-value fix on this list.
- `internal_server` accepts a request with no `Host` header — defence-weakening,
  not a correctness bug; left for the audit pass.
- `flatten_top_level` in xtask returns `Ok` on multiple matches despite a
  comment saying bail — benign today (real Spotify archive has exactly one
  `cef_binary_*` dir; sha1 gate precedes extraction); worth a comment fix.

### Coverage

- **Browser core** (cef/core/modal/view-source/helper): all read; deep-trace
  coverage skipped
  `crates/buffr-core/src/{crash.rs, telemetry.rs, updates.rs, favicon_cache.rs, inhibit/*}`,
  `cmdline.rs` 100-249, and the head of `crates/buffr-cef/build.rs`.
- **Main app** (apps/buffr-app): read in full except
  `windowing/other/{cursor,ime,output,surface,window}.rs` (re-export shims) and
  `art.txt`.
- **Engine/ui/supervisor**: every `.rs` read (10,866 lines). The
  `#[cfg(windows)]` supervisor module (main.rs:962-1710) was read but cannot be
  executed here — finding 6 is structural control-flow analysis, not
  runtime-verified.
- **Data/tooling**: all crates read; `tests/e2e/pages/*.html` fixtures not read
  beyond `README.md` + `e2e.js` (static inputs to the out-of-scope focus/Insert
  logic). The sub-agent ran the in-scope suites (store 14, zoom 15, permissions
  7, downloads 11, history 33, bookmarks 18, config 104, xtask 33 — all passed);
  the full workspace gate was not run by the pass.
- **Known drift found:** `tests/e2e/pages/README.md` contradicts
  `expectations.tsv` on `autofocus.html` and `shadow_closed.html` (documentation
  drift in the suite, not a runner defect).

---

## 12. Audit 2026-08-05 (8744e18) — findings

Security + correctness audit, low depth, clean tree, same four partitions as
§11. Every finding below was re-traced at its cited lines. **5 medium, 8 low — 0
critical, 0 high.** Overall risk: low-to-moderate; the hard external boundaries
(IPC socket, navigation input, permission prompts, pixel upload, internal-server
token) held up, and the residual risk concentrates in the **page→app console
bridge** (clipboard + Insert-mode) and the **browser-process fetch primitives**
(buffr-src / Copy Image).

### 1 MEDIUM — `buffr-src:` and Copy Image private-network guard classifies hostname strings, never resolutions; `127.0.0.1.nip.io`-class wildcard DNS bypasses it

**Where:** `crates/buffr-cef/src/view_source_scheme.rs:150`
(`if !is_non_public_host(&host) { return Ok(()) }`),
`crates/buffr-core/src/private_net.rs:10-77` (`is_non_public_host` never
resolves DNS; `:75` explicitly defers "DNS rebinding"),
`view_source_scheme.rs:498` (ureq GET in the browser process),
`crates/buffr-core/src/copy_image.rs:113-131` (`check_fetch_host`, same guard).

```
Repro: a page sets location.href = 'buffr-src:http://127.0.0.1.nip.io:8080/admin'
       (scheme is STANDARD|SECURE; buffr-src has no CORS/fetch but top-level
       navigation is the M13-intended path)
Expect: private-network fetch refused
Actual: "127.0.0.1.nip.io" is not localhost/.local, not an IP literal, not
       numeric-looking → classified public → browser-process GET resolves it to
       127.0.0.1 and renders the local service's response in the tab
```

No timing games needed — nip.io is a static resolution; the code's "DNS
rebinding out of scope" note (private_net.rs:75) is not a boundary. Same guard,
same bypass, in the Copy Image path (right-click → user-gesture-gated, lower
impact). Fix: resolve-and-classify the resolved IPs, or reject the wildcard-DNS
class. **Caveat:** reachability of the `buffr-src:` leg depends on CEF
permitting content-initiated top-level navigation to a STANDARD|SECURE custom
scheme — nothing in the handler enforces the "browser-initiated only" property
the M13 comment asserts, and the M13 fix is untested at runtime (backlog §2).

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
hijack impact being accepted without a security note. Same root as §11 item 1
(cross-tab misattribution) — this is the same-tab variant. Fix: implement the
gesture gate or delete the field + doc and document the tradeoff.

### 4 MEDIUM — profile data stores are created world-readable; any local user can read history, bookmarks, permission grants, session

**Where:** `crates/buffr-store/src/lib.rs:58-61` (SQLite `READ_WRITE | CREATE` —
file created `0666 & ~umask`, typically `0644`; `tune()` at 79-84 sets no mode),
`apps/buffr-app/src/main.rs:1245-1246` (`create_dir_all` →
`~/.local/share/buffr`, `~/.cache/buffr` at `0755`),
`apps/buffr-app/src/session.rs:154` (`fs::write` → `0644`). The supervisor
already shows the right standard — `ensure_private_dir` enforces 0700 + uid +
symlink-reject (`apps/buffr/src/main.rs:346-372`) — but it is never applied to
the XDG profile dirs.

```
Repro: multi-user box, default umask 022; user A browses; user B lists
       ~A/.local/share/buffr/
Expect: A's history, bookmarks, permissions (camera/mic/geolocation grants),
       session.json and CEF cookie/cache trees are unreadable by B
Actual: all readable (0644/0755); full browsing history + permission state
       disclosed to any other local user
```

Umask-077 machines are immune. Fix: chmod profile dirs 0700 / files 0600 at
`resolve_paths`/`open_tuned` (or SQLITE_OPEN_NOFOLLOW + explicit mode).

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

### 10 LOW — unbounded popup-window creation; no app-side cap on live popups

**Where:** `apps/buffr-app/src/event_loop.rs:1563-1645` (every `PopupCreated`
spawns a winit Toplevel + wgpu Renderer + CEF browser; `self.popups` never
capped), `crates/buffr-cef/src/handlers.rs:240-330` (every NEW_POPUP/NEW_WINDOW
disposition routed to a real window; `_user_gesture` at 248 ignored),
`crates/buffr-cef/src/osr.rs:88-94` (`PENDING_POPUP_ALLOC_CAP = 32` bounds only
the pre-`on_after_created` queue).

```
Repro: a page evades CEF's popup blocker (gesture-triggered chain, popunder)
Expect: a cap on live popup windows
Actual: unbounded windows/GPU/fds until the process degrades
```

Relies entirely on the engine's popup blocker today. Fix: cap `self.popups`
(match the queue cap) or gate on `_user_gesture`.

### 11 LOW — `import_netscape` has quadratic tag amplification: a sub-MB hostile bookmark file can hang import and bloat the store to 10⁸ rows

**Where:** `crates/buffr-bookmarks/src/lib.rs:478-481` (per anchor, every
ancestor folder cloned into `tags`, no depth cap), `639-644` (one
`INSERT OR IGNORE` per tag inside the single import transaction),
`apps/buffr-app/src/cli.rs:54-58` (import entry, file read unbounded).

```
Repro: ~800 KB file: 10⁴ nested <H3><DL> levels then 10⁴ anchors
Expect: import work linear in file size
Actual: 10⁴ tags × 10⁴ anchors = 10⁸ INSERTs in one transaction — tens of
       minutes of CPU, GB-scale WAL growth, and a permanently bloated store
       (every bookmark carries 10⁴ tags, slowing all later search/all)
```

Fix: cap folder depth / per-anchor tag count so work is linear in input.

### 12 LOW (dev-only) — xtask CEF trust chain: SHA-1 protects against corruption only, and the download/decompress path is unbounded

**Where:** `xtask/src/main.rs:460-493` (`verify_sha1` against the digest from
`index.json`, fetched over the _same_ TLS channel as the archive at 216-272 — a
hostile CDN can publish both), `381-408` (download, no size cap), `525-556`
(`extract_tar_bz2`, bzip2 with no output bound).

```
Repro: compromised Spotify CDN serves a matching-hash decompression bomb, or
       arbitrary libcef.so
Expect: an independent root of trust, or at least size bounds
Actual: the hash catches truncation/swapped downloads only (exactly what the
       comment at 453-459 claims); a hostile CDN can ship arbitrary code into
       every built binary, or exhaust the dev box's disk
```

Inherent to the design (same-channel digest) — the finding is the size bounds.
`fetch-cef` is manually invoked, so exposure is the developer running it.

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

### Cleared

Re-verified by re-reading the cited lines (✓) or traced by the sub-agents (the
rest):

- ✓ `buffr-src:` script-injection / XSS — every interpolation is
  `serde_json`-escaped, V8-fed directly; `capture_to_class` uses only bundled
  grammar-query class names, all source text `html_escape`d.
- ✓ Hint-commit injection via `__buffrHintCommit({id})` — `element_id` is
  `u32`-typed by serde, the splice emits digits only.
- ✓ `on_favicon_urlchange` — goes through CEF's own network stack
  (`download_image`, favicon decode, 64 px cap), not the ureq path; not SSRF.
- ✓ `sanitise_filename` traversal — `Path::file_name()` strips `../`; `.`/`..`
  fall back to "download"; reserved stems pinned by tests.
- ✓ Zip-slip / tar traversal in xtask — `validate_archive_name` (419-451) +
  `tar_path_is_safe` (511-523) + the tar crate's `unpack_in` canonicalisation
  (parents + link targets) with `Ok(false)` bailed on.
- ✓ SQL injection (all stores) — every query parameterized; bookmarks LIKE
  `ESCAPE '\'` + `escape_like`; history FTS needle `"…"`-quoted with `"`
  doubled; `load_tags_bulk` chunks at 500.
- ✓ Netscape importer panic paths — byte offsets char-aligned, entity bodies
  capped at 12 bytes, all regexes linear-time. (The §11 folder-stack desync is
  data-integrity only: `pop()` on an empty stack is a no-op, nothing reaches fs
  or code.)
- ✓ Internal-server request caps — 32 KiB request line → 414, 16 KiB header
  block → 413, 32 inflight → 503, enforced _while_ reading; no OOM path.
- ✓ Internal-server injection / confused deputy — request path/query/headers
  never reach handlers (they take no args); GET-only; token CSPRNG 128-bit,
  constant-time compare; exact-authority Host check incl. port.
- ✓ Unix supervisor file hygiene — socket 0600 in a 0700 per-uid dir verified by
  symlink_metadata + uid + mode; clean flag requires a regular file owned by us;
  stale-socket unlink-before-bind safe.
- ✓ IPC payloads — line cap, 100-URL / 1 KB-URL caps, scheme allow-list, serde
  recursion limit — all present and tested; `javascript:`/`data:` rejected.
- ✓ `resolve_input` / omnibar / paste / SearchSelection — `javascript:`/`data:`
  map to a search query; no navigation injection.
- ✓ Permission prompt — `resolve_permission` matches the prompt id against the
  queue front; stale answers discarded.
- ✓ OSR pixel upload — `is_osr_frame_fresh` enforces `pixels_len == w*h*4`
  before `write_texture`.
- ✓ New-tab HTML — keymap/splash substitutions escaped or static; splash JS push
  JSON-escaped and gated to `buffr://new`.
- ✓ Crash-guard/session writes — tmp+rename atomic and symlink-safe; corrupt
  launch.json degrades to fresh; quarantine is rename-aside, not delete.
- ✓ Ctrl+V insertText — `serde_json::to_string`-escaped before `execCommand`.
- ✓ `data:` URLs in image_copy — bounded decode (`IMAGE_FETCH_MAX_BYTES`), no
  network; `blob:` rejected.
- ✓ buffr-ui — no HTML/JS built anywhere; page-derived strings render as
  rasterized glyphs with bounds-checked blits; favicon `checked_mul` vs
  `pixels.len()`.
- ✓ media_js — coords and image URL `serde_json`-encoded before splicing.
- ✓ Permission/popup queues — poisoned locks degrade to empty; answers applied
  to the exact entry shown.
- ✓ Fuzz harnesses — all five targets parse-and-discard, no assertions in the
  fuzz path, fresh in-memory store per netscape input.

### Hardening

- `password-store=basic` — cookies/passwords plaintext in the profile dir;
  deliberate (app.rs:153-161) but worth a config escape hatch to a real keyring.
- Watcher callback mutex poisoning (known — still open); downloads `u64 → i64`
  casts round-trip losslessly today but any future `ORDER BY` on them would see
  negatives.
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
- §11's `close_index` HIGH remains the top correctness item; the page→app bridge
  gate (fix for findings 2+3) is the top security item.

### Coverage

- **Browser core:** every CEF callback class traced (console, load,
  context-menu, permission, audio, popup, download, find, favicon, cursor,
  paint, buffr-src handler); every JS asset read. GAPs:
  `crates/buffr-cef/build.rs` not read; `inhibit/{macos,windows,wayland}.rs`
  contract-level only; apps-layer dispatch-side impact of the §11 attribution
  items assessed only to the crate boundary.
- **Main app:** cli, single_instance, crash_guard, session, engine_router,
  context_menu, heartbeat, cef_translate, paint_policy, chrome_paint,
  event_loop, main (startup/restore/omnibar/edit-bridge/permissions/shutdown),
  render unsafe/pixel paths. GAP:
  `windowing/other/{ime,keyboard,pointer,surface,window, geometry,output,cursor,event}.rs`
  skimmed via grep only; wgpu shader/format handling not line-audited.
- **Engine/ui/supervisor:** all 21 engine + 8 ui + 2 supervisor files read,
  every checklist class walked. GAP: internal-page HTML assembly (keybind/splash
  substitution) lives in apps/buffr-app — the real XSS surface for `buffr://new`
  — outside the assigned paths, not examined here.
- **Data/tooling:** all store crates, config, xtask (all 2761 lines), 5 fuzz
  targets, e2e scripts read. GAPs: (a) buffr-core console parsers and CEF
  download/zoom/permission handlers feeding the stores are out of scope — their
  page-controlled strings reach the stores via recorders not audited here; (b)
  Windows/macOS-only xtask paths (MSI/DMG) not compiled here; (c) Chromium's own
  perms for the CEF cache/LocalStorage subdirectories not verified — finding 4
  is scoped to buffr-created files.
- Windows-gated code (supervisor pipes, single-instance IPC, inhibit) is
  structurally traced but never executed on this Linux host — consistent with
  §2's note that no Windows test job exists.

**Summary:** 0 critical, 0 high, 5 medium, 8 low. Overall risk is
low-to-moderate. Top fixes in order: (1) gate the page→app bridge — a real
gesture/Visual-mode gate on the `Selection` clipboard write and the
`Focus`→Insert transition (kills findings 2+3, same root: the console bridge
treats page input as trusted); (2) resolve-and-classify in the private-network
guard (finding 1) or reject the wildcard-DNS class; (3) chmod the profile dirs
0700/files 0600 (finding 4) — the only cross-user data exposure; (4) the Windows
pipe name/DACL (finding 5).

---

## 13. Tidy 2026-08-05 (8744e18) — cleanups

Quality pass (behavior-preserving cleanups only) on the same four partitions.
Every candidate below was verified against its call sites after the sub-agent
pass (`rg` across the whole workspace — a `pub` item in a lib crate is not
linter-flagged as dead, so grep is the only oracle). Ranked by size of win.

### Dead code (delete)

- **`crates/buffr-engine/src/media_js.rs` — the whole module is dead (99
  lines).** Its six fns (`play_pause`, `toggle_mute`, `toggle_loop`,
  `toggle_controls`, `picture_in_picture`, `copy_image_url`) have zero callers;
  the live paths are the `BrowserEngine` trait methods (separate stubs,
  engine.rs:410) implemented per-backend. Delete the file + `pub mod media_js;`
  (engine lib.rs:54).
- **Four dead `pub` methods on `BrowserHost`** (host.rs): `run_edit_apply`
  (:2435, no trait counterpart), `print_active` (:2633),
  `reload_ignore_cache_active` (:2628), `frame_del` (:2614) — zero call sites in
  the workspace.
- **`BrowserHost::new`** (host.rs:430-463) — 12-arg constructor forwarding
  verbatim to `new_with_options`; zero callers. Delete, keep `new_with_options`.
- **`make_client`** (handlers.rs:154-220) — 33-arg pass-through with a single
  caller (host.rs:1316) forwarding every arg unchanged to the macro-generated
  `BuffrClient::new`. Delete `make_client`, call `BuffrClient::new` directly.
- **`insert_intent_at` dead field** (main.rs:1594-1599, init :2115, writes
  :2549 + event_loop.rs:958, clear :4701) — write-only; the any-Focus-enters-
  Insert behavior it was meant to gate is deliberate (audit §12-3). Delete the
  field + both writes + the stale doc comment.
- **`Mode` enum** (buffr-modal/src/actions.rs:20-28) — never constructed or
  read; delete + its re-export (buffr-modal lib.rs:53, buffr-ui lib.rs:48).
- **`PendingPopupAlloc` + `new_pending_popup_alloc`**
  (buffr-engine/src/popup.rs:62, 76-78) — replaced by buffr-cef's own queue
  (osr.rs:72 comment confirms); only re-exports remain (engine lib.rs:78, cef
  lib.rs:91).
- **`TabOptions` / `TabSession` in buffr-engine** (tab.rs:15-30) — only
  references are the crate's own tests and the re-export (lib.rs:83); buffr-cef
  defines its own `TabSession` (host.rs:75). Delete both + the re-export names.
- **`pop_front` / `peek_front`** (buffr-engine/src/permissions.rs:88-100) plus
  the four dead alias re-exports `new_permissions_queue` /
  `peek_permission_front` / `pop_permission_front` / `permissions_queue_len`
  (engine lib.rs:73-75) — apps uses
  `peek_front_entry`/`take_front_matching`/`queue_len`; the two fns survive only
  in their own in-file tests (delete the test usages too).
- **`InternalServer::set_routes`** (internal_server.rs:241-245) — zero callers,
  not even tests (its doc claims "tests use this" — they don't).
- **`ContextMenuOverlay::row_at`** (buffr-ui context_menu.rs:255-257) and
  **`InputBar::paint`** (input_bar.rs:310-312) — zero callers; apps use
  `hit_test` and `paint_at`.
- **`KeyChord::new`** (buffr-modal/src/key.rs:56), **`Keymap::leader`**
  (keymap.rs:100), **`HintAlphabet::is_empty`** (buffr-core/src/hint.rs:129 —
  can never return true by construction) — zero callers.
- **`Engine::count`** (buffr-modal/src/engine.rs:151) — used only in its own
  tests; `count_buffer` is the live accessor (main.rs:3026).
- **`BuffrLoadHandler.edit_sink`** (handlers.rs:649, 542) — write-only; its only
  "use" is `let _ = &self.edit_sink;` (799). Remove field + constructor arg +
  the no-op line.
- **windowing dead accessors** (apps/buffr-app, linter-invisible behind
  `#[allow(dead_code)] mod windowing;`): `SurfaceId::as_u64` (surface.rs:20),
  `OutputId::as_u64` (output.rs:17), `OutputInfo.description` (output.rs:30 —
  always written `None`), `Position::ZERO` (geometry.rs:16). Leave `Rect`
  (documented deliberate wayr shape-parity, main.rs:196-199).
- **Dead computation in paint path** — `host_is_loading` (main.rs:3520-3523,
  discarded at :3596) and the `browser_id` destructure in `pump_cursor_changes`
  (main.rs:3206, :3216 — a comment claims it's "logged"; it isn't). Delete both.
- **`_phantom: PhantomData<T>`** (windowing/other/event_loop.rs:146, init :166)
  — `T` is already used by `inner` and `proxy`; delete field + init + the stale
  doc ("Stash any `Window`s…" describes nothing that exists).
- **Three unreachable `"space"` mapping arms** — each shadowed by an earlier
  check in the same chain: `parse_named_key` (buffr-modal key.rs:262;
  `parse_named` maps `"space"` to `Char(' ')` at :195 first), `map_named`
  (adapter.rs:57; `chord_from_parts` handles it at :125), `WNamed::Space`
  (winit_adapter.rs:129; `chord_from_logical` returns early at :90). Keep the
  `NamedKey::Space` variant itself (matched at main.rs:4443).
- **`fuzz/fuzz_targets/fuzz_target_keys.rs:7-14`** — a no-op loop
  (`for chord in &chords { let _ = chord; }`) whose comment describes a
  discarded approach. Drop the loop + comment; keep `parse_keys`.
- **`cef_minimal_url`** (xtask:302) — test-only, lives in prod code under
  `cfg_attr(not(test), allow(dead_code))`; move into the tests module.

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
  2058-2061, 2065-2068, 2070-2073, 2077-2080, 2084-2087, 2090-2093, each `let
  deadline = match self.X { Some(at) if at < deadline => at, * => deadline
  }`→`let deadline = deadline.min(self.X.unwrap_or(deadline))`.
- **`key_to_neutral_events`** (cef_translate.rs:395-431) — three near-identical
  `NeutralKeyEvent` literals; `NeutralKeyEvent` is not `#[non_exhaustive]`
  (buffr-engine input.rs:38), so build one base literal and use struct-update
  `..base`. ~24 lines → ~10.
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
- **`host_deb_arch` / `host_rpm_arch` / `host_msi_arch`** (xtask:310-342) —
  identical `cfg!(target_arch)` chains differing only in the returned token. One
  `fn host_arch_token(x86_64, aarch64) -> &'static str`.
- **`cargo build -p buffr -p buffr-app -p buffr-helper` block** (xtask:664-682
  vs 1044-1059) — copy-pasted; extract
  `build_binaries(workspace, release, target)`. The
  `if release { "release" } else { "debug" }` profile string also repeats at
  658/1028/1532/1811.
- **`copy_file_executable`** (xtask:946-958) inlines what `set_executable`
  (1238-1251) already does — call it.
- **`mode_name(PageMode)` duplicated in one crate** — config lib.rs:1006-1014
  and keybinding.rs:300-308. Make one `pub(crate)`.
- **host-head extraction** — search.rs:155-163 (`looks_like_url`) and 196-199
  (`needs_http`) each re-implement `split(['/', '?', '#']).next()` +
  `rsplit_once(':')` port-strip, both with a dead `.unwrap_or("")`. Extract
  `fn host_head(s) -> &str`. Pure extraction only (the port>65535 finding §11-12
  is separate).

### YAGNI / over-abstraction

- **`deserialize_keymap`** (config lib.rs:70, 657-664) — pure pass-through whose
  doc admits it exists "so we can later add a normalization pass". That pass
  never arrived; the derived deserializer behaves identically. Delete the
  attribute + the fn.
- **`Keymap::audit_default_bindings(_leader)`** (buffr-modal keymap.rs:227) —
  dead parameter (named `_leader`), and the doc claims it "renders the resolved
  `<leader>` chord" which the code does not do. Drop the param, the caller-side
  leader resolution (cli.rs:375-389), and fix the doc (keymap.rs +
  docs/site/keymap.md).
- **`buffr_cef::new_tab` re-export module** (cef lib.rs:82-85) — no users; apps
  import from `buffr_engine::newtab` (main.rs:156-161). The file's own comments
  frame it as a deliberate Phase-6e compat shim — owner's call whether the shim
  still earns its keep.
- **Over-exposed pub API with no in-workspace users** — buffr-ui constants
  `FAVICON_RENDER_SIZE` (tab_strip.rs:67), `SUGGESTION_ROW_HEIGHT`
  (input_bar.rs:41), `PERMISSIONS_PROMPT_HEIGHT` (permissions_prompt.rs:23),
  `CONTEXT_MENU_MIN_WIDTH` (context_menu.rs:28), `ACTION_HINT`
  (permissions_prompt.rs:136) used only inside buffr-ui but exposed via the
  `pub use` block (lib.rs:25-39); and `pub use raw_window_handle;` (engine
  lib.rs:89) with zero users. Drop the re-exports, keep the items.
- **Never-constructed enum variants** (flag, not action — documented future
  surface): `MediaType::Canvas/File/Plugin` (buffr-engine types.rs:83-85),
  `CertState::Secure/Insecure` (buffr-ui lib.rs:54-55).
- **`Statusline::progress`** (buffr-ui lib.rs:101, paint block :289-305) —
  write-only today (stays 1.0, bar never renders); documented Phase-3
  placeholder for CEF's `OnLoadingProgressChange`. Lower confidence: deleting is
  behavior-preserving but removes a planned-feature placeholder — wire it or
  delete it, don't leave the write-only field.

### Minor

- **`tab_strip.rs:268-269`** — `two_char_px`/`badge_content_w` recomputed per
  tab per frame inside the paint loop but depend only on constants; hoist.
- `set_min_size`/`set_max_size` near-identical
  (windowing/other/window.rs:36-57); `run_check_for_updates`/`run_update_status`
  differ in one call (cli.rs:347-363); `cef_cursor_to_icon` if-else chain →
  `match` (cef_translate.rs:101-176); `omnibar_suggestions` history/bookmark
  loops share a dedup+display+cap body (main.rs:4303-4339); `const GUTTER`
  declared inside a fn body (paint_policy.rs:326) — hoist to module scope so the
  five test-local `const GUTTER: u32 = 4;` re-declarations (main.rs:6669-6731)
  can reference it.

### Known items — confirmed, with verdicts

- **`__buffrUserGesture` in edit.js is write-only** — confirmed. Written by
  `markGesture` + the mousedown/pointerdown/touchstart listeners (edit.js:75-79)
  and by Rust (host.rs:2173, 2452; buffr-webkit out of scope); nothing reads it
  in-page (the blur gate it documented was removed — edit.js:252-257). Delete
  the flag + `markGesture` + the three listeners + their teardown lines
  (edit.js:348-350) + the four Rust writers.
- **`DEFAULT_SKIP_SCHEMES` duplicated across crates** (config lib.rs:453 vs
  history lib.rs:49) — same 5-element list, no cross-check, and the runtime
  always overrides the history default with the config value (main.rs:505,511),
  so the history copy can silently drift. Fixing it "properly" would need a
  dependency in the wrong direction — the cheap honest fix is a sync test like
  the existing `default_hint_alphabet_matches_core` precedent.
- **Store-crate shape duplication** (schema-error wrappers, open/open_in_memory
  pairs, DELETE+VACUUM clear_all across the five stores) — verdict: leave the
  error-enum + open-pair shape (deliberate M-hardening; collapsing needs a macro
  or trait for ~12 saved lines). The `clear_all`+VACUUM trio is the one piece a
  small `buffr_store::delete_all(conn, table)` would cleanly absorb —
  borderline, owner's call.
- **`drain_edit_focus_events` vs the test `drain_into` mirror**
  (main.rs:6407-6474) — verdict: leave. The test double is a deliberately
  simplified FSM (documented mirror); driving the real function needs a full
  `AppState`.
- §5 backlog items (`crate::*` globs, `mod tests` placement,
  `wayr_key_to_planned` prefix) not re-reported.

### Coverage

- **Browser core** (cef/core/modal/view-source/helper): every line read, incl.
  `inhibit/*`; platform-gated `inhibit/macos.rs`/`windows.rs` read but not
  compiled here.
- **Main app** (apps/buffr-app): every `.rs` read in full (incl. windowing/);
  candidates verified via `rg` against call sites.
- **Engine/ui/supervisor**: all 26 in-scope files read; `apps/buffr/tests/`
  (integration tests) not read. Supervisor's restart/Windows-pipe machinery left
  alone (open findings §11/§12).
- **Data/tooling**: all store crates, config, xtask (2761 lines), 5 fuzz
  targets, e2e scripts read; the 22 `tests/e2e/pages/*.html` fixtures not read
  (data, not harness).

---

## 14. Performance review 2026-08-05 (8744e18) — findings

Perf pass on the same four partitions. Every finding below was re-verified at
its cited lines after the sub-agent pass. No O(n²) hot loops, no per-item I/O,
no syscalls-in-loops, no lock-across-await anywhere; the waste is steady-state
per-frame chrome work and a few per-interaction paths. Estimates are from
allocation/lock counts, not measurements — see Coverage.

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

### 6. Downloads `OnDownloadUpdated` tick: full-row hydrate + two round-trips per tick

**Where:** `crates/buffr-downloads/src/lib.rs:303-315` (`get_by_cef_id` SELECTs
all 12 columns; `row_to_download` allocates 5 Strings + a DateTime per tick
purely to read `id`/`status`) and `223-244` (`update_progress` — a second,
autocommit WAL write on every tick even when `received_bytes` is unchanged).
Caller: `crates/buffr-cef/src/handlers.rs:1337-1438` (`on_download_updated`,
fired periodically for the whole life of every in-flight download).

**Fix:** narrow the per-tick SELECT to `id, status, received_bytes` (or key a
handler-side `cef_id → row_id` map and skip the read), and skip the `UPDATE`
when the bytes are unchanged.

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

### 9. Loop-invariant constant recomputed per tab per frame — flagged by BOTH the tidy and perf passes

**Where:** `crates/buffr-ui/src/tab_strip.rs:268` —
`two_char_px = font::text_width("WW")` sits inside the per-tab loop's badge
branch, so every tab re-runs 2 glyph lookups for a value identical across the
whole frame (and every frame). Also `badge_content_w` (269).

**Fix:** hoist above the loop. (Cross-cutting: this exact item is also §13
tidy.)

### 10. `open_pending_tabs` re-materializes the full tab list per restored tab — O(M×N) at startup

**Where:** `apps/buffr-app/src/main.rs:2846-2904` — the restore loop calls
`host.tabs_summary()` (each O(N) with per-tab locks + clones) just to read the
last entry's `browser_id` (2869, 2891, 2900).

**Fix:** an accessor for "the tab that was just opened" — the open calls already
return the new `TabId`; expose `browser_id_of(id)` (one lookup) or capture the
summary from the `open_tab` return. Startup-only, ~1-10 ms for a large session.

### 11. Two nonce-table lookups (mutex + String clone each) per sentinel console line

**Where:** `crates/buffr-cef/src/handlers.rs:1095, 1112` — any sentinel-prefixed
line pays both `console_nonces.hint(...)` and `.page(...)`, each a lock +
entry/or-insert + 32-byte String clone (console_nonce.rs:186-200), regardless of
which subsystem the line belongs to.

**Fix:** gate each accessor behind
`text.starts_with(<that subsystem's sentinel>)` — the parsers already do that
prefix check internally, so the lock is pure waste on lines that cannot match.

### 12. `url_encode` allocates per escaped byte

**Where:** `crates/buffr-config/src/search.rs:254` —
`out.push_str(&format!("{b:02X}"))` inside the per-byte loop.

**Why hot:** `resolve_input` runs per omnibar keystroke (suggestion path) and
per submit. Queries are short; small absolute cost, constant fix.

**Fix:** push two chars from a `const HEX: &[u8; 16] = b"0123456789ABCDEF"`.

### 13. Omnibar submit resolves the input twice

**Where:** `apps/buffr-app/src/main.rs:5279-5282` — `classify_input(&raw)`
internally runs the full `resolve_input` (search.rs:47-70), then
`resolve_input(&raw, &self.search_config)` runs it again (two `url::Url` parses

- two string builds per submit; same pair in `paste_url` at main.rs:2536-2542).

**Fix:** a combined `resolve(&input, &search) -> (InputKind, String)` that
resolves once and derives the kind from the result (the logic already exists —
`classify_input` derives the kind by inspecting the resolved string).

### 14. Internal-server per-request waste: whole `Routes` table cloned per connection, String alloc per lookup

**Where:** `crates/buffr-engine/src/internal_server.rs:298-303` (per-connection
`routes.lock()` … `g.clone()` cloning every handler Arc + content-type String)
and `146-158` (`lookup` → `normalize_path` allocates a fresh String for the
HashMap key; paths already start with `/`).

**Why hot:** per navigation to `buffr://new`/`buffr://settings` — not per-frame,
but the clone is unnecessary per-request work.

**Fix:** Arc'd `RouteEntry`s and key lookup on `&str` without the normalize
alloc.

### 15. Context-menu geometry recomputed on every mouse move

**Where:** `crates/buffr-ui/src/context_menu.rs:97-105` (`preferred_width_for`
runs `text_width` per label) reached from `panel_rect_for` (83-95) on every
`contains_at`/`hit_test` while the menu is open (N ≤ ~10).

**Fix:** cache the panel width, recompute only when `entries` changes.

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

### Coverage

- **Browser core:** full trace of the CEF handlers (console/load/OSR/audio/
  downloads/popups), the IPC modules (edit/hint/media/console_nonce/sentinel),
  buffr-modal engine/keymap, the injected JS. Skimmed (one-off/startup, no hot
  loops): crash, cmdline, open_finder, image_copy, private_net, inhibit/\*,
  new_tab.html, exit_insert/focus_first_input/media_probe_init.js.
- **Main app:** the whole `about_to_wait` chain (event_loop.rs:1297-2108), the
  full paint path, key/pointer/scroll dispatch, cef_translate, engine_router,
  context_menu, paint_policy, session, single_instance, windowing bridge. Not
  settled without profiling: absolute µs of the per-tick resync; the
  CEF-internal cost of `pump_message_loop` per tick; the omnibar search's
  dominant cost (FTS MATCH vs GROUP BY) — inside the accepted ~1 ms.
- **Engine/ui/supervisor:** every file read; caller frequency established via
  the dirty-repaint gate (chrome repaints only on dirty frames). Not settled:
  ns-level cost of findings 1/2/7 needs a profiler; the child heartbeat cadence
  (out of scope). Supervisor heartbeat verified cold and bounded (`recv_timeout`
  slices, ≤3-entry `CrashWindow`).
- **Data/tooling:** all store crates, config, search traced with callers
  established (omnibar keystroke/submit, paste, downloads handler, zoom,
  bookmarks). Not settled: CEF `OnDownloadUpdated`'s real per-tick rate
  (Chromium throttles progress callbacks, ~1/s per download; finding 6's
  absolute cost scales with it). xtask/e2e/fuzz not traced (dev-only/cold).

---

## 15. Robustness audit 2026-08-05 — follow-ups from TODO.md

Migrated from `TODO.md` (tracked file, removed 2026-08-05). The audit verified
every item it raised; most turned out resolved, three still need a decision
before any code change. Line references were refreshed against the current tree
— several of the file's originals had drifted.

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

### Cleared — verified, no action needed

Recorded so they are not re-raised as findings.

- **`set_permissions` + `from_mode(0o600)`** (`apps/buffr/src/main.rs:694`) —
  `std::fs::Permissions::from_mode()` is the stable `PermissionsExt` extension
  (imported at main.rs:191), not an unstable API. Resolved.
- **`signal::kill(child_pid, None)`** (main.rs:877-878) — `None` performs a
  0-signal existence check; the `ESRCH` match is the correct way to detect a
  reaped process. Resolved.
- **`now_secs` epoch fallback** (`apps/buffr-app/src/crash_guard.rs:117-122`) —
  `unwrap_or(0)` returns 1970 timestamps only when the system clock predates the
  epoch; practically impossible on any real system. Resolved, low risk.
- **`VACUUM` failures logged at `warn!`** after `clear_all()`
  (`buffr-history lib.rs:428-429`, `buffr-downloads lib.rs:370-371`,
  `buffr-bookmarks lib.rs:355-356`) — intentional and documented: the DELETE
  already removed the data, VACUUM is storage hygiene. Resolved.

---

## Shipped 2026-08-05

Backlog items fixed this session, each one commit verified by the full workspace
gate (fmt --check, clippy --workspace -D warnings, build, nextest):

- **§10-1 HIGH** — close_index stale active index left the old active tab
  running as if foregrounded: `fe9ec53`.
- **§11-2** — middle-click last-tab exit skipped session save and clean
  shutdown: `c2f7e18` (shared `AppState::request_exit` across all four last-tab
  exit paths).
- **§12-4** — profile dirs world-readable: `1d4b44b` (0700 dirs incl. the CLI
  short-circuits, 0600 session file).
- **§12-1** — wildcard-DNS SSRF bypass in the fetch guards: `848a6fb`
  (resolve-and-classify on the fetch worker, never CEF's IO thread; the
  image_copy check was already off-thread).
- **§11-1** — edit events without browser attribution: `575a271` (tagged sink;
  drain drops non-active-browser events; webkit call site updated).
- **§11-4** — closed tab kept playing media until stack eviction: `bab8298`
  (pause + mute on stash, unmute on reopen).
- **§11-7** — signal during restart cooldown ignored → orphaned respawn:
  `ed46f3a` (loop-top shutdown check + persistent signal handler).

**New findings from the review pass over the §12-1 fix (open):** the string
guard's `is_non_public_v4` misses RFC 2544 benchmarking range `198.18.0.0/15`,
and the IPv6 path misses deprecated site-local `fec0::/10` — a hostname (or
literal) resolving to either is currently allowed. Pre-existing (both apply to
the literal path too); natural to close alongside the resolve-and-classify work
in `crates/buffr-core/src/private_net.rs`.

## 16. Code review 2026-08-06 (63b8a25) — findings

Full-codebase review pass; the 2026-08-04/05 findings (sections 10-15) were
treated as known and are not re-reported. Two new MEDIUMs, both in the
`buffr-src:`/Copy Image surface the §12-1 fix (`848a6fb`) touched. Both were
fixed on 2026-08-06 (`ac9b395`, `f630585`).

### Cleared

- `close_index` fixup (`fe9ec53`) — `active_after_close` traced for all four
  cases (close left of active, close active, stashable branch hides/pauses/mutes
  before re-select). Correct.
- `request_exit` extraction (`c2f7e18`) — all four last-tab paths run
  save+clean+flag+redraw; the middle-click path sums `tab_count()` across
  engines. Correct.
- Supervisor signal loop (`ed46f3a`) — loop-top `shutdown_requested` check
  before respawn; handler loops in `signals.forever()` so a second Ctrl+C is
  consumed after the child-wait. No orphan path.
- Edit attribution (`575a271`) — id spaces match on both backends: CEF pushes
  `cef::ImplBrowser::identifier(browser)`, `Tab.browser_id` is
  `t.browser.identifier()` (host.rs:1624); webkit pushes `ctx.tab_id.0 as i32`
  and its `TabSummary.browser_id` is the same (runtime.rs:98, 1351). Popup /
  background events dropped by the drain.
- Pause-on-stash (`bab8298`) — unmute on reopen present (host.rs:1197); eviction
  closes the browser; `set_audio_muted` is a real `cef::BrowserHost` method.
- Profile-dir chmod (`1d4b44b`) — `restrict_dir` applied to both `--private` and
  persistent branches and every CLI short-circuit; session `.json.tmp` chmod'd
  0600 before rename; cfg(unix) tests assert the modes.
- `host_resolves_public` — fail-closed on resolve error and zero addresses;
  IPv4-mapped IPv6 round-trips the string guard correctly; the RFC 2544
  `198.18.0.0/15` / `fec0::/10` misses are already recorded (Shipped note
  below).
- hint.js / edit.js — `__buffrHintCommit` id is u32-typed by serde, splice emits
  digits only; the `data-buffr-hint-target-id` attribute-collision clicks a page
  element — same trust as the page's own handlers. edit.js dual
  `focus`+`focusin` registration idempotent via `lastFocusId`.
- view-source worker permit / read path — `_permit` held for the whole fetch,
  dropped on every exit; `read` bounds-checked against `body.len()`;
  poisoned-lock and null-callback paths serve synchronously.

### Hardening

- **WebAudio-only audio on a stashed tab.** `pause_media.js` pauses only
  `<video>`/`<audio>` elements; an `AudioContext` keeps producing audio. Whether
  a muted CEF browser still emits `OnAudioStreamStopped` (which clears the §11-4
  indicator) is a CEF runtime detail this tree cannot verify.
- **Stale `EditFocus` after tab round-trip (`575a271` side effect).** While tab
  B is active, tab A's `Blur`/`Focus` events are dropped, so returning to A can
  show Insert mode against a field the page has since blurred; no edit-state
  resync on tab switch. Narrower than the misattribution bug it replaced;
  self-heals on the next real focus.
- **`ureq` default redirects elsewhere.** `updates.rs:104-114` (GitHub API)
  inherits the same default; benign for the pinned upstream URL, but the
  redirect default is a standing foot-gun for any future browser-process fetch.

### Coverage

Reviewed at depth: all 8 post-`8744e18` commits and their touched code
(`host.rs` close/reopen/`set_active_index`/`navigate`, `view_source_scheme.rs`
in full, `private_net.rs` in full, `edit.rs` in full, `image_copy.rs` in full,
`main.rs` drain/exit/restore, `session.rs` write path, `context_menu.rs`
dispatch, `event_loop.rs` about_to_wait chain, `apps/buffr/src/main.rs`
supervisor loop + signal thread, `handlers.rs` console bridge + downloads,
`osr.rs` paint routing, `updates.rs`, cmdline/crash/telemetry, all six JS
assets, ureq 3.3.0 installed source). Not re-reviewed (covered by 2026-08-05
passes, findings recorded): buffr-modal keymap/engine, buffr-ui
painting/hit-testing, `windowing/*`, `single_instance.rs` in full, the Windows
supervisor arm (never executed on this host), `buffr-view-source` renderer,
`buffr-zoom`, `buffr-permissions`, the store crates' full bodies, `xtask`,
`fuzz/`, `tests/e2e/*`. Known items confirmed: §11-13 (edit.js teardown omits
`focus` from its removal list — edit.js:286 registered, teardown removes
`focusin`/`focusout`/`input`/`mousedown`/`pointerdown`/`touchstart` only); the
§12-1 range-gap follow-ups still open as recorded.

## 17. Audit 2026-08-06 (63b8a25) — findings

Fresh pass over the whole workspace; the 2026-08-04/05 findings (sections 10-15)
treated as known. One MEDIUM, two LOW — 0 critical, 0 high. Findings 1 and 3
were fixed on 2026-08-06 (`f630585`, `a6b7482`); only 2 remains open.

### 2 LOW — edit-bridge browser attribution collides across engines: webkit `TabId` and CEF `browser.identifier()` share one `i32` space

**Where:** `apps/buffr-app/src/main.rs:4701-4712` (`drain_edit_focus_events`
trusts `browser_id`);
`crates/buffr-webkit/src/platform/runtime.rs:893, 1348-1353` (events tagged
`ctx.tab_id.0 as i32`); webkit mints `TabId(st.next_id)` starting at 1
(`worker.rs:81`, `runtime.rs:3878`). CEF identifiers also start at 1 and
increment per browser.

The §11-1 fix compares a page's tagged `browser_id` against the active engine's
active-tab `browser_id` as a single flat integer. Both backends number tabs from
1, so in a cef+webkit config a webkit tab's id collides with a CEF tab's id. A
page on a **background webkit tab** can forge a `Focus` event (the page nonce is
page-readable — known accepted limitation, §3) tagged with its own tab id; if
that id equals the active CEF tab's identifier, the event is accepted and the
CEF tab flips into Insert — the exact cross-tab keystroke-capture the fix
closed, restored across the engine boundary.

Caveat: cef-only configs (the production default) are sound — all browsers share
the CEF id space. Only multi-engine configs with webkit are exposed; webkit is
experimental and not built by CI; the attacker cannot choose which ids its tabs
receive. Fragile but real.

**Fix:** namespace the attribution (`engine_id + browser_id`), or gate
webkit-tagged events to webkit-active-tabs at the engine level.

### Cleared

- `buffr-src`/image_copy gate internals — `http_host` parsing,
  `host_resolves_public` fail-closed on resolution error/empty/mixed results,
  `is_non_public_host` numeric-form coverage, M14 fetch pool and `FetchPermit`
  accounting, `open`'s synchronous fallback — all traced; no new reachable gap
  beyond finding 1.
- `close_index` (`fe9ec53`), media stash (`bab8298`), `request_exit`
  (`c2f7e18`), supervisor signal loop (`ed46f3a`) — re-traced; correct.
- Profile-dir hardening (`1d4b44b`) — 0700 dirs incl. CLI short-circuits, 0600
  session tmp+rename; umask-independent. `updates.rs`/`telemetry.rs`/`crash.rs`/
  `favicon_cache.rs` — bounded, parameterized, no network exfil, `create_new`
  crash reports.
- Internal-page HTML assembly — keymap/chord strings escaped, splash spans
  static, push JSON-escaped and gated; no XSS.
- `buffr-helper` — argv pass-through to `execute_subprocess` only. `buffr-poc` —
  dev demo, private stores, no network surface.
- `open_finder` — `xdg-open`/`open`/`explorer.exe` argv, never a shell;
  `sanitise_filename` reserved-stem and traversal tests solid.
- Console bridge — nonce anchored + rotated per load/session, parsers
  length-capped, edit/hint/media sinks bounded; the same-tab
  forged-`Selection`→clipboard and forged-`Focus`→Insert paths confirmed still
  present exactly as recorded in §12-2/§12-3 (known, not re-filed).
- Supervisor signal thread after `ed46f3a` — second signal during the graceful
  wait is queued and handled after; `child_pid_slot` cleared before the loop
  re-checks. No new race.

### Hardening

- Internal server accepts a request with no `Host` header
  (`internal_server.rs:444`): defence-weakening only, not exploitable — the
  128-bit per-launch token is still required, browsers always send `Host`, and a
  raw-socket attacker who already holds the token gains nothing from omitting
  the header.
- §12-1 follow-ups already recorded (RFC 2544 `198.18.0.0/15`, IPv6 `fec0::/10`)
  — confirmed present in the tree.
- `tick_splash_js_push` gate `url.starts_with("buffr://new")` is loose but the
  scheme is unreachable from page content and the pushed HTML is static —
  cosmetic only.
- Webkit URI-scheme clipboard handler (`runtime.rs:1400-1569`): gated to pages
  whose URI starts with `buffr://`; all such documents are app-served. The
  10k-line webkit FFI/worker/wpe_subclass code otherwise remains §15-2's
  deferred item — not audited beyond the scheme handler and id-space check.

### Coverage

Walked in full or traced line-by-line: the console bridge (`handlers.rs`
`on_console_message`/`on_load_end`), `edit.rs`/`edit.js`, `hint.js`,
`pause_media.js`, `console_nonce.rs`/`console_sentinel.rs`,
`view_source_scheme.rs` (whole), `private_net.rs` (whole), `host.rs`
close/reopen/`set_active_index`/media-probe/audio/JS-injection call sites,
`internal_server.rs` (whole), `session.rs`, `cli.rs`, `single_instance.rs` (Unix
half), downloads handler + `sanitise_filename` + `open_finder.rs`,
`image_copy.rs` (whole), `buffr-helper`, `buffr-poc`, new-tab/splash assembly,
and the previously-unread `buffr-core` modules (`crash.rs`, `telemetry.rs`,
`updates.rs`, `favicon_cache.rs`) and `buffr-cef/build.rs`. Skimmed via grep,
not line-audited: the full diff of the eight fix commits, the webkit crate
beyond the scheme handler, `windowing/other/*`, `tests/e2e/pages/*.html`
fixtures. No tests run — read-only pass.

Known items confirmed, one line each: §12-2 pastejacking, §12-3 dead
`insert_intent_at` gate, §12-6 download overwrite, §12-7 session-restore scheme
allow-list, §12-9 token persisted, §12-10 unbounded popups, §12-11 import
amplification, §11-5 video probe on active tab only, §11-14 untagged hint sink —
all still present as recorded; §10-1/§11-2/§11-4/§11-7/§12-1/§12-4 fixes
verified working.

**Summary: 1 medium, 2 low new findings — 0 critical, 0 high.** Overall risk
remains low-to-moderate, unchanged from the prior pass: the hard boundaries (IPC
socket peer-cred, internal-server token, permission callbacks, pixel upload,
nonce-anchored parsers) held, and residual risk still concentrates in the
page→app console bridge and the browser-process fetch primitives. Item (1)
(`max_redirects(0)`) and the §17-3 view-source fix shipped on 2026-08-06
(`f630585`, `a6b7482`). Remaining: (2) namespace the edit-attribution id per
engine; (3) decide/implement the §12-2 clipboard gate, the top open security
item.

## 18. Tidy 2026-08-06 (63b8a25) — cleanups

Quality-only sweep (behavior-preserving cleanups; no correctness findings).
Working tree clean at HEAD; backlog §1, §2, §10-§15 read first — items already
recorded there are not re-reported, except as one-line confirmations. Only code
changed since the last tidy pass (`8744e18`) is the 7 fix commits; every hunk of
those diffs was reviewed, then whole-tree sweeps ran over the unchanged surface
too.

### Dead code (each verified: whole-workspace `rg` shows zero callers)

1. **`crates/buffr-cef/src/view_source_scheme.rs:497-505` — two unreachable
   checks in `fetch_and_render`.** The `848a6fb` rewrite added
   `validate_target(url, initiator_host, true)` at :494, which already returns
   `Err` for both an empty URL (:153-154) and a non-http scheme (:156-158) —
   with the exact same error strings. The follow-up `if url.is_empty()`
   (:497-499) and `if http_host(url).is_none()` (:500-505) blocks are dead; they
   were the pre-`848a6fb` inline belt-and-braces the new gate superseded.
   **Action:** delete both blocks (the `error_page` calls they produce are
   byte-identical to `validate_target`'s `Err`).
2. **`crates/buffr-cef/src/host.rs:769-777` — dead `BrowserHost` favicon
   accessor pair.** `favicons_enabled()` (:769-771) and `set_favicon_enabled()`
   (:775-777) have zero call sites. The display handler reads the flag directly
   off the shared `FaviconEnabled` Arc (`handlers.rs:952`,
   `favicon_is_enabled`), not through these methods, and no runtime toggle
   exists — the doc's "reflects any runtime toggle via
   [`Self::set_favicon_enabled`]" describes a feature that was never wired
   (favicon enablement is startup-only, `main.rs:592`). **Action:** delete both
   methods and their doc comments.
3. **`crates/buffr-modal/src/engine.rs:121-123` — dead `Engine::keymap_mut`.**
   Zero callers (the live surface is `keymap()` at :117 and `set_keymap()` at
   :128, called from `main.rs:806`). **Action:** delete.
4. **`apps/buffr-app/src/windowing/other/window.rs:36-57` and `:175-185` — four
   dead size methods** (linter-invisible behind
   `#[allow(dead_code)] mod windowing` at `main.rs:200`):
   `Window::set_min_size`/`set_max_size` (:36-57) and
   `ToplevelBuilder::with_min_size`/`with_max_size` (:175-185) — zero callers;
   both builder call sites (`event_loop.rs:104-108`, `:1572-1575`) set only
   title/app-id/size. Keep the builder _fields_ `min_size`/`max_size` (:151-152)
   — read by `build_window` (`event_loop.rs:243-246`). Supersedes the §13
   "set_min_size/set_max_size near-identical" note: they are dead, not merely
   duplicated. **Action:** delete all four methods.
5. **`apps/buffr-app/src/render.rs:193-194` — `OsrTexture::view` is
   write-only.** Written at :212 and :242, never read (only `texture` is read,
   at :257); the field exists solely under `#[allow(dead_code)]`. The bind group
   created from `&view` holds wgpu's own refcount on the view, so the field
   handle is redundant. _Lower confidence:_ relies on wgpu resource-lifetime
   semantics, not compiled (tree kept pristine). **Action:** delete the `view`
   field (keep the local in `new`/`maybe_upload`), or keep it and drop the
   `#[allow(dead_code)]` with a comment saying the handle is held for the bind
   group's lifetime.
6. **`crates/buffr-webkit/src/platform/engine.rs:380-381` —
   `set_newtab_html_provider` and the `newtab_html_provider` field are
   write-only** (field :59, written :332/:381, never read; setter has no
   callers). Experimental crate, excluded from the workspace, review deferred
   (§15-2) — reported for completeness, not expected to be actioned with the
   rest. **Action:** delete setter + field, or wire the read.

### Nothing new found in

- duplication (machete: no unused deps; no new copies of the §13 helper patterns
  in the changed code — `ensure_profile_data_dir`/`restrict_dir` and
  `request_exit` are themselves the extractions §13 asked for),
- over-abstraction or indirection in the new code,
- needless clones/allocations in the new code (the `host_resolves_public`
  `to_string()` round-trip is a documented "one copy, no drift" choice, and
  `image_copy`/`view_source` call it on worker threads only).

### Known items confirmed (still open at HEAD — recorded in §13/§12/§11, not new)

- §13 dead code, all still present: `media_js.rs` module (all 6 fns),
  `BrowserHost::{run_edit_apply :2467, print_active :2665, frame_del :2646, reload_ignore_cache_active}`,
  `BrowserHost::new` (host.rs:443), `make_client` (handlers.rs:154),
  `insert_intent_at` (main.rs:1630, still write-only under
  `#[allow(dead_code)]`), `Mode` enum (buffr-modal/src/actions.rs:21),
  `PendingPopupAlloc` re-export, `TabOptions` (buffr-engine/src/tab.rs:23),
  `pop_front`/`peek_front` (buffr-engine/src/permissions.rs:88,96),
  `InternalServer::set_routes`, `ContextMenuOverlay::row_at`, `InputBar::paint`,
  `KeyChord::new`, `Keymap::leader` (keymap.rs:100), `HintAlphabet::is_empty`
  (buffr-core/src/hint.rs:129), `Engine::count` (engine.rs:151),
  `BuffrLoadHandler.edit_sink` write-only (handlers.rs:799), windowing accessors
  `SurfaceId::as_u64`/`OutputId::as_u64`/`OutputInfo.description`/
  `Position::ZERO`, `fuzz_target_keys.rs` no-op loop
  (fuzz/fuzz_targets/fuzz_target_keys.rs:8-14).
- §13 YAGNI, all still present: `deserialize_keymap` (config lib.rs),
  `Keymap::audit_default_bindings` dead `_leader` param, `buffr_cef::new_tab`
  re-export shim, the buffr-ui `pub use` constant block, `Statusline::progress`
  write-only.
- §13 duplication, still present: `run_heartbeat_loop` unix/windows twin,
  atomic-write helper (session.rs:147-164 vs crash*guard.rs:137-148 — session.rs
  now additionally chmods the tmp file), deadline-clamp idiom ×9
  (event_loop.rs:2024-2093), `key_to_neutral_events` (cef_translate.rs:395-431),
  cli
  `open*\*\_for_cli`store-open scaffolding, chrome_paint modal-panel paint blocks, statusline right-pen cells ×7, xtask arch/build helpers,`mode_name(PageMode)`
  ×2, host-head extraction (search.rs:155-163 vs 196-199).
- §13 minor, still present: `tab_strip.rs:268-269` loop-invariant glyph width
  (also §14-9), `cef_cursor_to_icon` if-else chain, `omnibar_suggestions` dedup
  loops, `const GUTTER` hoist.
- §13 known items, still present: `__buffrUserGesture` write-only
  (edit.js:75-79,348-350, writers host.rs:2205,2484), `DEFAULT_SKIP_SCHEMES`
  cross-crate duplicate, store-crate shape.
- §13 "last-tab graceful exit ×3" is **shipped** (`c2f7e18` —
  `AppState::request_exit`, main.rs:2766; all four last-tab sites call it).

### Coverage

- Changed since last tidy (`8744e18..HEAD`): every hunk of all 7 fix commits
  read and cross-checked against callers — `private_net.rs`
  (`host_resolves_public`), `scripts.rs` (`PAUSE_MEDIA_JS` + test),
  `view_source_scheme.rs` (`initiator_host` plumbing, `validate_target` resolve
  flag, `fetch_and_render` — finding 1), `edit.rs` (`TaggedEditEvent`),
  `session.rs` (chmod), `host.rs` (`active_after_close`, pause+mute on stash,
  `set_audio_muted`), `handlers.rs` (tagged push), `image_copy.rs`, `main.rs`
  (`restrict_dir`/
  `ensure_profile_data_dir`/`request_exit`/`drain_edit_focus_events`), `cli.rs`,
  `event_loop.rs`, `context_menu.rs`, `apps/buffr/src/main.rs` (signal loop),
  `pause_media.js`, webkit `runtime.rs`.
- Sweeps over the whole tree: `#[allow(dead_code)]` sites (each read and judged
  — `single_instance.rs:132` flock holder, `inhibit/mod.rs:112` NoopInhibitor,
  `main.rs:1736` lifetime-held `update_checker` are deliberate, not re-flagged);
  `todo!`/`unimplemented!`/`unreachable!` (all in documented test stubs —
  `engine_router.rs` StubEngine, `engine.rs` NoOpEngine); whole-workspace pub-fn
  occurrence analysis (findings 2-6); `impl From`/`Deref` shims, clone-on-Copy,
  write-only fields, machete.
- Previously-unread files covered: `apps/buffr/tests/*` (7 integration test
  files + `common/mod.rs` — clean), `buffr-helper` (clean), `buffr-poc`
  (experimental demo, clean).
- Not walked in depth: `buffr-webkit` internals beyond the changed hunks and
  finding 6 (experimental, not in the workspace, §15-2 defers its review);
  `tests/e2e/pages/*.html` fixtures (data); JS assets beyond confirming the
  known `__buffrUserGesture` item (fully read by the previous pass, unchanged
  since).
- No build/test/format run — the tree stays pristine.

## 19. Performance review 2026-08-06 (63b8a25) — findings

Fresh pass; backlog §7 (2026-08-04 perf) and §14 (2026-08-05 perf) treated as
known and not re-reported. The three findings below are in code the 2026-08-05
pass only skimmed (`buffr-view-source` / `view_source_scheme.rs`) plus one
sibling of a known item. No O(n²) loops, no per-item I/O, no syscalls-in-loops,
no lock-across-await found in the previously-untouched crates (buffr-store,
buffr-history, buffr-bookmarks, buffr-downloads, buffr-zoom, buffr-modal,
buffr-permissions, fuzz/, xtask/) — those are cold-path (open/migrate/import/
clear at startup or on user action) or bounded-small-N (omnibar search capped in
SQL, keymap trie = 1-2 HashMap hops per keystroke, hint `feed` = one `retain`
over ≤256 labels per keystroke).

### 1 MEDIUM — view-source rebuilds the entire highlight setup chain (registry, loader, grammar dlopen, highlighter) per request

**Where:** `crates/buffr-view-source/src/lib.rs:75`
(`GrammarRegistry::embedded()`), `:91` (`GrammarLoader::user_default(meta)`),
`:109` (`Grammar::load_from_path`), `:117` (`Highlighter::new(grammar)`), all
inside `try_highlight`, called from `render()` (`:57`) for every view-source
navigation.

Every `buffr-src:` request re-parses the embedded `bonsai.toml` manifest into a
fresh `HashMap`, re-resolves the XDG data/cache dirs
(`SourceCache::user_default`, `QuerySourceCache::user_default` — env read + path
build each), re-walks the three grammar dirs in `lookup_only` (:103),
**re-`dlopen`s the grammar `.so`** (`Grammar::load_from_path` → `Library::new` +
symbol lookup), and constructs a fresh tree-sitter `Parser` + predicate
registry. Only the compiled query artifacts are cached (hjkl-bonsai's
process-global `COMPILED_CACHE` keyed by content hash — installed 0.41.0 source,
`highlighter.rs:373-378`), so the dlopen and env/dir plumbing is paid on every
request.

Why it matters: per view-source navigation (worker spawned at
`crates/buffr-cef/src/view_source_scheme.rs:322-334`, `render` called at :535),
including reloads and restored pinned view-source tabs at startup (up to 8
concurrent, `MAX_INFLIGHT_FETCHES`). The dlopen is the dominant term (~100 µs–1
ms on a cold page cache) plus a handful of stat/env syscalls — all of it
identical work on the second request for the same language as on the first. For
a small source file this setup is a meaningful fraction of total render time.

**Fix:** cache per language — a
`static GRAMMARS: OnceLock<Mutex<HashMap<&'static str, Arc<Grammar>>>>` keyed by
language name makes the dlopen + load happen once per language per process (the
`Arc<Grammar>` is already what `Highlighter::new` takes), plus a second
`OnceLock` for the registry+loader pair. Trade: a handful of MB of loaded
grammars retained for process lifetime — the standard memory-for-speed trade;
grammars are small C objects. The existing A6 comment (:99-102) forbids
`Grammar::load` (network compile); the cache is orthogonal — keep `lookup_only`
as the only resolution path.

### 2 LOW-MEDIUM — `render_spans` makes ~4-5 heap allocations per span and copies escaped content twice

**Where:** `crates/buffr-view-source/src/lib.rs:171-173` inside the span walk
`render_spans` (:147-187): `capture_to_class(capture)` allocates twice
(`capture.replace('.', "-")` at :191 then `format!("hl-{normalized}")` at :192),
`html_escape(content)` allocates a fresh `String` per span (:196-209), then
`html.push_str(&format!("<span class=\"{class}\">{escaped}</span>"))` copies the
just-escaped bytes again (:173), plus `html_escape(plain)` per inter-span gap
(:168).

Why it matters: the same per-view-source path as finding 1. A 10 MiB source (the
`MAX_SOURCE_BYTES` cap, :25) at ~40 bytes per token span ≈ 250 K spans ≈ 1 M
heap allocations and 2× re-copy of nearly every byte of the source before the
final page string is assembled. Dominant Rust-side cost of highlighting large
files — the case the 10 MiB cap exists for.

**Fix:** write into the existing buffer instead of allocating intermediates — a
small `push_escaped(&mut String, &str)` helper escaping straight into `html`, a
`push_str("<span class=\"") + push(class) + push_str("\">")` sequence, and
precompute the class string via a `match` on the bounded set of tree-sitter
capture names (the CSS table at :270-294 already enumerates them — the
`replace`+`format` per span only ever produces one of ~30 values). Same
complexity, zero per-span heap traffic.

### 3 LOW — `json_string_literal` allocates a `String` per escaped non-ASCII char on the hint-filter keystroke path

**Where:** `crates/buffr-cef/src/host.rs:2911` —
`out.push_str(&format!("\\u{unit:04x}"))` inside the per-char loop of
`json_string_literal` (:2897-2918).

One `format!` heap allocation per non-ASCII (or otherwise non-printable)
codepoint in the typed hint filter string. Runs on every hint `Filter`/backspace
keystroke (`host.rs:2373-2376` and `:2416-2419` — the `__buffrHintFilter`
splice). ASCII text (the common case) hits the `is_ascii_graphic` arm and pays
nothing, so this only bites when the hint filter contains non-ASCII — but it is
the same per-char-alloc class §14-12 flagged at `search.rs:254` (`url_encode`),
a second copy in a per-keystroke path.

**Fix:** identical to §14-12's — push the two hex digits from a
`const HEX: &[u8; 16]` table instead of `format!` (or `write!` into `out`).
Constant fix, no memory trade.

### Coverage

Traced fully (with callers/frequency established): the whole per-tick
`about_to_wait` chain (`event_loop.rs:1297-2108` — session flush, notice expiry,
tab-strip resync, favicon pump, telemetry flush, cursor blink, loading anim),
key/pointer/scroll/IME dispatch, `pump_address_changes`, `pump_cursor_changes`,
`drain_edit_focus_events`, hint filter JS + Rust session feed, omnibar
suggestions + resolve path, internal server accept/handle loop, OSR
`on_paint`/`view_rect`/`screen_info`/`resolve_dims`/`resolve_frame_view`,
downloads `on_download_updated` + store, media probe poll cadence
(occluded-only, active engine, 2 s — cold), console-message scrape path, favicon
cache get/put, view-source scheme create/fetch/render/read, image_copy,
private_net guards, session save, zoom/bookmark/history/store crates,
buffr-modal engine+keymap, config search, all core JS assets,
supervisor/cli/crash_guard/ heartbeat/single_instance (cold), fuzz targets,
xtask.

Known items confirmed (recorded in §14 — not re-reported, one line each): 14-1
font glyph mutex per char per frame (font.rs:39-58); 14-2 per-tick
`refresh_tab_strip` + favicon pump (main.rs:2973-3015, 3110-3132); 14-3 edit
full-value IPC + two-pass JSON parse per keystroke (edit.js:332,
edit.rs:214/243); 14-4 `tick_splash_js_push` polls active URL every tick off the
new-tab page; 14-5 OSR per-paint popup mutex + Arc clones (osr.rs:326-331,
388-412); 14-6 downloads per-tick full-row hydrate (downloads lib.rs:303-315,
223-244); 14-8 hint.js DOM rebuild per filter keystroke (hint.js:145-168); 14-9
`two_char_px` loop-invariant (tab_strip.rs:268-269); 14-11 two nonce-table
lookups per sentinel line (handlers.rs:1095, 1112); 14-12 `url_encode` per-byte
`format!` (search.rs:254); 14-14 internal-server routes-table clone per
connection (internal_server.rs:298-303).

Not settled without profiling: absolute µs of the view-source highlight setup vs
the highlight itself (finding 1's rank assumes dlopen + dir/env resolution
dominates for small files; finding 2 is unambiguous allocation count); whether
hjkl-bonsai's `Highlighter::new` `PredicateRegistry::with_builtins()` is
material at request rates. `buffr-webkit`/`buffr-poc` remain out of the
workspace (per §15-2) and were not compiled or line-audited.

**Verdict:** nothing blocking. Two genuine per-request wins in the newest code
(view-source), one minor per-keystroke alloc; the rest of the previously
unreached tree is cold or bounded-small-N. Findings 1-2 are the next queue
entries when perf work resumes.
