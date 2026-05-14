# buffr-cef

CEF (Chromium Embedded Framework) backend for buffr.

## Process model constraints

### `cef::initialize` is once-per-process

`cef::initialize(Settings)` must be called **exactly once** per OS process.
`BuffrApp` is registered at that call site in `app.rs` and governs the
single CEF message loop for the lifetime of the process.

### Cache path is global

`Settings::cache_path` is a global setting for the entire CEF process — it
cannot differ between `BrowserHost` instances constructed in the same process.
Two `BrowserHost` instances therefore share one on-disk cache, one cookie jar,
one localStorage tree, and one GPU process.

**Per-engine on-disk isolation** (separate cookies, separate localStorage)
requires using `CefRequestContext` with per-context
`CefRequestContextSettings::cache_path`. This is tracked as a follow-up to
phase 3 (reference issue #74).

### Multiple `BrowserHost` instances are safe

Multiple `BrowserHost` instances **can** coexist in one process. Each owns its
own tab map, OSR frame buffer, audio event queue, and popup state. No `static`
globals shadow across instances — the per-instance `Arc<Mutex<…>>` fields
introduced in Phase 1 ensure full isolation of runtime state.

### `execute_subprocess()` is called once per process

The CEF helper/renderer subprocess is launched once. Because all
`BrowserHost` instances share the same CEF process group, helper subprocess
arguments cannot be namespaced per-instance. Per-instance helper args
(`--engine-id=<id>`) require `CefRequestContext` isolation and are tracked
as a Phase 5+ follow-up (reference issue #74).

## Phase 3 notes

- Multiple `BrowserHost` instances are constructable via `BrowserHost::new_with_options`.
- The per-engine `data_dir` config field is accepted but **advisory only**
  in Phase 3. A `tracing::warn!` is emitted at startup when set, pointing
  users to the Phase 5+ follow-up.
- Paint multiplexing, cross-engine navigation, and the engine registry
  all go through `dyn BrowserEngine` — no CEF-specific code outside
  `buffr-cef` is needed.
