# Backlog

Everything left over from the full-codebase review in
[`code-review.md`](./code-review.md), plus what surfaced while fixing it.

Nothing here is a known-broken **build**: `main` is green on CI (all three
OSes), `cargo deny`, `cargo machete`, and the fuzz workflow. These are the items
that were deliberately **not** actioned, and why.

That is not the same as "no known bugs". Sections 8 and 9 hold a review pass
from 2026-08-02. Five correctness findings have since been fixed and removed
from section 8; what remains there — and the whole of section 9 — is
reproducible today on green CI. Read those before assuming the tree is clean.

Grouped by what is actually blocking them.

---

## 1. Needs a product decision

Each of these has a real defect behind it, but two or more defensible
resolutions. None was touched, so the current behaviour is whatever the table
describes.

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

| ID  | Where                                                    | The problem                                                                                                                                                                                            | The choice                                                                                                         |
| --- | -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| H6  | `crates/buffr-cef/src/lib.rs`, `app.rs`                  | `no_sandbox: 1` plus a `--no-sandbox` switch disable the Chromium sandbox process-wide, so any renderer RCE is immediate code execution as the user.                                                   | Ship the setuid `chrome-sandbox` helper / rely on user namespaces, or gate it per-target.                          |
| M48 | `apps/buffr-app/src/main.rs`                             | `:open` passes its argument straight to `navigate`, bypassing the allow-list every other path enforces. `:open javascript:…` runs in the page origin.                                                  | Is `:open` meant to be a privileged escape hatch, or should it go through `resolve_input`?                         |
| M39 | `crates/buffr-modal/src/keymap.rs`                       | `<C-c>` is bound twice; last-write-wins gives `YankUrl`, so `StopLoading` has no working binding — and `missing_default_bindings` scans the table, not the trie, so the a11y test reports it covered.  | Pick an owner for `<C-c>`; separately, make the audit walk the built keymap so a shadowed row can't fake coverage. |
| M49 | `buffr-config` + `apps/buffr-app`                        | Four knobs are parsed, validated and documented but never read: `startup.new_tab_url`, `startup.restore_session`, `theme.mode`, `updates.channel`.                                                     | Wire each up or delete it. Docs currently describe them honestly as inert.                                         |
| L23 | `apps/buffr-app/src/main.rs` + `event_loop.rs`           | The occlude-sleep debounce is dead — `sleep_deadline` is only ever set to `None`, so the expiry check and deadline clamp can never fire. The field is on `AppState`; the checks moved to `event_loop`. | Wire `Occluded(true)` to arm it as designed, or delete the field, const and both blocks.                           |
| L36 | `crates/buffr-ui/src/lib.rs`                             | Hint status renders `(n/n)` — numerator and denominator are the same field, so it always reads e.g. `3/3`.                                                                                             | Drop the pair, or add a real `current` index to `HintStatus`.                                                      |
| L37 | `crates/buffr-modal/src/actions.rs`                      | `PageMode::Pending` is documented as live but never produced; `buffr-ui` and `buffr-app` carry unreachable arms for it.                                                                                | Produce it while a prefix is pending, or delete it and the dead arms.                                              |
| L38 | `crates/buffr-modal/src/engine.rs`                       | The register prefix (`"a`) consumes two keystrokes, stores the register, then discards it — indistinguishable from a broken keymap.                                                                    | Plumb `register` into the emitted action, or remove the prefix handling.                                           |
| L18 | `crates/buffr-engine/src/event.rs`, `types.rs`, `tab.rs` | `EngineEvent`, `NavigationEvent`, `LoadState`, `CursorChanged`, `CursorKind`, `TabOptions` have zero users; `CursorKind` is actively contradicted by the trait.                                        | Delete, or keep for a planned migration.                                                                           |
| L19 | `crates/buffr-engine/src/engine.rs`                      | `supports_native` / `set_native_parent` / `set_native_visible` document a four-step protocol with no callers and no implementors.                                                                      | Delete until the subsurface work lands, or wire the apps layer to honour it.                                       |
| L40 | `crates/buffr-history/src/lib.rs`                        | Eight constructors for the cross-product of two optional params; a third would mean sixteen.                                                                                                           | Collapse to an options struct/builder — a public API change.                                                       |
| L41 | `crates/buffr-config/src/search.rs`                      | `classify_input` / `InputKind` are dead public API whose doc admits it "mirrors the branch order in `resolve_input` exactly" — i.e. hand-synced.                                                       | Delete, or implement `resolve_input` in terms of it so they cannot drift.                                          |

### `buffr-webkit` only (excluded crate, not shipped)

