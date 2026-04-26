# buffr — privacy

Two opt-in surfaces — telemetry counters and the crash reporter — both off by
default and both **local-only**. buffr never sends data to a network endpoint.
Not now, not ever, not even to a kryptic-owned server. The implementation is a
deliberate no-op that documents the design rather than a stub waiting for an
endpoint.

## Telemetry — opt-in usage counters

Off by default. Set `[privacy] enable_telemetry = true` in `config.toml` to opt
in. When enabled, buffr writes anonymous integer counters to:

```
~/.local/share/buffr/usage-counters.json
```

(macOS: `~/Library/Application Support/sh.kryptic.buffr/usage-counters.json`;
Windows: `%APPDATA%\kryptic\buffr\data\usage-counters.json`.)

The file is pretty-printed JSON. After one app start it looks like:

```json
{
  "app_starts": 1
}
```

Counters tracked:

| Key                   | Increments on                                                |
| --------------------- | ------------------------------------------------------------ |
| `app_starts`          | Successful CEF init.                                         |
| `tabs_opened`         | Every `BrowserHost::open_tab` (foreground + background).     |
| `pages_loaded`        | Every main-frame `LoadHandler::on_load_end`.                 |
| `searches_run`        | Omnibar input that falls through to the search-engine route. |
| `bookmarks_added`     | `:bookmark` cmdline (Netscape import is intentionally not).  |
| `downloads_completed` | `DownloadHandler` reports `is_complete()`.                   |

Counters flush every 60 s in the background plus once at clean shutdown. There
is **no** code path that opens a network socket for telemetry — there is no
endpoint to disable, no opt-out flag to flip; the network surface simply does
not exist.

If you want to share counters with someone, write a script that reads the JSON
and `curl`s it to wherever you choose. buffr will not do this for you.

CLI:

```sh
buffr --telemetry-status   # print enabled/disabled, path, and current counts
buffr --reset-telemetry    # truncate counters to {}
```

`--private` mode forces telemetry off regardless of the config flag — the whole
point of `--private` is "leave no traces".

## Crash reporter — opt-in local panic capture

Off by default. Set `[crash_reporter] enabled = true` to opt in. When enabled,
buffr installs a `std::panic::set_hook` that captures the panic message,
panic-site location, and a `Backtrace::force_capture` (always on, regardless of
`RUST_BACKTRACE`) and writes a JSON report to:

```
~/.local/share/buffr/crashes/<RFC3339-timestamp>.json
```

Filename pattern: `YYYY-MM-DDTHH-MM-SS.sssZ.json` (colons swapped for dashes so
the path is portable to FAT/Windows).

CEF's `BrowserProcessHandler` does **not** expose an `on_uncaught_exception`
callback in libcef-147 — the only `on_uncaught_exception` is on the renderer-
process `RenderProcessHandler` and only fires for V8 exceptions (JavaScript
errors). Native CEF crashes are caught by Chromium's internal crashpad/ breakpad
pipeline, which buffr does not currently configure (it requires a
`crashpad_handler` binary plus a symbol-server URL — both Phase 7 work). Phase 6
ships the panic-hook reporter only.

Reports are kept locally. Inspect them by hand:

```sh
buffr --list-crashes   # one line per report: <ts>\t<version>\t<location>\t<msg>
buffr --purge-crashes  # delete reports older than crash_reporter.purge_after_days
```

If you want to send a report to someone, mail the JSON file. buffr never
uploads.
