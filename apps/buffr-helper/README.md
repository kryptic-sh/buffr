# buffr-helper

CEF subprocess helper binary (renderer / GPU / utility processes).

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

## Why it exists

CEF spawns separate OS processes for renderer, GPU, and utility work. On Linux
and Windows the main `buffr` binary re-launches itself with `--type=renderer` /
`--type=gpu-process` / `--type=utility` — CEF detects the flag via
`cef::execute_process` and the subprocess exits immediately after its work is
done.

On macOS, Chromium requires a **distinct** executable under
`Contents/Frameworks/buffr Helper.app/Contents/MacOS/`. `buffr-helper` fills
that role so the macOS app bundle is structured correctly. The main binary still
works as its own helper on Linux and Windows.

## What it does

1. On macOS: loads `Chromium Embedded Framework.framework` via
   `cef::LibraryLoader`.
2. Pins the CEF API version via `buffr_core::init_cef_api()`.
3. Calls `cef::execute_process` — CEF identifies the subprocess type from argv
   and runs it.
4. Exits with whatever code CEF returns.

That's the entire binary. No window, no event loop, no config.

## Users don't run this directly

`buffr-helper` is invoked automatically by the CEF browser process. Running it
by hand produces a harmless immediate exit (CEF sees no `--type` flag and
returns `-1`).

## License

MIT. See [LICENSE](../../LICENSE).