| ID  | The problem                                                                                                                                                                                                           |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| W2  | The `buffr-clipboard` scheme is registered CORS-enabled and secure, and the handler returns the full system clipboard with no origin check, gesture or prompt — any page can read it via `fetch`.                     |
| W8  | Any non-internal scheme is handed to `xdg-open` straight from the policy handler, with no user gesture and no confirmation (Chromium prompts here). The spawned child is never reaped, so each launch leaks a zombie. |

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
- **"Every commit is CI-green" is not true, and the run list says so.** The CI
  workflow uses a concurrency group that cancels superseded runs, so when
  commits land in quick succession only the last one completes. Of the five
  correctness fixes, **C1–C4 (`4b9e4cb`, `29e6a22`, `64c2327`, `08ab615`) all
  show `cancelled`**; the first green run in that stretch is C5 (`f6c1360`). Six
  of the seven decomposition slices were cancelled the same way, with `e374b37`
  the green one. The tip being green is genuine and is what the release rests
  on, but no individual fix was independently verified on CI — each rests on the
  local `--workspace` run instead. If per-commit CI is wanted, pushes have to be
  spaced or the group narrowed.
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
- **`docs/code-review.md` is deliberately not in `SUMMARY.md`.** The book is the
  public user-facing site; listing it would publish unfixed security findings
  with reproduction steps.

---

## 4. Small cleanups

Mechanical, low-risk, no decision needed — just not in scope for the review
slices.

- `fuzz/corpus/` is gitignored, so every CI fuzz run starts from an empty corpus
  and re-derives the same shallow coverage. Committing seed inputs (or caching
  the corpus between runs) would let the two new targets — `console_sentinel`
  and `netscape_import` — actually get somewhere.
- `docs/ui-stack.md` still describes option A's softbuffer history in prose.
  Accurate as history, but worth a "superseded" marker now that the wgpu path is
  the only one.
- **`[privacy] clear_on_exit` — `cache` and `local_storage` are no-ops.**
  `run_clear_on_exit` in `apps/buffr-app/src/main.rs` resolves both against
  `paths.cache` (`~/.cache/buffr`), but CEF's `root_cache_path` is `paths.data`
  (`~/.local/share/buffr`), so the deletes hit a directory CEF never populated
  and log `clear_on_exit: dir absent — skipping`. The other four categories
  (`cookies`, `history`, `bookmarks`, `downloads`) work. One-line fix
  (`paths.cache` → `paths.data`); documented as broken in `docs/config.md` and
  `config.example.toml` until it lands.
- The review's own `Summary` counts in `code-review.md` are frozen at the time
  of writing and no longer reflect what has been fixed; the `Status` section
  above them is the live view.

---

## 5. Release follow-ups

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

## 6. `main.rs` decomposition — partly done

