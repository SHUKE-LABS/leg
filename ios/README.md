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

The pinned Alpine root has no `bash`, and the guest program runs its
commands with `bash -lc`, so the provisioner stages a shim at
`/usr/local/bin/bash` in the guest that runs the shell the root does have
(`/bin/busybox sh`). The shim is part of the staging archive, so it is
verified with the other staged files, and a root that was imported before
the shim existed is imported again.

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

The same file lists the two other changes the vendored tree carries. A
readlink of `/proc/<pid>/exe` for a task without an address space took the
emulator down, so the read now fails with `ESRCH` for such a task. A guest
that bounded its socket reads had its `setsockopt` refused with `EINVAL`,
because the emulator knew the timeout options by other numbers than a Linux
guest uses and because the timeout value is wider on the host than in the
guest; the two options now have the numbers a Linux guest uses and the
timeout is translated in both directions.

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
import takes a moment on a cold container. Every test of this class also
launches the app with `-leghost-provider anthropic`, because the host reads
its provider configuration before it touches the guest root and stops
without one.

The `LegHostFixtureUITests` class drives the app against the loopback fixture
instead. It launches the app with the fixture arguments, so it reuses the
verified guest root and does not import it again.

## Provider configuration

The host owns the provider configuration and reads it before it touches the
guest root: a missing or an unsupported configuration is reported before
anything is provisioned, booted or started.

The configuration is one Keychain generic password. The host reads the item
and never writes it, so the credential stays where the user put it:

| Field | Value |
| --- | --- |
| Service | `com.shukelabs.leg.ioshost.provider` |
| Account | `default` |
| Accessibility | `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` |

Its data is UTF-8 JSON with exactly these five keys:

```json
{"provider":"anthropic","api_key":"sk-ant-...","base_url":"https://api.anthropic.com","model":"claude-sonnet-4-5","context_window_tokens":200000}
```

`context_window_tokens` has to be a positive number. The host reports two
stable diagnostics:

| Diagnostic | Meaning |
| --- | --- |
| `IOS_PROVIDER_CONFIG_MISSING` | There is no item, or the item does not hold exactly the five keys with usable values. |
| `IOS_PROVIDER_CONFIG_UNSUPPORTED` | The item names `openai-chat-completions` or `openai-responses`. This host speaks the Anthropic Messages protocol. |

