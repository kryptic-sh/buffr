# buffr — Packaging

Phase 6 lands distribution artifacts for all three tier-1 targets, all unsigned
in this round (signing infrastructure is the next step):

| Platform | Driver                            | Output                                             |
| -------- | --------------------------------- | -------------------------------------------------- |
| Linux    | `cargo xtask package-linux`       | `.deb` + `.rpm` + `.tar.gz` + AUR PKGBUILD         |
| Linux    | `flatpak-builder` (CI)            | `.flatpak` (single-file bundle)                    |
| Linux    | `snapcraft` (CI)                  | `.snap` (classic confinement, single-file bundle)  |
| macOS    | `cargo xtask package-macos-dmg`   | `target/dist/macos/buffr-<ver>-arm64.dmg`          |
| Windows  | `cargo xtask package-windows-msi` | `target/dist/windows/buffr-<ver>-<x64\|arm64>.msi` |

The macOS bundle assembly (driving the DMG) lives in
[`docs/macos-signing.md`](./macos-signing.md); the Windows MSI flow has its own
[`docs/windows-packaging.md`](./windows-packaging.md). The rest of this document
covers Linux end-to-end.

## Linux

Phase 6 ships three Linux distribution paths, all producible from a single Linux
dev box:

| Format   | Tooling               | Audience                          |
| -------- | --------------------- | --------------------------------- |
| AppImage | `appimagetool`        | Distro-agnostic single-file blob. |
| `.deb`   | `dpkg-deb`            | Debian / Ubuntu / Mint.           |
| PKGBUILD | `makepkg` (user-side) | Arch / Manjaro / EndeavourOS.     |

None of these are **signed** in this round. Signing lives in the release
pipeline (Phase 6, separate trust-store work). The artifacts here are
installable but Gatekeeper-equivalent prompts will warn the user.

## Building all three

```sh
cd buffr
cargo xtask fetch-cef                # vendor CEF if not already
cargo xtask package-linux --release  # default --variant all
ls target/dist/linux/
```

You'll get:

```
target/dist/linux/
├── buffr-0.0.1-x86_64.AppImage      # ~350 MiB squashfs
└── buffr-0.0.1-amd64.deb            # ~330 MiB
```

The PKGBUILD is written to `pkg/aur/PKGBUILD` (in-tree, not under `target/`) —
its version field is rewritten to match `[workspace.package] version` on every
run.

### Variant flags

```sh
cargo xtask package-linux --variant appimage
cargo xtask package-linux --variant deb
cargo xtask package-linux --variant aur
cargo xtask package-linux --variant all      # default
```

Add `--release` to use the release-profile binaries; without it the debug
binaries land in the package (slow, large, useful for smoke testing the bundle
scripts).

### Tooling fall-back

`appimagetool` and `dpkg-deb` are auto-detected:

1. **`appimagetool`** — checked on `$PATH` first; falls back to
   `vendor/appimagetool/appimagetool-x86_64.AppImage`; if neither exists,
   downloaded from the upstream `continuous` release. The download is cached in
   `vendor/appimagetool/` and CI keys an `actions/cache@v4` entry off it. If the
   tool can't be obtained at all (no internet), the `buffr.AppDir` staging
   directory is left in place under `target/<profile>/` and a warning is
   printed.
2. **`dpkg-deb`** — checked on `$PATH`. If absent (Arch / Fedora hosts without
   the `dpkg` package), the staging tree at `target/<profile>/buffr-deb/` is
   left in place and a warning is printed. The `.deb` itself is not produced.

## AppImage

```sh
chmod +x target/dist/linux/buffr-*-x86_64.AppImage
./target/dist/linux/buffr-*-x86_64.AppImage
```

The AppImage embeds:

- `usr/bin/buffr` + `usr/bin/buffr-helper`
- `usr/lib/libcef.so` + `*.pak` + `icudtl.dat` + `v8_context_snapshot.bin`
- `usr/lib/locales/<lang>.pak`
- `AppRun` launcher (sets `LD_LIBRARY_PATH` and execs `usr/bin/buffr`)
- `buffr.desktop` + `buffr.png` (placeholder icon)

### Glibc requirement

The bundled CEF expects **glibc >= 2.28**. Distros older than the following will
fail at load time with `version 'GLIBC_2.28' not found`:

- Ubuntu 18.04 (glibc 2.27) — **not supported**
- Ubuntu 20.04+ — supported
- Debian 10 (Buster, glibc 2.28) — supported
- Debian 11+ — supported
- RHEL 8+ — supported

### Fuse / `--appimage-extract`

If the host doesn't have `libfuse2` installed, AppImages fail with
`/dev/fuse: Permission denied`. Workaround:
`./buffr-*.AppImage --appimage-extract` produces a `squashfs-root/` directory
you can run `./squashfs-root/AppRun` out of.

## `.deb`

```sh
sudo dpkg -i target/dist/linux/buffr-*-amd64.deb
sudo apt-get install -f      # auto-resolve any missing depends
```

Layout on disk:

