# buffr-core

CEF integration and browser host for buffr.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Central integration layer between CEF and the rest of the buffr workspace. Owns
`BrowserHost` (multi-tab browser lifecycle), off-screen rendering (OSR), the
hint-mode injection subsystem, the console-IPC bridge for edit-mode DOM sync,
popup window management, and Phase 6 crash reporter + update checker.

## Status

`0.0.1` — multi-tab `BrowserHost`, OSR on Linux, `NEW_POPUP` / `window.open` in
dedicated native windows, OAuth popups isolated. History / downloads / bookmarks
/ permissions / zoom data layers wired into CEF handler callbacks.

## Key types

| Type / function                  | Purpose                                                                 |
| -------------------------------- | ----------------------------------------------------------------------- |
| `BuffrApp`                       | `cef::App` impl; handles command-line processing + scheme registration. |
| `BrowserHost`                    | Multi-tab CEF browser host; creates / routes / closes tabs.             |
| `Tab` / `TabId`                  | Per-tab state (URL, title, CEF browser, OSR frame, hint session, …).    |
| `init_cef_api()`                 | Pin the CEF API version before any CEF entry point — call first.        |
| `profile_paths()`                | Resolve `~/.local/share/buffr/` (or platform equivalent).               |
| `HintSession`                    | Manages hint label injection, JS execution, and label-pick dispatch.    |
| `EditEventSink`                  | MPSC sink for DOM edit-field console-IPC events.                        |
| `FindResult` / `FindResultSink`  | Find-in-page result slot updated by `OnFindResult` CEF callback.        |
| `UsageCounters`                  | Local-only opt-in usage counters (`pages_loaded`, `tabs_opened`, …).    |
| `CrashReporter`                  | Panic hook that writes `<data>/crashes/<ts>.json`; off by default.      |
| `UpdateChecker` / `ReleaseInfo`  | Queries GitHub releases API; compares against running semver.           |
| `PopupQueue` / `PopupCreateSink` | Thread-safe queues for `window.open` / popup lifecycle routing.         |

## Usage

```toml
# Cargo.toml (workspace path dep)
buffr-core = { path = "crates/buffr-core" }
```

```rust,no_run
// pseudo-code — see apps/buffr/src/main.rs for the full wiring

use buffr_core::{BuffrApp, BrowserHost, init_cef_api, profile_paths};

// 1. Pin CEF API version — must be first.
init_cef_api();

// 2. Dispatch subprocess roles (renderer/GPU/utility).
let args = cef::args::Args::new();
let code = cef::execute_process(Some(args.as_main_args()), Some(&mut BuffrApp::new()), std::ptr::null_mut());
if code >= 0 { std::process::exit(code); }

// 3. Resolve profile paths and initialise CEF.
let paths = profile_paths().expect("project dirs");
// ... cef::initialize(settings, &mut app, ...) ...

// 4. Create a tab host and open the first tab.
// BrowserHost::new(...) accepts Arc<History>, Arc<Downloads>, Arc<Bookmarks>,
// Arc<Permissions>, Arc<ZoomStore>, config, keymap, etc.
```

## Modules

| Module        | Contents                                                                                                                                             |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app`         | `BuffrApp`, `ProfilePaths`, CEF command-line processing.                                                                                             |
| `handlers`    | All CEF handler impls: `LoadHandler`, `DisplayHandler`, `KeyboardHandler`, `DownloadHandler`, `PermissionHandler`, `LifeSpanHandler`, `FindHandler`. |
| `host`        | `BrowserHost`, `Tab`, `TabId`, `TabSession`, `TabSummary`, `HintStatus`.                                                                             |
| `hint`        | Hint-mode JS injection, label arithmetic, `HintSession`, `HintAlphabet`.                                                                             |
| `edit`        | Edit-mode console-IPC sentinel, `EditConsoleEvent`, `EditEventSink`.                                                                                 |
| `find`        | Find-in-page `FindResult` slot + `new_sink` / `take_latest`.                                                                                         |
| `osr`         | Off-screen rendering frame buffer (`OsrFrame`, `SharedOsrFrame`).                                                                                    |
| `permissions` | `PendingPermission`, `PermissionsQueue`, decision precheck.                                                                                          |
| `new_tab`     | `buffr://new` scheme handler; new-tab HTML + keymap renderer.                                                                                        |
| `telemetry`   | `UsageCounters` — local-only opt-in event counters.                                                                                                  |
| `crash`       | `CrashReporter` — panic hook + JSON report writer.                                                                                                   |
| `updates`     | `UpdateChecker`, `ReleaseInfo`, `HttpClient` trait, `UreqClient`.                                                                                    |
| `cmdline`     | Clap-free argv parser for utility flags (`--list-permissions`, …).                                                                                   |

## CEF API version pinning

Every binary that touches CEF (browser process **and** helper) must call
`init_cef_api()` before any CEF entry point. See the doc-comment on that
function for the gory details. This is the single biggest footgun in the CEF
Rust binding.

## License

MIT. See [LICENSE](../../LICENSE).
