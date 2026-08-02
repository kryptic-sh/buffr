# Backlog

Everything left over from the full-codebase review in
[`code-review.md`](./code-review.md), plus what surfaced while fixing it.

Nothing here is a known-broken build: `main` is green on CI (all three OSes),
`cargo deny`, `cargo machete`, and the fuzz workflow. These are the items that
were deliberately **not** actioned, and why.

Grouped by what is actually blocking them.

---

## 1. Needs a product decision

Each of these has a real defect behind it, but two or more defensible
resolutions. None was touched, so the current behaviour is whatever the table
describes.

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

| ID  | Where                                                    | The problem                                                                                                                                                                                           | The choice                                                                                                         |
| --- | -------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| H6  | `crates/buffr-cef/src/lib.rs`, `app.rs`                  | `no_sandbox: 1` plus a `--no-sandbox` switch disable the Chromium sandbox process-wide, so any renderer RCE is immediate code execution as the user.                                                  | Ship the setuid `chrome-sandbox` helper / rely on user namespaces, or gate it per-target.                          |
| M48 | `apps/buffr-app/src/main.rs`                             | `:open` passes its argument straight to `navigate`, bypassing the allow-list every other path enforces. `:open javascript:…` runs in the page origin.                                                 | Is `:open` meant to be a privileged escape hatch, or should it go through `resolve_input`?                         |
| M39 | `crates/buffr-modal/src/keymap.rs`                       | `<C-c>` is bound twice; last-write-wins gives `YankUrl`, so `StopLoading` has no working binding — and `missing_default_bindings` scans the table, not the trie, so the a11y test reports it covered. | Pick an owner for `<C-c>`; separately, make the audit walk the built keymap so a shadowed row can't fake coverage. |
| M49 | `buffr-config` + `apps/buffr-app`                        | Four knobs are parsed, validated and documented but never read: `startup.new_tab_url`, `startup.restore_session`, `theme.mode`, `updates.channel`.                                                    | Wire each up or delete it. Docs currently describe them honestly as inert.                                         |
| L23 | `apps/buffr-app/src/main.rs`                             | The occlude-sleep debounce is dead — `sleep_deadline` is only ever set to `None`, so the expiry check and deadline clamp can never fire.                                                              | Wire `Occluded(true)` to arm it as designed, or delete the field, const and both blocks.                           |
| L36 | `crates/buffr-ui/src/lib.rs`                             | Hint status renders `(n/n)` — numerator and denominator are the same field, so it always reads e.g. `3/3`.                                                                                            | Drop the pair, or add a real `current` index to `HintStatus`.                                                      |
| L37 | `crates/buffr-modal/src/actions.rs`                      | `PageMode::Pending` is documented as live but never produced; `buffr-ui` and `buffr-app` carry unreachable arms for it.                                                                               | Produce it while a prefix is pending, or delete it and the dead arms.                                              |
| L38 | `crates/buffr-modal/src/engine.rs`                       | The register prefix (`"a`) consumes two keystrokes, stores the register, then discards it — indistinguishable from a broken keymap.                                                                   | Plumb `register` into the emitted action, or remove the prefix handling.                                           |
| L18 | `crates/buffr-engine/src/event.rs`, `types.rs`, `tab.rs` | `EngineEvent`, `NavigationEvent`, `LoadState`, `CursorChanged`, `CursorKind`, `TabOptions` have zero users; `CursorKind` is actively contradicted by the trait.                                       | Delete, or keep for a planned migration.                                                                           |
| L19 | `crates/buffr-engine/src/engine.rs`                      | `supports_native` / `set_native_parent` / `set_native_visible` document a four-step protocol with no callers and no implementors.                                                                     | Delete until the subsurface work lands, or wire the apps layer to honour it.                                       |
| L40 | `crates/buffr-history/src/lib.rs`                        | Eight constructors for the cross-product of two optional params; a third would mean sixteen.                                                                                                          | Collapse to an options struct/builder — a public API change.                                                       |
| L41 | `crates/buffr-config/src/search.rs`                      | `classify_input` / `InputKind` are dead public API whose doc admits it "mirrors the branch order in `resolve_input` exactly" — i.e. hand-synced.                                                      | Delete, or implement `resolve_input` in terms of it so they cannot drift.                                          |

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

- **`buffr-bin` on the AUR is still at the pre-0.14.7 version.** The `aur-bin`
  job failed on the `v0.14.7` tag run with
  `No ED25519 host key is known for aur.archlinux.org`: the pinned `known_hosts`
  was written correctly, but `GIT_SSH_COMMAND` was supplied through an `env:`
  block containing a literal `~`, and neither git's `sh -c` (tilde expansion
  does not apply to the result of a parameter expansion) nor ssh itself expands
  it in a `-o` value.

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

  The fix cannot be applied retroactively: re-running the job checks out the
  workflow file as of the `v0.14.7` tag, which still has the bug, and the tag
  must not be moved. So AUR catches up on the **next** tagged release. Nothing
  to do here beyond cutting one.

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

## Corrections to the review itself

Three findings in `code-review.md` were **wrong** and were deliberately not
"fixed". Recorded here so nobody re-files them:

- **L39** — `TYPEFLAG_FRAME` does have a caller (`context_menu.rs`), so it was
  kept while the genuinely-dead constants around it were removed.
- **L21** — `ActivationError` is live via `request_activation`; only the rest of
  the windowing parity surface was dead.
- **L46 (the cef half)** — the `cef` crate at 148.x wraps **libcef 147.0.14**,
  and `xtask` pins `CEF_VERSION_PREFIX = "147."`, so the docs' "cef-147" was
  already correct. Only the wording was clarified.
