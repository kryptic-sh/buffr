# buffr

Vim-modal browser. Native shell, GPU-accelerated compositing via CEF. Keyboard
first. No Electron. No web UI for chrome.

This site is the user-facing docs surface. The chapter list on the left covers:

- **Getting started** — build from source, run the dev tree.
- **Running on macOS** — Homebrew prerequisites, CEF vendoring, and direct
  `cargo run` behavior for local Mac development.
- **Configuration** — every section of `config.toml`: `[general]`, `[startup]`,
  `[search]`, `[theme]`, `[privacy]`, `[downloads]`, `[hint]`,
  `[crash_reporter]`, `[updates]`, `[accessibility]`, `[idle_inhibit]`,
  `[engines]`, `[keymap]`.
- **Keymap** — every default page-mode binding, with a reference for the
  vim-flavoured action grammar.
- **Multi-tab** — multi-tab `BrowserHost`, session restore, pinned tabs.
- **Hint mode** — `f`/`F` follow-by-letter overlay.
- **Context menu** — buffr's own right-click menu, and `buffr-src:` view-source.
- **Updates** — the once-a-day GitHub release check, opt-out, and the manual
  `--check-for-updates` CLI.
- **Privacy** — what buffr stores, what it never does, and the one network
  request it makes by default. Telemetry is opt-in and local-only; there is no
  collector to send it to.
- **Accessibility** — CEF renderer accessibility, keyboard-first chrome,
  high-contrast theme.
- **Packaging** — Linux `.deb` / `.rpm` / `.tar.gz` / AUR / Flatpak / Snap;
  macOS `.app` + `.dmg`; Windows MSI.
- **macOS signing** — Developer-ID + notarization plan (not yet implemented).
- **Windows packaging** — the WiX 3 MSI layout.
- **UI stack ADR** — why CEF off-screen rendering composited with `wgpu` in one
  winit window, instead of a separate chrome window or a CPU-blitted strip.

Source repo: <https://github.com/kryptic-sh/buffr>.

## Install

**macOS (Homebrew)**

```bash
brew install --cask kryptic-sh/tap/buffr
```

**Arch Linux (AUR)**

```bash
paru -S buffr-bin
```

Pre-built binaries for Windows (MSI), Debian/Ubuntu (deb), Fedora/RHEL (rpm),
Snap, and Flatpak are available on the
[releases page](https://github.com/kryptic-sh/buffr/releases).
