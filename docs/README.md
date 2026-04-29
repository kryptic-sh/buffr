# buffr

Vim-modal browser. Native shell, GPU-accelerated compositing via CEF. Keyboard
first. No Electron. No web UI for chrome.

This site is the user-facing docs surface. The chapter list on the left covers:

- **Getting started** — build from source, run the dev tree.
- **Running on macOS** — Homebrew prerequisites, CEF vendoring, and direct
  `cargo run` behavior for local Mac development.
- **Configuration** — the `[general]`, `[search]`, `[theme]`, `[privacy]`,
  `[updates]`, `[accessibility]`, `[keymap]` sections.
- **Keymap** — every default page-mode binding, with a reference for the
  vim-flavoured action grammar.
- **Multi-tab** — multi-tab `BrowserHost`, session restore, pinned tabs.
- **Hint mode** — `f`/`F` follow-by-letter overlay.
- **Updates** — the once-a-day GitHub release check, opt-out, and the
  manual `--check-for-updates` CLI.
- **Privacy** — what buffr stores, what it never does, and the one network
  request it makes by default.
- **Accessibility** — CEF renderer accessibility, keyboard-first chrome,
  high-contrast theme.
- **Packaging** — Linux AppImage / `.deb` / AUR; macOS `.app` + `.dmg`;
  Windows MSI.
- **macOS signing** — Developer-ID + notarization plan.
- **UI stack ADR** — why winit + softbuffer for chrome instead of full OSR.

Source repo: <https://github.com/kryptic-sh/buffr>.
