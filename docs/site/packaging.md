# buffr — Packaging

Distribution artifacts for all three tier-1 targets. Everything below is
**unsigned** — no signing step exists in any workflow yet:

| Platform | Driver                            | Output                                             |
| -------- | --------------------------------- | -------------------------------------------------- |
| Linux    | `cargo xtask package-linux`       | `.deb` + `.rpm` + `.tar.gz` + AUR PKGBUILD         |
| Linux    | `flatpak-builder` (CI)            | `.flatpak` (single-file bundle)                    |
| Linux    | `snapcraft` (CI)                  | `.snap` (classic confinement, single-file bundle)  |
| macOS    | `cargo xtask package-macos-dmg`   | `target/dist/macos/buffr-<ver>-arm64.dmg`          |
| Windows  | `cargo xtask package-windows-msi` | `target/dist/windows/buffr-<ver>-<x64\|arm64>.msi` |

The macOS bundle assembly (driving the DMG) lives in
[`docs/site/macos-signing.md`](./macos-signing.md); the Windows MSI flow has its
own [`docs/site/windows-packaging.md`](./windows-packaging.md). The rest of this
document covers Linux end-to-end.

## Linux

`cargo xtask package-linux` ships four Linux distribution paths, all producible
from a single Linux dev box:

| Format    | Tooling               | Audience                       |
| --------- | --------------------- | ------------------------------ |
| `.deb`    | `dpkg-deb`            | Debian / Ubuntu / Mint.        |
| `.rpm`    | `rpmbuild`            | Fedora / RHEL / openSUSE.      |
| `.tar.gz` | `tar`                 | Distro-agnostic portable tree. |
| PKGBUILD  | `makepkg` (user-side) | Arch / Manjaro / EndeavourOS.  |

