# macOS code signing + notarization (stub)

> **Status: aspirational.** None of the signing or notarization described here
> is implemented — no workflow in `.github/workflows/` signs anything.
> `cargo xtask bundle-macos` skips signing entirely; assembled bundles only run
> after ad-hoc local signing (`codesign --force --deep --sign -`). The one
> section that describes shipped behaviour is
> [Helper-flavor split](#helper-flavor-split-current-layout) and
> [DMG production](#dmg-production).

## Why signing matters

macOS Gatekeeper refuses to run unsigned (or ad-hoc-signed) bundles downloaded
from the internet. To ship `Buffr.app` (or its `.dmg` wrapper) to end users we
need:

1. **Apple Developer ID** — a paid developer account, with a
   `Developer ID Application` certificate provisioned in the keychain of the
   build host (or signing service).
2. **Hardened Runtime** — `codesign --options runtime` on every Mach-O in the
   bundle. CEF requires several entitlements relaxations; see below.
3. **Notarization** — submit the signed `.app` (zipped or in a `.dmg`) to
   Apple's notary service via `notarytool`. Apple staples a ticket back onto the
   artifact.
4. **Stapling** — `xcrun stapler staple Buffr.app` so first-launch works
   offline.

## Bundle signing order

CEF bundles must be signed inside-out:

1. `Contents/Frameworks/Chromium Embedded Framework.framework/Versions/A/Libraries/*.dylib`
2. `Contents/Frameworks/Chromium Embedded Framework.framework`
3. `Contents/Frameworks/Buffr Helper.app` plus the three flavored bundles —
   `Buffr Helper (GPU).app`, `(Renderer).app`, `(Plugin).app` — which the
   bundler already produces (see below)
4. `Contents/MacOS/buffr` (the main bundle binary, signed last with the bundle
   plist)

`codesign --deep` sometimes works but is unreliable for nested helper bundles
with their own plists. The bundle script will eventually grow per-component
signing logic.

## Entitlements

CEF's renderer / GPU / plugin helpers each need slightly different entitlements
files. At minimum:

- `com.apple.security.cs.allow-jit` — V8.
- `com.apple.security.cs.allow-unsigned-executable-memory` — sandboxed
  third-party plugins on older Chromium drops.
- `com.apple.security.cs.disable-library-validation` — load CEF from outside the
  bundle's signed framework root.
- `com.apple.security.cs.disable-executable-page-protection` — only on helpers;
  required for Chromium's V8.

The Chromium upstream `cef/tests/cefclient/resources/mac/*.entitlements` files
are the reference; adapted copies would be vendored when signing lands. None are
in the tree today.

## Helper-flavor split (current layout)

`cargo xtask bundle-macos` ships **four** helper bundles inside
`Buffr.app/Contents/Frameworks/` — Apple's full sandboxing model wants one
helper per subprocess type so per-flavor entitlements can differ:

| Bundle name                   | Bundle id                          | Plist template                          | Subprocess type          |
| ----------------------------- | ---------------------------------- | --------------------------------------- | ------------------------ |
| `Buffr Helper.app`            | `sh.kryptic.buffr.helper`          | `xtask/templates/helper.plist`          | utility / generic worker |
| `Buffr Helper (GPU).app`      | `sh.kryptic.buffr.helper.gpu`      | `xtask/templates/helper-gpu.plist`      | GPU process              |
| `Buffr Helper (Renderer).app` | `sh.kryptic.buffr.helper.renderer` | `xtask/templates/helper-renderer.plist` | renderer process         |
| `Buffr Helper (Plugin).app`   | `sh.kryptic.buffr.helper.plugin`   | `xtask/templates/helper-plugin.plist`   | plugin (PPAPI / WASM)    |

Apple requires every nested `.app`'s Mach-O have a **distinct** file name; each
bundle's `Contents/MacOS/Buffr Helper (Flavor)` is a `fs::copy` of the same
`buffr-helper` binary (notarisation rejects symlinks for executables).

cef-rs 147 only resolves a single `browser_subprocess_path`, so today every
subprocess type is launched out of the unbranded `Buffr Helper.app`. The other
three bundles are still shipped (each a full copy of the helper binary, so the
bundle grows accordingly) so future signing only needs per-flavor entitlements
plus a path-resolver hook — when cef-rs grows `on_browser_process_handler_path`
(or equivalent) we point each subprocess at its branded helper, no bundle layout
migration required.

## DMG production

`cargo xtask package-macos-dmg [--release]` wraps the bundle into
`target/dist/macos/buffr-<version>-<arch>.dmg` (`arm64` on Apple silicon hosts,
`x86_64` on Intel). Implementation:

1. The bundle from `bundle-macos` is copied into
   `target/<profile>/dmg-staging/Buffr.app/`.
2. A relative `Applications -> /Applications` symlink is created next to it as
   the drag-target.
3. `hdiutil create -volname buffr -srcfolder dmg-staging -ov -format UDZO` runs
   on macOS.
4. On Linux dev hosts (no `hdiutil`) the script falls back to `genisoimage` —
   the resulting image mounts on macOS but loses the Finder layout affordances;
   only useful for smoke-testing the staging step. CI on a `macos-latest` runner
   exercises the real `hdiutil` path.
5. If neither tool is on `PATH` the staging tree is left in place and a clear
   warning is printed; nothing fails.

The DMG is **unsigned**. After download, first-run users must clear the
quarantine xattr that Gatekeeper attaches to web-downloaded files:

```sh
xattr -d com.apple.quarantine /Applications/Buffr.app
```

Once Developer-ID signing + notarization land (next section), Gatekeeper will
accept the bundle without manual intervention.

## Notarization tooling

```sh
# zip the bundle
ditto -c -k --keepParent target/release/Buffr.app buffr.zip

# submit
xcrun notarytool submit buffr.zip \
    --apple-id $APPLE_ID --team-id $TEAM_ID --password $APP_SPECIFIC_PWD \
    --wait

# staple
xcrun stapler staple target/release/Buffr.app
```

CI integration (GitHub Actions secrets, ephemeral keychain via
`security create-keychain`, etc.) would go in `.github/workflows/ci.yml`, next
to the existing `macos-package` and `publish-github-release` jobs — there is no
separate `release.yml` in this repo.
