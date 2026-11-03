# Leg iOS host

`LegHost` is the native iOS host for leg. It is a standalone Xcode project:
it does not depend on the Rust workspace in the repository root, and no
proprietary `leg` source is compiled or linked into it.

The host embeds one pinned revision of [iSH](https://github.com/ish-app/ish)
(`de5ab2c4b1039551203c1106e75f715cd917544b`, vendored under `vendor/ish`), the
pinned guest bundle and the pinned Alpine root. On launch it

1. reads the provider configuration and checks the device: the pinned
   runtime files, the architecture of the host and the free space of the
   app container,
2. verifies both pinned archives against their SHA-256 digests,
3. checks the guest bundle manifest, its file set, the ELF32 i386 headers of
   the four guest programs and the inner `SHA256SUMS`,
4. merges the verified Alpine root and the four guest programs into one
   staging archive and imports it into a fresh fakefs root with the vendored
   iSH importer,
5. verifies the imported root and publishes it as the persistent active root,
6. boots the vendored iSH runtime on that root, opens a pseudo terminal and
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

## The state of a session

The guest workspace and the session trail live in the app container, so they
survive a relaunch. The host never claims that it runs in the background:
iOS suspends the app whenever it wants to, and the host stops the session at
a boundary it controls instead.

| What | Where | What clears it |
| --- | --- | --- |
| The guest root | `Library/Application Support/LegHost/roots/root.active` | `-leghost-reset`, or removing the app |
| The workspace catalog of the guest | `<root>/data/root/.local/state/leg-ui` | `-leghost-fixture-reset` |
| The exchange trail of the guest | `<root>/data/tmp/leg-ios-fixture/exchange.jsonl` | `-leghost-fixture-reset` |
| The durable request ledger | `Library/Application Support/LegHost/test-artifacts/request-ledger.jsonl` | `-leghost-fixture-reset` |
| The trail of the stopped session | `Library/Application Support/LegHost/test-artifacts/exchange-trail.jsonl` | the next session that is stopped overwrites it |

An ordinary relaunch keeps all of them: the guest program finds the
workspace it was given, the trail of the last run is still there, and the
durable ledger still holds the requests of the run before it. The host never
sends the turn that was running again by itself.

A retry after an interruption keeps them as well. The reset control belongs
to the launch: the first chain of a launch uses it, and every chain after it
— a retry — keeps the catalog, the trail and the ledger of the run that was
stopped, so the evidence of a stopped turn is not thrown away.

### The lifecycle

The lifecycle of a session is one state machine with one gate. The chain of
operations that prepares, provisions, boots and starts the session runs to
its end or is cancelled, and no second chain starts while it runs. A retry
is only accepted from a failed, a stopped or an interrupted state.

When the app goes to the background, the host stops the session at this
boundary:

1. the chain is cancelled,
2. the trail of the guest is written out to the test artifacts,
3. the output and the exit callbacks of the runtime are detached,
4. the thread of the output is joined, and the wait for it is checked,
5. the guest of the session is killed: every task of the guest is killed, and
   the terminal of the session is hung up, so a guest that reads the terminal
   wakes up and is killed before it runs another instruction of the guest —
   it cannot write to the trail of the guest anymore and it cannot run next
   to the session that comes next,
6. the fixture and the watcher of the trail are stopped — the connections of
   the fixture end the waits of the guest that wait for the answer of the
   provider — and the host waits for the guest to end,
7. the output of the session that stopped is cleared and the runtime releases
   the terminal of the guest — each of them exactly once,
8. the host shows `IOS_SESSION_INTERRUPTED` with the recovery guidance.

A task of the guest that waits for data sleeps in a read of the host, and the
kill of the guest does not end that wait on its own: the kill is pending until
the task runs the next instruction of the guest, and the task runs that
instruction only when its wait ends. The stop of the session ends the waits of
the guest with the things the host owns: the terminal of the session is hung
up, so a guest that reads the terminal wakes up, and the connections of the
fixture are closed, so a guest that waits for the provider wakes up.

The kernel ends the waits that are left: it walks the tables of files of the
tasks, and it ends the wait of every file that waits for data — the socket of
the guest is shut down, and a byte is written into the pipe of the guest, so
a task that reads one of them wakes up. The wake is given again in every round
of the wait, because a task that was running when the wake arrived waits
again right away, and the kill of a task is delivered only once. The kill is
pending while the task wakes up, and the kernel delivers the kill before the
guest runs another instruction, so the guest does not write to the trail of the
guest anymore.

The host does not read the files of a task of the guest. The files of a task
belong to the task, and the task frees them when it exits, so a host that
reads them while the task exits reads memory that is gone. The walk of the
files happens in the kernel, which keeps the table of files of a task alive
while it walks it, and which leaves a task that is on its way out alone.

The stop of a session and the retry after it can happen many times in a row.
The fixture test `testTheStoppedGuestCannotWriteAfterManyRetries` drives
three turns, stops every one of them and retries, with the allocator of the
host scribbling the memory it frees, so a use of memory that is gone shows
itself. Every cycle holds the same promise: the guest of the stopped session
does not run any more, and it does not write to the trail of the guest.

The waits of the boundary are checked. When the thread of the output did not
stop, or when the guest of the session did not stop, the host shows
`IOS_SESSION_NOT_STOPPED` and the session is not released: a guest that is
still alive would write to the trail of the guest and would run next to the
session that comes next, so no other session starts. The fixture is stopped
in that case as well, so the guest that is still alive does no provider work
anymore.

A callback of a session that was stopped belongs to a session that is gone
and is ignored. The workers of a session belong to the chain they were made
for as well: the thread of the output is joined before the next session
starts, and the watcher of the trail and the callback of the exit are
ignored when the chain that runs now is another one. The text and the
records of a stopped session never reach the session that runs now. When the
app comes back to the foreground, the interrupted state stays where it is:
the user starts a new session with the retry control, and the prompt of the
turn that was running is not sent again.

## The vendored emulator

The pinned guest bundle is built for `i686-unknown-linux-musl`, and rustc
enables SSE2 for that target, so the guest programs contain SSE2
instructions. The vendored iSH emulator did not implement `movmskps`, so the
guest died with `SIGILL` before it drew anything. The instruction is
implemented in the vendored tree, and the change is listed in
[`vendor/ish/LOCAL_CHANGES.md`](vendor/ish/LOCAL_CHANGES.md). It is the only
instruction the pinned guest bundle needs that the vendored emulator was
missing.

The same file lists the other changes the vendored tree carries. A
readlink of `/proc/<pid>/exe` for a task without an address space took the
emulator down, so the read now fails with `ESRCH` for such a task. A guest
that bounded its socket reads had its `setsockopt` refused with `EINVAL`,
because the emulator knew the timeout options by other numbers than a Linux
guest uses and because the timeout value is wider on the host than in the
guest; the two options now have the numbers a Linux guest uses and the
timeout is translated in both directions. The kernel ends the waits of the
tasks that wait for data, and it keeps the files of a task alive while it
walks them, so the stop of a guest does not race with the exit of a task.

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
verified guest root and does not import it again. Every fixture test starts
with `-leghost-fixture-reset`, so it starts from a known state.

The fixture class covers the success, the provider error, the unreachable
and the runtime failure scenarios, the missing and the unsupported provider
configuration, the relaunch that keeps the workspace, the trail and the
requests, the background that interrupts a turn, and the faults of the
device that are reported before the guest is touched.

The `LegHostTests` target holds the tests of the lifecycle and of the
preflight. They drive the state machine of the session and the checks of
the device with a host and a device they make up, so they need no guest:

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
ios/tests/run-lifecycle-tests.sh
```

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

## The checks before the guest root

The host reads the provider configuration first and checks the device next.
Both happen before the guest root is touched, so a run that fails one of
them never provisions, boots, starts a session or starts the fixture: the
counts of the run stay at zero.

The checks run in a fixed order, and the first one that fails is the
diagnostic the host shows:

| Order | Diagnostic | What it means | What to do |
| --- | --- | --- | --- |
| 1 | `IOS_PROVIDER_CONFIG_MISSING` | There is no item, or the item does not hold exactly the five keys with usable values. | Add the Keychain item. |
| 2 | `IOS_PROVIDER_CONFIG_UNSUPPORTED` | The item names `openai-chat-completions` or `openai-responses`. | Replace the item with an Anthropic one. |
| 3 | `IOS_RUNTIME_FILES_MISSING` | The pinned runtime files are not in the app bundle. | Remove the app from the device and install it again. |
| 4 | `IOS_RUNTIME_FILES_CORRUPT` | One of the pinned runtime files does not match its pinned digest. | Remove the app from the device and install it again. |
| 5 | `IOS_ARCHITECTURE_UNSUPPORTED` | The host runs on an architecture the pinned guest runtime does not support. | Run the host on an `x86_64` or an `arm64` host. |
| 6 | `IOS_STORAGE_INSUFFICIENT` | The app container holds less than 512 MB of free space. | Free space on the device and start the host again. |

Every diagnostic names itself and says what to do about it, and the status of
the host holds it under the `guest-status-detail` identifier.

The three checks of the device read the filesystem, the free space of the
container and the architecture of the host. They are injected, so the unit
tests hand the preflight the state of a device they make up.

The architecture of the host is the one the app was built for: the pinned
guest bundle runs on the emulator the app carries, and that emulator is built
for the architecture of the build. The name a device reports is a model
(`iPhone15,2`) and not an architecture, so the model is not what the check
looks at.

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
| `-leghost-fixture delayed` | The fixture holds its answer back, so a turn stays running long enough for the host to be interrupted in the middle of it. |
| `-leghost-provider anthropic` | The run is configured for the Anthropic Messages protocol. |
| `-leghost-provider openai-chat-completions` | The run is configured for a provider this host refuses. |
| `-leghost-provider openai-responses` | The run is configured for a provider this host refuses. |
| `-leghost-no-provider` | The run has no provider configuration at all. |
| `-leghost-runtime-failure` | The guest runtime is asked for a program that is not there. |
| `-leghost-fixture-reset` | The run starts from a known state: the catalog of the guest, the exchange trail and the durable ledger of the requests are cleared. The control belongs to the launch, so a retry after an interruption keeps the state of the run that was stopped. |
| `-leghost-interrupt-after-request` | The run stops the session as soon as the fixture accepts the request of the turn. The path is the one of the background, so the session is stopped at the boundary the host controls and the trail of the stopped turn is written out, without the app having to go to the background. |
| `-leghost-runtime-missing` | The run reports the pinned runtime files as missing. |
| `-leghost-runtime-corrupt` | The run reports the pinned runtime files as corrupt. |
| `-leghost-unsupported-architecture` | The run reports an architecture the guest cannot run on. |
| `-leghost-low-storage` | The run reports the container as too small for the import. |

The fault controls replace the state of the device for one run, in memory
and for that run only. They are the way the simulator tests induce the
faults of the preflight without breaking the device.

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
every run. The delayed scenario raises the two timeouts of the turn to 120
seconds, so the turn is still running when the host stops the session.

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

A fixture run starts from a known state only when it is told to. The
`-leghost-fixture-reset` control removes the state the guest program keeps
between runs: the catalog of the guest program, so the run asks for a
workspace again and shows the first-run warning again, the exchange trail of
the last run, and the durable ledger of the requests, so the count of the
requests of the run is the count of that run. Without the control the run
keeps all of them: the guest program finds the workspace it was given, the
trail of the last run is still there, and the count of the requests is the
count of every run that came before. The control belongs to the launch: it
is used by the first chain of the process, so a retry after an interruption
keeps the state of the run that was stopped. The host creates the directory
of the trail either way, because the guest appends to the trail file and
does not make its directory.

The terminal is the same terminal in every phase of the host. The phases
hold it in views of a different shape, and a terminal without an identity
of its own would be a new terminal in every one of them: the web view
would be built again and the screen of the guest would be lost when the
session is interrupted. The identity keeps the one web view of the terminal
across the phases, so the screen of the guest stays on the screen when the
session is interrupted and when it is started again.

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
`IOS_PROVIDER_CONFIG_UNSUPPORTED`, `IOS_GUEST_RUNTIME_ERROR` and
`IOS_SESSION_NOT_STOPPED`. The runtime diagnostic is the host's own finding:
the guest program it was asked to run is not there. The guest program never
ran in that case, so the host writes the diagnostic into the terminal as
well, and the terminal stays on the screen when the guest could not start,
because it is the only place the run can show what went wrong:

| Run | What the terminal holds |
| --- | --- |
| Provider error | `provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR` |
| Unreachable | `transport error: ...` |
| Runtime failure | `IOS_GUEST_RUNTIME_ERROR: the guest program could not be started ...` |

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

The state holds `tasks` as well: the number of tasks of the guest that run,
read when the session starts and when it stops. A session that was stopped
holds `tasks=0`, because the guest of the session was stopped and waited
for; a session that runs holds the tasks of its guest.

The fixture validates every request in memory: the method, the path, the
`stream` flag, the credential and the order of the requests. The second
request has to carry the tool result of the `bash` call, and a third request
is refused. The observation holds counts, statuses and the validation
result only; a request header, a request body and the credential are never
written down.

### The artifacts

A fixture run writes these artifacts, all inside the app container:

| Artifact | Path |
| --- | --- |
| Terminal transcript | `Library/Application Support/LegHost/test-artifacts/terminal-transcript.log` |
| Request ledger | `Library/Application Support/LegHost/test-artifacts/request-ledger.jsonl` |
| Trail of the stopped session | `Library/Application Support/LegHost/test-artifacts/exchange-trail.jsonl` |
| Application log | the `LegHost` entries of the system log |
| Guest file system | `Library/Application Support/LegHost/roots/root.active/data` |
| Exchange trail | `tmp/leg-ios-fixture/exchange.jsonl` in the guest file system |

The request ledger holds one JSON object per accepted request, and nothing
else: the turn the request belongs to, the ordinal of the request in that
turn and the status it was answered with. No body, no header and no
credential ever reaches it. The ledger is cumulative: a second run of the
host reads the requests of the first one, so a run that starts again cannot
hide a duplicate behind a fresh counter. The number of requests the host
shows under `guest-fixture-state` is the number of the ledger.

### The verification command

```
ios/tests/run-fixture-verification.sh
```

The script runs six scenarios in the booted simulator: the success
scenario, the provider error scenario, the unreachable scenario, the runtime
failure scenario, the relaunch run and the background run. Every scenario
gets a simulator of its own, and the artifacts of every run are scanned for
the configured credential. The script fails when the credential shows up in
any of the artifacts, when the turn did not reach the fixture and when the
transcript of a run does not hold the marker of that run.

| Scenario | The transcript of the run holds |
| --- | --- |
| Success | `IOS_FIXTURE_STREAM_TEXT`, `IOS_FIXTURE_TOOL_RESULT`, `IOS_FIXTURE_FINAL_RESPONSE` |
| Provider error | `provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR` |
| Unreachable | `transport error:` |
| Runtime failure | `IOS_GUEST_RUNTIME_ERROR` |

The relaunch run starts the app twice, and the script checks the durable
ledger of the requests across the two processes: it holds the two requests
of the first turn and nothing else, so the turn was not sent again. The
background run stops the session in the middle of the turn, and the script
checks the trail the host wrote out when it stopped it. The stopped guest
run stops the session and starts the host again, and the script checks that
the trail of the guest holds the records of the stopped turn only: the guest
of the stopped session was stopped and waited for, so it did not write to
the trail of the guest after the interruption.

The exchange trail of a run is only there when the guest program ran, so the
runtime failure run has no trail to scan.

Pass `--udid <simulator udid>` to pick the simulator, and `--skip-run` to
scan the run that is already in the simulator: the script runs no scenario
then, and it scans the four artifacts of the last run.

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
the call into the Security framework, so they need neither a Keychain nor a
simulator, and they cover the query of that call, the JSON of the item and
the two diagnostics:

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

The settings UI, physical device runtime testing, the native Photos, Files
and Shortcuts adapters, and App Store metadata are later slices of `#255`.
The provider support of this slice is the Anthropic Messages protocol; the
OpenAI wire variants are covered only as the rejection cases of the fixture.
The host does not run in the background: iOS decides when the app is
suspended, and the host stops the session at the boundary it controls
instead.
