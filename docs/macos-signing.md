# macOS code signing + notarization (stub)

> **Status:** Phase 6 work. This document is a placeholder describing what real
> macOS distribution will need. The current `cargo xtask bundle-macos` skips
> signing entirely; assembled bundles only run after ad-hoc local signing
> (`codesign --force --deep --sign -`).

## Why signing matters

macOS Gatekeeper refuses to run unsigned (or ad-hoc-signed) bundles downloaded
from the internet. To ship `buffr.app` (or its `.dmg` wrapper) to end users we
need:

1. **Apple Developer ID** — a paid developer account, with a
   `Developer ID Application` certificate provisioned in the keychain of the
   build host (or signing service).
2. **Hardened Runtime** — `codesign --options runtime` on every Mach-O in the
   bundle. CEF requires several entitlements relaxations; see below.
3. **Notarization** — submit the signed `.app` (zipped or in a `.dmg`) to
   Apple's notary service via `notarytool`. Apple staples a ticket back onto the
   artifact.
4. **Stapling** — `xcrun stapler staple buffr.app` so first-launch works
   offline.

## Bundle signing order

CEF bundles must be signed inside-out:

1. `Contents/Frameworks/Chromium Embedded Framework.framework/Versions/A/Libraries/*.dylib`
2. `Contents/Frameworks/Chromium Embedded Framework.framework`
3. `Contents/Frameworks/buffr Helper.app` (and any
   `Helper (GPU/Renderer/Plugin).app` once the multi-helper split lands)
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
are the reference; we'll vendor adapted copies once Phase 6 lands.

## Helper-flavor split (deferred)

The single `buffr Helper.app` shipped today maps to all subprocess types.
Apple's full sandboxing model wants four distinct helpers:

| Bundle id suffix   | Subprocess type          | Entitlements file (TODO)        |
| ------------------ | ------------------------ | ------------------------------- |
| `.helper`          | utility / generic worker | `helper.plist` (current single) |
| `.helper.gpu`      | GPU process              | `helper-gpu.plist` (TODO)       |
| `.helper.renderer` | renderer process         | `helper-renderer.plist` (TODO)  |
| `.helper.plugin`   | plugin (PPAPI / WASM)    | `helper-plugin.plist` (TODO)    |

When this split lands we'll need:

- Three additional `xtask/templates/*.plist` files.
- A `buffr-helper` per flavor (separate Cargo bins, or one bin with a symlink
  tree under `Contents/Frameworks/`).
- Per-helper entitlements + signing.
- Updated `buffr-core::App::on_browser_process_handler_path` (or the equivalent
  path-resolver hook in cef-rs 147) so each subprocess type is spawned out of
  the right `.app`.

## Notarization tooling

```sh
# zip the bundle
ditto -c -k --keepParent target/release/buffr.app buffr.zip

# submit
xcrun notarytool submit buffr.zip \
    --apple-id $APPLE_ID --team-id $TEAM_ID --password $APP_SPECIFIC_PWD \
    --wait

# staple
xcrun stapler staple target/release/buffr.app
```

CI integration (GitHub Actions secrets, ephemeral keychain via
`security create-keychain`, etc.) will live in `.github/workflows/release.yml`
once we cut the first signed nightly.