Flatpak and Snap bundles are produced by CI, not by the xtask — see
[Flatpak](#flatpak) and [Snap](#snap) below.

None of these are **signed**. Signing is separate trust-store work that has not
landed; the artifacts here are installable but Gatekeeper-equivalent prompts
will warn the user.

## Building all four

```sh
cd buffr
cargo xtask fetch-cef                # vendor CEF if not already
cargo xtask package-linux --release  # default --variant all
ls target/dist/linux/
```

You'll get:

```
target/dist/linux/
├── buffr-<version>-amd64.deb
├── buffr-<version>-x86_64.rpm
└── buffr-<version>-x86_64.tar.gz
```

`<version>` is `[workspace.package] version` from the root `Cargo.toml`, stamped
in by the xtask.

The PKGBUILD is written to `pkg/aur/PKGBUILD` (in-tree, not under `target/`) —
its `pkgver` field is rewritten to match the workspace version on every run.

### Variant flags

```sh
cargo xtask package-linux --variant deb
cargo xtask package-linux --variant rpm
cargo xtask package-linux --variant tarball   # `tar` is accepted too
cargo xtask package-linux --variant aur
cargo xtask package-linux --variant all       # default
```

Anything else is rejected by `LinuxVariant::parse` in `xtask/src/main.rs`.

Add `--release` to use the release-profile binaries; without it the debug
binaries land in the package (slow, large, useful for smoke testing the bundle
scripts).

### Tooling fall-back

`dpkg-deb` is checked on `$PATH`. If absent (Arch / Fedora hosts without the
`dpkg` package), the staging tree at `target/<profile>/buffr-deb/` is left in
place and a warning is printed; the `.deb` itself is not produced. `rpmbuild` is
checked the same way — `xtask: rpmbuild not on PATH; skipping rpm build` — and
the tarball leg shells out to `tar`. A missing tool never fails the run.

## `.deb`

```sh
sudo dpkg -i target/dist/linux/buffr-*-amd64.deb
sudo apt-get install -f      # auto-resolve any missing depends
```

Layout on disk:

```
/opt/buffr/                          (binaries + CEF runtime payload)
├── buffr                            (supervisor — Linux entrypoint)
├── buffr-app                        (browser; rpath=$ORIGIN finds libcef.so)
├── buffr-helper                     (CEF subprocess helper)
├── libcef.so
├── *.pak / icudtl.dat / v8_context_snapshot.bin
├── locales/
└── icon.png
/usr/share/applications/buffr.desktop
/usr/share/icons/hicolor/512x512/apps/buffr.png
/usr/local/bin/buffr     -> /opt/buffr/buffr      (postinst symlinks)
/usr/local/bin/buffr-app -> /opt/buffr/buffr-app
/usr/local/bin/buffr-helper -> /opt/buffr/buffr-helper
```

The `postinst` hook also refreshes `gtk-update-icon-cache` and
`update-desktop-database` best-effort — missing tooling is not an error. The
`prerm` hook removes the `/usr/local/bin/buffr*` symlinks if (and only if) they
still point back at `/opt/buffr/`.

### Apt depends

```
libgtk-3-0, libnss3, libxss1, libasound2, libgbm1,
libxshmfence1, libxkbcommon0, libxkbcommon-x11-0, libgles2
```

`libgtk-3-0` transitively brings in `libatk-1.0-0`, `libatk-bridge-2.0-0`,
`libpango-1.0-0`, `libcairo2`, `libdbus-1-3`, `libdrm2`, `libxcomposite1`,
`libxdamage1`, `libxrandr2`, `libxext6`, `libxfixes3`, `libxrender1` — so we
don't list those explicitly. `libnspr4` and `libcups2` are pulled by `libcef.so`
directly but ship as default-installed on every modern Debian/Ubuntu desktop
image. If you hit a `libnspr4.so` / `libcups.so.2` not-found error on a minimal
container, `sudo apt-get install -f` resolves it.

### Signing

Not done in this round. To sign locally:

```sh
dpkg-sig --sign builder target/dist/linux/buffr-*-amd64.deb
```

You need a GPG key the user has imported. CI release signing is Phase 6
follow-up.

## AUR PKGBUILD

The PKGBUILD assumes a **tagged release on GitHub** at
`https://github.com/kryptic-sh/buffr/archive/v${pkgver}.tar.gz`. Until a tag
actually ships, `makepkg` will 404. The `sha256sums=('SKIP')` entry is
intentional — replace it with the tarball's real digest at release time:

```sh
updpkgsums pkg/aur/PKGBUILD
```

`pkgver` is rewritten on every `cargo xtask package-linux` invocation to match
`[workspace.package].version`; manual edits are clobbered.

### Local install

Copy `pkg/aur/PKGBUILD` (and `pkg/buffr.desktop` + `pkg/buffr.png`, which the
`package()` step references) to a clean dir and:

```sh
makepkg -si
```

### makedepends

```
rust cargo cmake
```

Plus the runtime depends:

```
gtk3 nss libxss alsa-lib mesa libxshmfence libxkbcommon
libxkbcommon-x11 libglvnd
```

`libglvnd` provides `libGLES.so.2` — Arch's equivalent of Debian's `libgles2`.

## Sandbox caveat

CEF on Linux uses a SUID sandbox helper by default. Every Linux package here
ships the unprivileged binary and **no** `chrome-sandbox` helper; CEF falls back
to the **namespace sandbox** when the kernel allows unprivileged user
namespaces. On hosts where that has been turned off (some hardened-kernel
distros), buffr will warn and continue without sandboxing. Re-enabling means
either flipping the sysctl or shipping a SUID helper at
`/opt/buffr/chrome-sandbox` — the latter is not implemented.

## Icon — placeholder

`pkg/buffr.png` is a 512×512 placeholder generated with ImageMagick (`#7aa2f7`
lowercase "b" on `#1a1a1a`). The real icon will live at the same path; the
`.deb`, `.rpm`, tarball, PKGBUILD, and the flatpak job all point at it.
Replacing the file and re-running `cargo xtask package-linux` is enough to ship
a new icon.

## CI

The `linux-package` job in `.github/workflows/ci.yml` runs the full
`cargo xtask package-linux --release --variant all` pipeline. It is **skipped on
pull requests** (`if: github.event_name != 'pull_request'`) and runs on pushes
to `main`, on `v*` tags, and on `workflow_dispatch`. The same gate applies to
`macos-package`, `windows-package`, `flatpak`, and `snap`. It:

- caches the CEF binary distribution,
- runs `dpkg-deb -I` against the produced `.deb` and `rpm -qpi` against the
  `.rpm` to assert valid metadata,
- validates the tarball and smoke-tests the extracted binaries,
- uploads the `.deb` / `.rpm` / `.tar.gz` plus `.sha256` sidecars as workflow
  artifacts (`if-no-files-found: error`).

Release publishing lives in the same workflow, not a separate one: on a `v*`
tag, `publish-github-release` gathers every packaging job's artifacts and
attaches them to the GitHub release, then `aur-bin`, `brew-tap`, and
`scoop-bucket` push the downstream manifests.

## macOS

`cargo xtask bundle-macos --release` assembles `Buffr.app` (with the four-helper
layout — see [`macos-signing.md`](./macos-signing.md)).
`cargo xtask package-macos-dmg --release` then wraps it into
`target/dist/macos/buffr-<ver>-<arch>.dmg` via `hdiutil create … -format UDZO`
(macOS hosts) or `genisoimage` (Linux fallback, smoke testing only).

The DMG embeds:

- `Buffr.app/` (full bundle, including all four helpers + CEF framework)
- `Applications -> /Applications` symlink (drag-target)

Unsigned in this round. After download, first-run users must clear the
quarantine xattr Gatekeeper attaches:

```sh
xattr -d com.apple.quarantine /Applications/Buffr.app
```

The CI `macos-package` job runs the full pipeline on a `macos-latest` runner and
uploads the DMG as a build artifact. Signing + notarization are **not
implemented** anywhere in CI yet; see [`macos-signing.md`](./macos-signing.md)
for the plan.

## Windows

`cargo xtask package-windows-msi --release` produces
`target/dist/windows/buffr-<ver>-<x64|arm64>.msi` from a hand-rolled WiX 3
source (`xtask/templates/buffr.wxs`). Full layout, registry directives,
uninstall behaviour, and cross-build prerequisites are documented in
[`windows-packaging.md`](./windows-packaging.md).

Unsigned. SmartScreen will warn the user on first run until Authenticode signing
lands.

The CI `windows-package` job runs the full pipeline over a two-entry matrix —
`windows-latest` / `x86_64-pc-windows-msvc` and `windows-11-arm` /
`aarch64-pc-windows-msvc` — with the WiX 3 toolset installed, and uploads a
`.msi` plus a `.zip` per arch as build artifacts.

## Flatpak

`flatpak/sh.kryptic.buffr.yml` builds a single-file `.flatpak` bundle from the
runtime tarball emitted by `cargo xtask package-linux --variant tarball`. CI
extracts the tarball into `flatpak/payload/` (the manifest's module is
`type: dir, path: payload`), invokes `flatpak-builder`, and attaches
`buffr-<ver>-<arch>.flatpak` to the GitHub release. Users install with:

```sh
flatpak install --user ./buffr-<ver>-amd64.flatpak
```

Runtime is `org.gnome.Platform//47`. We don't link GTK from buffr's own code
(the chrome is wgpu + winit + a bitmap font), but `libcef.so` itself depends on
`libgtk-3.so.0` for Chromium's native dialogs (file picker, color picker,
printing). The GNOME Platform provides GTK3 from a shared layer, so we don't
have to bundle it.