`apps/buffr-app/src/main.rs` was 11,628 lines. Seven slices took it to 6,994,
each a separate commit verified with `cargo fmt --all`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` (1072
passed, 0 failed throughout) and a diff against the previous commit proving the
move was pure. CI is green on the result (`e374b37`). The C1–C5 fixes have since
grown it back to 7,301, which is the number to beat.

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

## 7. Found while cleaning up, not actioned

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
  `archive/wl-subsurface-poc` is the subsurface work **L19** refers to.

---

## 8. Correctness review, 2026-08-02

A read-only review pass, since partly actioned. **C1–C5 shipped** and have been
removed from this list — `git log` has them. What is left below is unfixed.
Findings marked ✅ were re-verified by opening the cited location; the rest are
reported as found and still need confirming before anyone acts on them.

### C6 MEDIUM — `on_tab_switch` resets `last_osr_generation` to 0

**Where:** `AppState::on_tab_switch` in `apps/buffr-app/src/main.rs`;
`is_osr_frame_fresh` in `apps/buffr-app/src/paint_policy.rs`.

The doc comment argues this is safe because the new tab's first paint always has
a non-zero generation. True, but it also makes the _already-consumed_ generation
compare as fresh, defeating the double-swap guard. On `gt` the user can see a
stale frame of the tab they just left instead of the loading animation.

### C7 LOW — `reopen_closed_tab` can break the pinned-first ordering

**Where:** `reopen_closed_tab` in `crates/buffr-cef/src/host.rs` restores at
`entry.index.min(tabs.len())` without calling `enforce_pinned_ordering()`,
unlike `toggle_pin_active` and `set_pinned`. `hit_test_tab_strip_pure` derives
pill widths from `i < pinned_count` while `TabStrip::paint` uses each tab's own
`pinned` flag, so once the invariant breaks, clicks select the wrong tab.

### C8 ✅ MEDIUM — popup browsers never get a device scale, so they render at 1×

**Where:** `on_before_popup` in `crates/buffr-cef/src/handlers.rs` builds the
popup's `OsrViewState::new()` and stores only width and height into it;
`BrowserHost::set_device_scale` in `crates/buffr-cef/src/host.rs` writes
`self.osr_view` — the **main** view — and nothing else.
`OsrPaintHandler::resolve_scale` in `crates/buffr-cef/src/osr.rs` returns the
popup's own `scale()` for a popup browser id, and `default_view_dims_and_scale`
in the same file asserts `new()` starts at 1.0.

So on a 2× display CEF lays the popup's page out at scale 1.0 while the embedder
divides pointer coordinates by 2: the page renders at half size in the quad and
clicks land at roughly twice the intended offset. Found while fixing C5 and left
alone — C5 was physical-to-physical geometry, this needs the popup view seeded
from the main view's scale at creation, and then kept in step by
`set_device_scale`, which today has no idea popups exist. Untested at a real
HiDPI scale either way; see section 2.

### C9 LOW — `[keymap.pending]` folds into the Normal trie without saying so

**Where:** `Keymap::mode_map_mut` in `crates/buffr-modal/src/keymap.rs`
(`PageMode::Normal | PageMode::Pending => Some(&mut self.normal)`); `validate`
in `crates/buffr-config/src/lib.rs` rejects only `PageMode::Insert`.

The same silent-fold shape C3 fixed, one severity down. Folding Pending into
Normal is _correct_ — a pending chord continues in the normal trie, so there is
no separate map to file into — but a user who writes `[keymap.pending]` gets a
binding that also fires in Normal with no diagnostic. Either reject the section
the way `[keymap.insert]` now is, or document that it is an alias. Note **L37**
in section 1: `PageMode::Pending` is never actually produced, so resolving that
one first may delete the question.

---

## 9. Performance review, 2026-08-02 (none fixed)

Same pass, same caveat: **nothing here has been changed**. ✅ = re-verified at
the cited location.

Frequency baseline the findings rest on: `about_to_wait` clamps its wakeup to
the fastest output's refresh period, so its whole body runs at display refresh
rate even when idle. A 1080p logical chrome buffer is ~8.3 MB.

### P1 ✅ HIGH — unbounded bookmark query on the UI thread per omnibar keystroke

**Where:** `AppState::omnibar_suggestions` in `apps/buffr-app/src/main.rs` calls
`self.bookmarks.search(needle)` then `.take(8)`.

`search` delegates to `search_limited(query, None)` → `NO_LIMIT`, so a
`LIKE '%…%'` full scan returns **every** match, plus a second query fetching
tags for all of them — after which Rust discards all but 8. `search_limited`
already exists in `crates/buffr-bookmarks/src/lib.rs` and takes the limit.

Runs synchronously on the event loop for every printable char, backspace,
`<C-u>` and `<C-w>` in the omnibar, and is **not debounced** — while live-find
next to it _is_ (`FIND_LIVE_DEBOUNCE_MS`). Fix: pass `Some(8)`, and debounce the
suggestion refresh like find. Cheapest high-value fix in this list.

### P2 ✅ HIGH — the full OSR buffer is copied every frame, then often discarded

**Where:** `Renderer::frame` in `apps/buffr-app/src/render.rs` does
`osr.as_ref().map(OsrUploadOwned::from)`, whose `From` impl is
`pixels: u.pixels.to_vec()` — an ~8 MB alloc + memcpy on the UI thread. The
worker then skips `write_texture` entirely when `upload.generation` is
unchanged, making the copy pure waste.

The `SyntheticScratch` path deliberately reuses the generation the GPU already
holds, so this fires on cursor blink, statusline and progress updates, tab hover
and mode changes. Fix: track the last-sent generation and send an empty `pixels`
when it matches; recycle buffers rather than copying on the fresh path.

### P3 ✅ HIGH — 8.3 MB allocated and zeroed per chrome-dirty frame

**Where:** `apps/buffr-app/src/render.rs`, `vec![0u32; lw * lh]` immediately
before `paint_chrome`.

Allocated on the UI thread, freed on the worker thread — a pattern that defeats
allocator thread caches — and only the top (~52) and bottom (~28) logical rows
are ever written; the browser region must stay transparent, which it already is
in a reused buffer. Forced true every tick during the loading animation. Fix:
recycle a `chrome_scratch` via a return channel and clear only the strip bands
plus the previous overlay rect.

### P4 ✅ HIGH — the tab strip is cloned and diffed at refresh rate

**Where:** `apps/buffr-app/src/event_loop.rs`,
`let prev_tabs = self.tab_strip.tabs.clone();` before `refresh_tab_strip`.

Per tick this clones N tab-title `String`s purely to diff — work
`refresh_tab_strip` already does internally as `tabs_changed`. It also runs
`host.tabs_summary()`, which takes one `tabs` lock plus a `display_urls` lock
and two `String` allocations **per tab**, and allocates a `HashSet` to drive one
`retain`. At 20 tabs / 144 Hz that is roughly 100 allocations and 21 mutex
acquisitions per frame to conclude "nothing changed".

The comment above it is also stale: it claims "the cost is a small alloc" and
that redraws are gated "via softbuffer's damage rect" — softbuffer was replaced
by the wgpu path. Fix: return `tabs_changed` from `refresh_tab_strip` and add a
`tabs_revision` counter on `BrowserHost` to early-return.

### P5 ✅ MEDIUM — every glyph blit clones its bitmap and takes two mutexes

**Where:** `crates/buffr-ui/src/font.rs`,
`lock_ignore_poison(&f.cache).get(&c).cloned()`.

`.cloned()` copies the rasterized coverage bitmap on a cache **hit**, per
character. `draw_text` separately calls `char_width` per char, taking a second
mutex, and `truncate_to_width` walks the string doing the same. A chrome repaint
draws several hundred glyphs. Fix: store `Arc<(Metrics, Vec<u8>)>`, fold the
advance into the same entry, and hold one guard across the string.

### P6 MEDIUM — the whole chrome texture is re-uploaded when one strip changed

**Where:** `Renderer::write_chrome` in `apps/buffr-app/src/render.rs` always
issues a full-size `write_texture` with no sub-rect. This is the largest
per-frame PCIe transfer in the app, and the same file documents observed
multi-second blocks on `write_texture`. Fix: pass a dirty rect (top band +
bottom band) and use `origin`/`Extent3d` with an offset into the source. Typical
saving ~8.3 MB → ~0.4 MB per frame.

### P7 MEDIUM — SQL is recompiled from source text on every omnibar keystroke

**Where:** `crates/buffr-history/src/lib.rs` and
`crates/buffr-bookmarks/src/lib.rs` use `conn.prepare(...)`; there is no
`prepare_cached` anywhere in the workspace. The history query is a
`visits ⋈ visits_fts` join with `GROUP BY` and two correlated subqueries, so
SQLite re-parses and re-plans it per keystroke. Both stores hold a long-lived
`Connection`, so a per-connection statement cache would persist. No new
dependency needed.

### P8 LOW — avoidable per-event allocations

- `tabs_summary()` is materialised in full just to read the ids (throttled to 4
  Hz); a `tab_ids()` accessor would cost one lock and no allocations.
- `tick_splash_js_push` calls `active_tab_live_url()` — two locks and a `String`
  — from every `about_to_wait` tick merely to compare against `NEW_TAB_URL`.
  Gate it on the next-push deadline it already maintains.
- While a context menu is open, `to_overlay` rebuilds a `Vec<ContextMenuEntry>`
  per `PointerMoved` (100–1000 Hz) purely to hit-test, then again during paint.
- `loading_anim.rs` does `cell.ch.to_string()` per cell per frame; exposing a
  `font::draw_char` removes it.

---

## 10. Queued, agreed but not started

The order settled on 2026-08-02: finish the correctness findings, then the
performance ones, then the dependency bump. One item per commit — delegate,
verify independently, commit, push — rather than a batch.

- **Correctness: C6, C7, C8, C9** in section 8. C6 and C7 are from the original
  pass; C8 and C9 surfaced while fixing C5 and C3 respectively.
- **Performance: P1–P8** in section 9, none started. P1 (unbounded bookmark
  query plus the missing omnibar debounce) is the cheapest high-value one and
  was picked as the entry point.
- **Bump the HJKL dependencies to latest.** Six of them are declared in
  `[workspace.dependencies]` in the root `Cargo.toml`: `hjkl-engine`,
  `hjkl-buffer`, `hjkl-clipboard`, `hjkl-splash`, `hjkl-config` and
  `hjkl-bonsai`. They are caret requirements, not the `=` pins older changelog
  entries describe, so a `cargo update` already moves them within a major and
  only a major bump needs the manifest touched. Use `cargo add`/`cargo update`
  rather than editing versions by hand. Historically these bumps needed no
  source changes because buffr consumes only editor-level APIs — an assumption
  worth re-checking each time, not a guarantee.

---

## 11. Working practice, learned the hard way

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

## Corrections to the review itself

Three findings in `code-review.md` were **wrong** and were deliberately not
"fixed". Recorded here so nobody re-files them:

- **L39** — `TYPEFLAG_FRAME` does have a caller
  (`crates/buffr-core/src/context_menu.rs` — there are now three files by that
  name), so it was kept while the genuinely-dead constants around it were
  removed.
- **L21** — `ActivationError` is live via `request_activation`; only the rest of
  the windowing parity surface was dead.
- **L46 (the cef half)** — the `cef` crate at 148.x wraps **libcef 147.0.14**,
  and `xtask` pins `CEF_VERSION_PREFIX = "147."`, so the docs' "cef-147" was
  already correct. Only the wording was clarified.
