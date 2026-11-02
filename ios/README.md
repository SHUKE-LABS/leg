# Leg iOS host

`LegHost` is the native iOS host for leg. It is a standalone Xcode project:
it does not depend on the Rust workspace in the repository root, and no
proprietary `leg` source is compiled or linked into it.

The host embeds one pinned revision of [iSH](https://github.com/ish-app/ish)
(`de5ab2c4b1039551203c1106e75f715cd917544b`, vendored under `vendor/ish`), the
pinned guest bundle and the pinned Alpine root. On launch it

1. verifies both pinned archives against their SHA-256 digests,
2. checks the guest bundle manifest, its file set, the ELF32 i386 headers of
   the four guest programs and the inner `SHA256SUMS`,
3. merges the verified Alpine root and the four guest programs into one
   staging archive and imports it into a fresh fakefs root with the vendored
   iSH importer,
4. verifies the imported root and publishes it as the persistent active root,
5. boots the vendored iSH runtime on that root, opens a pseudo terminal and
   runs `/usr/local/bin/leg-tui` on it.

The terminal is [hterm](https://github.com/ish-app/libapps) in a `WKWebView`:
the guest output is written into it, the keyboard (software or hardware) is
forwarded to the guest terminal, and the terminal size is reported to the
guest with `TIOCSWINSZ`. A row of controls above the terminal sends the keys
the software keyboard has no way to send: escape, tab, the arrow keys and the
interrupt, so the guest program can be driven without a hardware keyboard. The
font of the terminal is scaled until the grid holds the 80 columns and 24 rows
the guest program needs, and the guest keeps that size when the software
keyboard covers the screen. A second launch verifies the guest bundle marker
and the four fixed program paths and reuses the root instead of importing it
again.

## The vendored emulator

The pinned guest bundle is built for `i686-unknown-linux-musl`, and rustc
enables SSE2 for that target, so the guest programs contain SSE2
instructions. The vendored iSH emulator did not implement `movmskps`, so the
guest died with `SIGILL` before it drew anything. The instruction is
implemented in the vendored tree, and the change is listed in
[`vendor/ish/LOCAL_CHANGES.md`](vendor/ish/LOCAL_CHANGES.md). It is the only
instruction the pinned guest bundle needs that the vendored emulator was
missing.

## Licensing

Everything in this directory is GPLv3 (see `LICENSE`) because it embeds and
adapts iSH, whose additional iOS terms are in `LICENSE.IOS`. See
`THIRD_PARTY_NOTICES.md` for the upstream revision and the vendored material.
The proprietary `leg` code stays outside this directory; the host runs `leg`
as a separate program inside the guest, never linked into the host binary.

## Pinned inputs

The host never follows a moving release at runtime: the two archives are
pinned by URL and SHA-256 in `LegHost/PinnedInputs.plist`, and the build
downloads exactly those files.

| Input | URL | SHA-256 |
| --- | --- | --- |
| leg guest bundle `v0.43.1` | `https://github.com/SHUKE-LABS/leg/releases/download/v0.43.1/leg-0.43.1-i686-unknown-linux-musl.tar.gz` | `c7ba65aa24e127039d0292ae1db8b8573fbc07565e5ea40805630dd58305277d` |
| Alpine i386 root `3.20.3` | `https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86/alpine-minirootfs-3.20.3-x86.tar.gz` | `4c7ba1c9e6642692dd5cb302e5e58943221901f2e15991af7d5cb4ded80dc7b9` |

The guest bundle holds exactly six files: `leg`, `leg-tui`, `leg-web`,
`leg-ui-supervisor`, `ish-compatibility-v1.json` and `SHA256SUMS`. Its
manifest has to declare the target `i686-unknown-linux-musl`, the
architecture `i386`, the operating system `linux`, the C library `musl`, the
Alpine release `3.20.3`, the platform `linux/386`, and the size and SHA-256 of
each of the four programs.

## Prerequisites

- macOS with Xcode 26.1 (iOS 26.1 SDK and simulator runtime). The build host
  used for this slice is `mbp15` (Intel, macOS 15.7.2).
- `xcode-select` may point at the Command Line Tools; every build command sets
  `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`.
- An ELF linker for the i386 vDSO that the vendored runtime embeds: `ld.lld`
  (Homebrew `lld`) or the `rust-lld` that ships with a rustup toolchain. Xcode
  itself only ships `ld64`, which cannot produce an i386 ELF object.
- Network access for the `Fetch pinned inputs` build phase, unless the
  archives are staged already (see below).

## Declared support

| Setting | Value |
| --- | --- |
| Deployment target | iOS 16.0 (`IPHONEOS_DEPLOYMENT_TARGET`) |
| Device family | iPhone and iPad (`TARGETED_DEVICE_FAMILY = 1,2`) |
| Bundled identifier | `com.shukelabs.leg.ioshost` |
| Signing | Automatic development signing (`CODE_SIGN_STYLE = Automatic`) |
| Architectures | `x86_64` and `arm64` simulator slices, `arm64` device slice (no device is attached yet, see `#265`) |

## Build

From a clean clone:

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcodebuild -project ios/LegHost.xcodeproj -scheme LegHost -sdk iphonesimulator \
    -configuration Release -derivedDataPath build/dd build
```

The build product is `build/dd/Build/Products/Release-iphonesimulator/LegHost.app`.
The derived data lands in `build/`, which the repository ignores.

Two script phases run before the sources are compiled:

- `scripts/build-ish-runtime.sh` compiles the vendored runtime for every
  architecture in `ARCHS` and archives it into `libish_runtime.a`, which the
  app links with `-force_load`. Upstream iSH builds the same runtime with
  meson, which needs meson, ninja and an LLVM toolchain that can target i386
  Linux; this script drives the stock Xcode clang instead and only needs an
  ELF linker for the vDSO. The same script builds the vendored libarchive into
  `libarchive.a`, which the guest root importer needs.
- `scripts/fetch-pinned-inputs.sh` downloads the two pinned archives,
  verifies their digests and stages them in `ios/resources/PinnedInputs/`,
  which is copied into the app bundle. A digest mismatch fails the build. The
  download cache is `ios/build/pinned-inputs`; set
  `LEGHOST_PINNED_INPUTS_DIR` to a directory that already holds the two
  archives to build without network access.

Xcode links one binary per architecture, so the link map is written per
architecture next to the app:
`build/dd/Build/Products/Release-iphonesimulator/LegHost-<arch>.linkmap`,
for example `LegHost-x86_64.linkmap`.

The commands below use the Release configuration. A Debug build puts its
products in `build/dd/Build/Products/Debug-iphonesimulator/` instead.

## Install and launch in the simulator

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcrun simctl boot "iPhone 17 Pro"
xcrun simctl install "iPhone 17 Pro" \
    build/dd/Build/Products/Release-iphonesimulator/LegHost.app
xcrun simctl launch "iPhone 17 Pro" com.shukelabs.leg.ioshost
```

The first launch verifies the pinned inputs, imports the guest root and
starts `leg-tui` in the terminal. It does not depend on the standalone iSH
app: the runtime it needs is compiled into the binary.

The provisioned root lives in the app container, under
`Library/Application Support/LegHost/roots`. To start over, remove the app
from the simulator and install it again:

```
xcrun simctl uninstall "iPhone 17 Pro" com.shukelabs.leg.ioshost
```

That removes the active root, so the next launch imports it again. The same
can be done without removing the app: the `-leghost-reset` launch argument
makes the host remove the provisioned root before it imports it again.

```
xcrun simctl launch "iPhone 17 Pro" com.shukelabs.leg.ioshost -leghost-reset
```

The host runs `/usr/local/bin/leg-tui` in the guest. The simulator tests need
a guest program they can make spawn a helper process, so they launch the host
with another program:

```
xcrun simctl launch "iPhone 17 Pro" com.shukelabs.leg.ioshost \
    -leghost-program /bin/sh
```

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

## Simulator UI tests

The `LegHostUITests` target drives the app in a simulator. It covers the
first launch (the guest root is imported and the guest program starts), the
guest program drawing its screen, the keyboard reaching the guest, the
rotation changing the size the guest was told about, the on screen interrupt
ending the guest program without a hardware keyboard, the exit of a helper
process leaving the session running, and the second launch reusing the
verified root instead of importing it again.

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcodebuild test -project ios/LegHost.xcodeproj -scheme LegHost -sdk iphonesimulator \
    -destination "platform=iOS Simulator,id=<simulator-udid>" \
    -derivedDataPath build/dd
```

The tests need a booted simulator. Every test launches the app with the
`-leghost-reset` argument, so the host imports the guest root again; the
import takes a moment on a cold container.

## Host fixtures

The provisioner is covered by host side fixtures that need neither the
network nor a simulator: they build their own pinned inputs and exercise the
valid path, a one byte corruption of each input, a bundle for the wrong
target, a program that is not an ELF32 i386 binary, a failed import, stale
staging state, a corrupted active root and the verified reuse of an existing
root.

```
ios/tests/run-host-tests.sh
```

The same provisioner runs against the real pinned archives:

```
ios/tests/run-pinned-inputs.sh
```

## Checking what is linked

The host must contain the vendored iSH runtime and no proprietary leg code.
Run these from the repository root after the Release build above:

```
app=build/dd/Build/Products/Release-iphonesimulator/LegHost.app

nm -a "$app/LegHost" | grep -E '_fiber_enter|_emu_'
grep -c 'libish_runtime.a(' \
    build/dd/Build/Products/Release-iphonesimulator/LegHost-x86_64.linkmap
nm -a "$app/LegHost" | grep -iE 'leg_tui|leg_ui|leg_web|shukelabs'
grep -iE 'leg_tui|leg_ui|src/main\.rs|companions/' \
    build/dd/Build/Products/Release-iphonesimulator/LegHost-*.linkmap
```

The first two commands show the vendored runtime: its symbols in the host
binary and its objects in the link map. The last two must print nothing,
because no proprietary leg crate or source is linked into the host. The host
names the four guest programs by file name, so the strings `leg-tui` and
`leg-web` are in the binary; they are the names of the programs the guest
root must hold, not code of them.

## Out of scope

Keychain and provider wiring, lifecycle and session persistence, physical
device runtime testing, the native Photos, Files and Shortcuts adapters, and
App Store metadata are later slices of `#255`.
