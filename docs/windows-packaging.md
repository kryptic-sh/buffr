# buffr — Windows packaging (MSI)

Phase 6 ships an MSI installer for Windows 10+. Like the Linux `.deb` / AppImage
and the macOS `.dmg`, it is **unsigned** in this round; Authenticode signing
lives in the post-Phase-6 release pipeline.

## Driver

```sh
cargo xtask package-windows-msi --release
ls target/dist/windows/
# buffr-<version>-x64.msi
# buffr.wxs
# payload/  (binaries + libcef.dll + paks + locales/)
```

Internally:

1. Render `xtask/templates/buffr.wxs` with `{VERSION}` / `{INSTALL_DIR}` /
   `{ARCH}` substituted and write to `target/dist/windows/buffr.wxs`.
2. Locate `buffr.exe`, `buffr-helper.exe`, `libcef.dll`, `icudtl.dat`, `*.pak`,
   and `locales/` from one of:
   - `target/<profile>/` (native Windows host),
   - `target/x86_64-pc-windows-msvc/<profile>/` (cross from Windows),
   - `target/x86_64-pc-windows-gnu/<profile>/` (Linux cross — see below).
3. Stage the payload under `target/dist/windows/payload/`.
4. Run `candle.exe` (XML → `.wixobj`) and `light.exe` (`.wixobj` → `.msi`) from
   the [WiX 3 toolset](https://github.com/wixtoolset/wix3/releases).

## WiX version

The `.wxs` targets the **WiX 3** namespace
(`http://schemas.microsoft.com/wix/2006/wi`) with `<Product>` at the root. WiX 3
tooling is the most broadly available baseline today; WiX 4 / 5 changed the
namespace, renamed root elements, and shipped a unified `wix.exe` driver. The
older `candle` + `light` are still on every CI Windows runner, and they produce
identical MSIs for our needs (no per-user install, no MSIX, no bundle).

## Install layout

```
C:\Program Files\buffr\
├── buffr.exe
├── buffr-helper.exe
├── libcef.dll
├── icudtl.dat
├── *.pak
└── locales\
```

Plus:

- Start menu shortcut: `Programs\buffr\buffr.lnk`
- Desktop shortcut: `Desktop\buffr.lnk`
- Registry entry under `HKLM\SOFTWARE\kryptic\buffr` recording `InstallPath` and
  `Version`.

## Uninstall

WiX `<RemoveFolder>` and `<RemoveRegistryKey Action="removeOnUninstall">`
directives ensure clean removal:

- The `Program Files\buffr\` directory and its contents are deleted.
- The Start menu shortcut + desktop shortcut are removed.
- The HKLM registry hive (`SOFTWARE\kryptic\buffr`) is deleted.
- The HKCU keypaths used to anchor shortcut components are removed for the
  installing user (other users keep theirs — by design).

`MajorUpgrade` is configured so installing a newer version automatically removes
the old one before laying down the new payload.

## Cross-build prerequisites (Linux → Windows)

If you want to produce the MSI from a Linux dev box without a Windows VM:

1. Add the cross target: `rustup target add x86_64-pc-windows-gnu`
2. Install MinGW: `pacman -S mingw-w64-gcc` (Arch) /
   `apt-get install gcc-mingw-w64-x86-64` (Debian).
3. Cross-build:
   `cargo build --target x86_64-pc-windows-gnu --release -p buffr -p buffr-helper`.
4. Run `cargo xtask package-windows-msi --release` — it will pick up the
   cross-target output automatically.

**Caveat:** CEF-147 binary distributions are built against MSVC and link against
the Microsoft C runtime; the `cef` crate's `libcef.lib` import library is
MSVC-format. Cross-linking from MinGW (`x86_64-pc-windows-gnu`) against an MSVC
`libcef.lib` is not officially supported and may fail at link time. The reliable
path is a native Windows host with the Visual Studio Build Tools installed. The
CI `windows-package` job uses the `windows-latest` GitHub-hosted runner (which
has VS Build Tools preinstalled) for the same reason.

## Tooling fall-back

Both `candle.exe` and `light.exe` are auto-detected on `PATH`. If either is
missing the script stops after writing `target/dist/windows/buffr.wxs` (and the
payload tree, if Windows binaries exist) and prints a warning. CI on the
`windows-latest` runner installs the WiX 3 toolset and exercises the full build.

If the Windows payload itself is unavailable (running on a fresh Linux host
without a cross-build), `cargo xtask package-windows-msi` still writes the
`buffr.wxs` source to `target/dist/windows/` for inspection — the MSI step is
skipped with a clear message.

## Authenticode signing (Phase 6 follow-up)

```sh
signtool sign /fd sha256 \
    /tr http://timestamp.digicert.com /td sha256 \
    /a buffr-<version>-x64.msi
```

Requires an EV or OV code-signing certificate provisioned on the build host.
Without signing, SmartScreen will warn the user on first run; with EV signing
reputation accrues immediately, OV reputation accrues over time. Detailed CI
integration (Azure Key Vault, ephemeral keychain, etc.) lives alongside the
macOS notarization steps in the eventual `.github/workflows/release.yml`.