`finish-args` mirrors the Brave/Vivaldi flatpaks closely — Wayland + fallback
X11 + pulseaudio + DRI for GPU + narrow xdg-config/data/cache filesystem access

- DBus name reservations for MPRIS and notifications. CEF subprocess helpers run
  inside the same sandbox via plain `execve`; no `flatpak-spawn` shim is needed.

### Phase 2 — Flathub

The current manifest is correct for direct-bundle distribution but not for
Flathub submission. Phase 2 work, deferred:

- Replace `type: dir, path: payload` with a `type: archive, url: <release URL>`
  - `sha256` entry — Flathub requires reproducible network sources.
- Add `<release>` and `<screenshots>` entries to the AppStream metainfo.
- Verify `--filesystem=xdg-config/buffr` is the narrowest set Flathub accepts.

### Future — drop GTK dependency (option 3)

Long-term, we'd like to swap to `org.freedesktop.Platform//24.08` and route all
native dialogs through `xdg-desktop-portal` so the flatpak base doesn't identify
us as a GNOME app. CEF supports portal-based file pickers via an
`--enable-features=DesktopPortalFileChooser` switch, which buffr does **not**
currently pass — the command-line hook is
`crates/buffr-cef/src/app.rs::on_before_command_line_processing` and the switch
is absent from it. The printing and colour-picker paths still need
investigation. Tracked separately because it affects the .deb and .rpm runtime
deps too — if we patch CEF / disable GTK fallbacks, the deb's `libgtk-3-0`
Depends and the rpm's `gtk3` Requires can drop.

## Snap

`snap/snapcraft.yaml` builds a `.snap` bundle from the same runtime tarball the
flatpak job uses. CI extracts the tarball into `payload/` at the **repo root**
(snapcraft resolves `source: payload` relative to the project root, not the
`snap/` directory) and runs `snapcore/action-build@v1`, which boots an LXD VM,
runs snapcraft, and emits `buffr-<ver>-<arch>.snap`. Users install with:

```sh
snap install --dangerous --classic ./buffr-<ver>-amd64.snap
```

Phase 1 ships **classic confinement** because that's the simplest path for
ad-hoc distribution. Until the Snap Store registration is filed, the snap is
bundled on GitHub Releases.

### Phase 2 — Snap Store + strict confinement

Modern Chromium-based snaps (Firefox, Brave, Chromium, Edge, Vivaldi) all run
**strict** confinement with the `browser-support` interface — classic for a
browser is unconventional today and likely to be flagged by Snap Store
reviewers. Phase 2 redesign:

```yaml
confinement: strict
extensions:
  - gnome # GTK3 + portal integration shared from the host
apps:
  buffr:
    plugs:
      - browser-support
      - network
      - network-bind
      - audio-playback
      - audio-record
      - opengl
      - x11
      - wayland
      - desktop
      - desktop-legacy
      - gsettings
      - removable-media
      - screen-inhibit-control
```

The `gnome` extension shares GTK3 with the host instead of bundling it inside
the snap (saves ~150 MB). Tracked alongside the flatpak option-3 work since both
touch CEF's GTK use.