The guest gets the configuration as an allowlist: `TERM`, `HOME`, `PATH`,
`SHELL`, `LEG_PROVIDER`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`,
`LEG_MODEL` and `LEG_CONTEXT_WINDOW_TOKENS`. Nothing else of the host
environment reaches the guest, and no `LEGHOST_*` variable is ever passed on.

## The loopback fixture

The fixture serves the Anthropic Messages endpoint on `127.0.0.1`, so one
guest turn runs without a provider. It is a DEBUG-only fixture: it is
reachable through the launch arguments below, it keeps its configuration in
memory and it never becomes a place where a credential is stored.

### Launch arguments

| Argument | Meaning |
| --- | --- |
| `-leghost-fixture success` | The fixture answers the first request with a text and a `bash` tool call, and the second one with the final text. |
| `-leghost-fixture provider-error` | The fixture answers the first request with HTTP 500. |
| `-leghost-fixture unreachable` | The fixture closes its port before the turn, so the guest cannot reach it. |
| `-leghost-provider anthropic` | The run is configured for the Anthropic Messages protocol. |
| `-leghost-provider openai-chat-completions` | The run is configured for a provider this host refuses. |
| `-leghost-provider openai-responses` | The run is configured for a provider this host refuses. |
| `-leghost-no-provider` | The run has no provider configuration at all. |
| `-leghost-runtime-failure` | The guest runtime is asked for a program that is not there. |

The provider arguments configure the run in memory: the credential is
`IOS_FIXTURE_CREDENTIAL_SENTINEL`, the model is `claude-ios-fixture` and the
context window is `200000`. Nothing is written to the Keychain, to the user
defaults or to the file system.

A run without the fixture talks to a closed port on the loopback interface,
so it stays on the device: a turn of such a run fails with a transport error
instead of reaching a provider. A DEBUG run without a Keychain item needs one
of the provider arguments, or the host stops at the configuration boundary
with `IOS_PROVIDER_CONFIG_MISSING` before it touches the guest root.

A fixture run pins the turn down with these values, which the guest gets in
addition to the ones above:

```
LEG_MAX_TOKENS=256
LEG_MAX_TOOL_ROUNDS=1
LEG_MAX_RETRIES=0
LEG_RETRY_BASE_DELAY_MS=0
LEG_TIMEOUT_SECS=5
LEG_STREAM_IDLE_TIMEOUT_SECS=5
LEG_BASH_TIMEOUT_SECS=5
LEG_EVENT_LOG=/tmp/leg-ios-fixture/exchange.jsonl
NO_COLOR=1
```

The zero retry budget makes the number of provider requests the same on
every run.

### The turn

The guest program asks for a workspace, shows the first-run warning and then
offers the composer. The simulator test

1. types `/tmp` and presses Enter to select the workspace,
2. presses Enter to acknowledge the first-run warning, when it is shown,
3. types `IOS_FIXTURE_TURN`,
4. presses the send control (`ctrl-s`) the host offers above the terminal.

The send control is on the screen because the software keyboard cannot send
the key the guest TUI submits the composer with.

The host names the two programs of the pinned bundle to the guest program
(`--leg-bin` and `--supervisor-bin`). The guest program would otherwise find
them next to its own executable, which it looks up through `/proc/self/exe`;
the emulator cannot answer that read for the process it runs, so the host
spells the paths out instead.

A fixture run starts from the same state every time. The host removes the
state the guest program keeps between runs: the catalog of the guest program,
so the run asks for a workspace again and shows the first-run warning again,
and the exchange trail of the last run, so the trail holds this run only. It
creates the directory of the trail, because the guest appends to the trail
file and does not make its directory.

### The failure of a turn

A turn that fails is reported by the guest, and the host writes what the
guest reported into the terminal. The guest writes every turn to the
exchange trail, and a failed turn writes a `response_error` record with the
message the guest produced. The host reads that record while the run goes
on and writes the message into the terminal, where the guest speaks:

| Run | What the guest reported |
| --- | --- |
| Provider error | `provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR` |
| Unreachable | `transport error: ...` |

The host does not write the message itself: it writes the message the guest
recorded, so the class of the failure is the one the guest reported. The
terminal transcript of the run holds the message as well, because the
transcript records what the terminal showed. The host reads the trail of a
fixture run only.

The guest program of the pinned bundle does not render the failure of a turn
in the iOS host: the turn never ends there, so the guest program keeps
showing the turn as running. The guest does report the failure, in the
trail, and the host writes that report into the terminal.

The diagnostics the host produces itself are in the status of the host, under
the `guest-status-detail` identifier: `IOS_PROVIDER_CONFIG_MISSING`,
`IOS_PROVIDER_CONFIG_UNSUPPORTED` and `IOS_GUEST_RUNTIME_ERROR`. The runtime
diagnostic is the host's own finding: the guest program it was asked to run
is not there.

### What the fixture observes

The host shows the counts of the run and the fixture observation under the
`guest-fixture-state` identifier, and the tests read them from there:

| Run | State |
| --- | --- |
| Success | `provision=1,boot=1,session=1,requests=2,statuses=200,200,validation=ok` |
| Provider error | one request with the status 500 |
| Unreachable | no accepted request |
| Missing configuration | no provisioning, no boot, no session, no listener and no request |
| Unsupported provider | the same as the missing configuration |

The fixture validates every request in memory: the method, the path, the
`stream` flag, the credential and the order of the requests. The second
request has to carry the tool result of the `bash` call, and a third request
is refused. The observation holds counts, statuses and the validation
result only; a request header, a request body and the credential are never
written down.

### The artifacts

A fixture run writes four artifacts, all inside the app container:

| Artifact | Path |
| --- | --- |
| Terminal transcript | `Library/Application Support/LegHost/test-artifacts/terminal-transcript.log` |
| Application log | the `LegHost` entries of the system log |
| Guest file system | `Library/Application Support/LegHost/roots/root.active/data` |
| Exchange trail | `tmp/leg-ios-fixture/exchange.jsonl` in the guest file system |

### The verification command

```
ios/tests/run-fixture-verification.sh
```

The script runs the success scenario in the booted simulator, scans the four
artifacts for the configured credential and then runs the provider error
scenario, because the failure of a turn is reported by the guest in its
exchange trail and the host writes that report into the terminal: the
transcript of that run has to hold the marker of the failure. The script
fails when the credential shows up in any of the four artifacts, when the
turn did not reach the fixture and when the transcript does not hold the
marker. Pass `--udid <simulator udid>` to pick the simulator, and
`--skip-run` to scan a run that already happened.

### The fixture tests

```
ios/tests/run-fixture-tests.sh
```

The script runs every fixture test with a simulator of its own. The
accessibility server of a simulator wedges when a long run of the tests
drives the app again and again, and the test that follows the wedge fails
with an accessibility timeout instead of an assertion, so every test starts
from a simulator that was booted for it. Pass the names of the tests to run
only some of them.


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

The provider configuration of the host has its own fixtures. They hand in
the read of the item of the Keychain, so they need neither a Keychain nor a
simulator, and they cover the read, the JSON of the item and the two
diagnostics:

```
ios/tests/run-provider-tests.sh
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

The settings UI, lifecycle and session persistence, physical device runtime
testing, the native Photos, Files and Shortcuts adapters, and App Store
metadata are later slices of `#255`. The provider support of this slice is
the Anthropic Messages protocol; the OpenAI wire variants are covered only
as the rejection cases of the fixture.