```
/opt/buffr/                          (binaries + CEF runtime payload)
├── buffr                            (main exe; rpath=$ORIGIN finds libcef.so)
├── buffr-helper
├── libcef.so
├── *.pak / icudtl.dat / v8_context_snapshot.bin
├── locales/
└── icon.png
/usr/share/applications/buffr.desktop
/usr/share/icons/hicolor/512x512/apps/buffr.png
/usr/local/bin/buffr -> /opt/buffr/buffr   (postinst symlink)
```

The `postinst` hook also refreshes `gtk-update-icon-cache` and
`update-desktop-database` best-effort — missing tooling is not an error. The
`prerm` hook removes the `/usr/local/bin/buffr` symlink if (and only if) it
still points back at `/opt/buffr/buffr`.

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
gtk3 nss libxss alsa-lib libgbm libxshmfence libxkbcommon
libxkbcommon-x11 libglvnd
```

`libglvnd` provides `libGLES.so.2` — Arch's equivalent of Debian's `libgles2`.

## Sandbox caveat

CEF on Linux uses a SUID sandbox helper by default. Both the AppImage and the
`.deb` ship the unprivileged binary; CEF will fall back to the **namespace
sandbox** if the kernel supports `unprivileged_userns_clone` (default on every
distro since 2018). On hosts where that's been turned off (some hardened-kernel
distros, or `sysctl kernel.unprivileged_userns_clone=0`), buffr will warn and
continue without sandboxing. To re-enable, the sysadmin needs to flip the sysctl
or the package needs to ship a SUID helper at `/opt/buffr/chrome-sandbox` —
Phase 6+ work.

## Icon — placeholder

`pkg/buffr.png` is a 512×512 placeholder generated with ImageMagick (`#7aa2f7`
lowercase "b" on `#1a1a1a`). The real icon will live at the same path; the
AppImage / `.deb` / PKGBUILD all point at it. Replacing the file and re-running
`cargo xtask package-linux` is enough to ship a new icon.

## CI

The `linux-package` job in `.github/workflows/ci.yml` runs the full
`cargo xtask package-linux --release --variant all` pipeline on every PR. It:

- caches the CEF binary distribution (~480 MiB extracted),
- caches the downloaded `appimagetool` binary,
- runs `dpkg-deb -I` against the produced `.deb` to assert valid metadata,
- asserts the AppImage is an executable ELF (it's an `AppImage` magic squashfs
  ELF, so `file` recognises ELF).

No artifacts are uploaded — the Phase 6 release pipeline replaces this with
proper artifact retention.

## macOS

`cargo xtask bundle-macos --release` assembles `buffr.app` (with the four-helper
layout — see [`macos-signing.md`](./macos-signing.md)).
`cargo xtask package-macos-dmg --release` then wraps it into
`target/dist/macos/buffr-<ver>-<arch>.dmg` via `hdiutil create … -format UDZO`
(macOS hosts) or `genisoimage` (Linux fallback, smoke testing only).

The DMG embeds:

- `buffr.app/` (full bundle, including all four helpers + CEF framework)
- `Applications -> /Applications` symlink (drag-target)

Unsigned in this round. After download, first-run users must clear the
quarantine xattr Gatekeeper attaches:

```sh
xattr -d com.apple.quarantine /Applications/buffr.app
```

The CI `macos-package` job runs the full pipeline on a `macos-latest` runner and
uploads the DMG as a build artifact. Signing + notarization land in the eventual
`release.yml` workflow.

## Windows

`cargo xtask package-windows-msi --release` produces
`target/dist/windows/buffr-<ver>-x64.msi` from a hand-rolled WiX 3 source
(`xtask/templates/buffr.wxs`). Full layout, registry directives, uninstall
behaviour, and cross-build prerequisites are documented in
[`windows-packaging.md`](./windows-packaging.md).

Unsigned in this round. SmartScreen will warn the user on first run until
Authenticode signing lands.

The CI `windows-package` job runs the full pipeline on a `windows-latest` runner
with the WiX 3 toolset installed and uploads the MSI as a build artifact.

## Flatpak

`flatpak/sh.kryptic.buffr.yml` builds a single-file `.flatpak` bundle from the
runtime tarball emitted by `cargo xtask package-linux --variant tarball`. CI
extracts the tarball into `flatpak/payload/`, invokes `flatpak-builder`, and
uploads `buffr-<ver>-<arch>.flatpak` to the GitHub release. Users install with:

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
us as a GNOME app. CEF supports portal-based file pickers via the
`--enable-features=DesktopPortalFileChooser` switch (set in
`crates/buffr-core/src/app.rs`'s command-line setup); the printing and color
picker paths still need investigation. Tracked separately because it affects the
.deb and .rpm runtime deps too — if we patch CEF / disable GTK fallbacks, the
deb's `libgtk-3-0` Depends and the rpm's `gtk3` Requires can drop.

## Snap

`snap/snapcraft.yaml` builds a `.snap` bundle from the same runtime tarball the
flatpak job uses. CI extracts the tarball into `snap/payload/` and runs
`snapcore/action-build@v1`, which boots an LXD VM, runs snapcraft, and emits
`buffr-<ver>-<arch>.snap`. Users install with:

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
