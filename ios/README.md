# Leg iOS host

`LegHost` is the native iOS host for leg. It is a standalone Xcode project:
it does not depend on the Rust workspace in the repository root, and no
proprietary `leg` source is compiled or linked into it.

The host embeds one pinned revision of [iSH](https://github.com/ish-app/ish)
(`de5ab2c4b1039551203c1106e75f715cd917544b`, vendored under `vendor/ish`) and
shows a placeholder host screen. This slice does not run a guest: it does not
ingest the `#253` guest bundle, does not open a PTY, and does not implement the
native Photos, Files or Shortcuts capabilities.

## Licensing

Everything in this directory is GPLv3 (see `LICENSE`) because it embeds and
adapts iSH, whose additional iOS terms are in `LICENSE.IOS`. See
`THIRD_PARTY_NOTICES.md` for the upstream revision and the vendored material.
The proprietary `leg` code stays outside this directory; the host runs `leg`
as a separate program inside the guest, never linked into the host binary.

## Prerequisites

- macOS with Xcode 26.1 (iOS 26.1 SDK and simulator runtime). The build host
  used for this slice is `mbp15` (Intel, macOS 15.7.2).
- `xcode-select` may point at the Command Line Tools; every build command sets
  `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`.
- An ELF linker for the i386 vDSO that the vendored runtime embeds: `ld.lld`
  (Homebrew `lld`) or the `rust-lld` that ships with a rustup toolchain. Xcode
  itself only ships `ld64`, which cannot produce an i386 ELF object.

## Declared support

| Setting | Value |
| --- | --- |
| Deployment target | iOS 16.0 (`IPHONEOS_DEPLOYMENT_TARGET`) |
| Device family | iPhone and iPad (`TARGETED_DEVICE_FAMILY = 1,2`) |
| Bundled identifier | `com.shukelabs.leg.ioshost` |
| Signing | Automatic development signing (`CODE_SIGN_STYLE = Automatic`) |
| Architectures | `x86_64` and `arm64` simulator slices, `arm64` device slice |

## Build

From a clean clone:

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcodebuild -project ios/LegHost.xcodeproj -scheme LegHost -sdk iphonesimulator \
    -configuration Debug -derivedDataPath build/dd build
```

The build product is `build/dd/Build/Products/Debug-iphonesimulator/LegHost.app`.

The `Build iSH runtime` script phase (`scripts/build-ish-runtime.sh`) compiles
the vendored runtime for every architecture in `ARCHS` and archives it into
`libish_runtime.a`, which the app links with `-force_load`. Upstream iSH builds
the same runtime with meson, which needs meson, ninja and an LLVM toolchain
that can target i386 Linux; this script drives the stock Xcode clang instead
and only needs an ELF linker for the vDSO.

The same phase writes a link map next to the app
(`build/dd/Build/Products/Debug-iphonesimulator/LegHost.linkmap`).

## Install and launch in the simulator

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcrun simctl boot "iPhone 17 Pro"
xcrun simctl install "iPhone 17 Pro" \
    build/dd/Build/Products/Debug-iphonesimulator/LegHost.app
xcrun simctl launch "iPhone 17 Pro" com.shukelabs.leg.ioshost
```

The app shows the placeholder host screen with the pinned iSH revision. It does
not depend on the standalone iSH app: the runtime it needs is compiled into the
binary.

## Signing a device build

The simulator build needs no signing identity. A device build uses automatic
development signing and needs a team:

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcodebuild -project ios/LegHost.xcodeproj -scheme LegHost -sdk iphoneos \
    -configuration Release -derivedDataPath build/dd \
    DEVELOPMENT_TEAM=<team-id> build
```

The project ships `DEVELOPMENT_TEAM` empty so no team id is committed; pass it
on the command line (or set it in Xcode for the local checkout).

## Checking what is linked

The host must contain the vendored iSH runtime and no proprietary leg code:

```
nm -a build/dd/Build/Products/Release-iphonesimulator/LegHost.app/LegHost \
    | grep -E '_vdso_symbol|_fiber_enter'
nm -a build/dd/Build/Products/Release-iphonesimulator/LegHost.app/LegHost \
    | grep -iE 'leg_tui|leg_ui|leg_web|shukelabs'
grep -iE 'leg_tui|leg_ui|leg-web|src/main\.rs|companions/' \
    build/dd/Build/Products/Release-iphonesimulator/LegHost-x86_64.linkmap
```

The first command lists the vendored runtime symbols; the second and the third
must print nothing.

## Out of scope

Guest bundle ingestion, PTY and terminal integration, Keychain and provider
wiring, lifecycle persistence, physical device runtime testing, the native
Photos, Files and Shortcuts adapters, and App Store metadata are later slices
of `#255`.
