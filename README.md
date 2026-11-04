# leg

`leg` binary skeleton.

## Quickstart

```
export ANTHROPIC_API_KEY=sk-...

leg ask "hello"

LEG_CONTEXT_WINDOW_TOKENS=200000 LEG_EVENT_LOG=trail.jsonl leg session
# type a message, then Ctrl-D or /exit to end the session and create trail.jsonl

leg session --resume trail.jsonl
```

See [Usage](#usage) and [Sessions](#sessions) below for the full flag set
(model selection, alternate credential env vars, `--session <id>` when a
trail holds more than one session, etc).

## Install

```
npm i -g @shukelabs/leg
```

This installs `leg`, `leg-tui`, and `leg-web`. Run `leg-tui` in a terminal or
`leg-web` to open the local browser interface. Their internal supervisor is
bundled beside the native binaries; no `LEG_UI_SUPERVISOR_BIN` setting is needed.

or build from source directly:

```
cargo install --git https://github.com/SHUKE-LABS/leg --locked
```

From a checkout, run `bash scripts/dev.sh` to update and install `leg` and its UI
companions.

Per-platform archives for every CI-supported target (`.tar.gz` on
Unix, `.zip` on Windows) are attached to each
[GitHub Release](https://github.com/SHUKE-LABS/leg/releases).
Each desktop archive contains `leg`, `leg-tui`, `leg-web`, and `leg-ui-supervisor`
(`.exe` on Windows); keep these binaries together when installing them.
The i386 bundle for iSH hosts is described below.

### Install the native binaries

npm and pnpm expose `leg` through a generated script shim, and the TUI and Web
companions cannot start a turn from one. `leg install` copies the native
binaries into a per-user directory that they can use:

```
leg install
```

- Windows: `%LOCALAPPDATA%\leg\bin`, added to the current user's PATH. The
  change applies to newly started shells only, so open a new shell afterwards.
- Unix: `$XDG_BIN_HOME`, or `$HOME/.local/bin` when it is unset. `leg install`
  never edits shell startup files; add the directory to your PATH yourself.

`leg install --dest <dir>` picks another directory, and `leg install
--no-path` leaves the Windows PATH alone. `leg install --help` lists every
option.

Run the command from the native `leg` binary of a release archive or of the
npm platform package: it installs `leg`, `leg-tui`, `leg-web`, and
`leg-ui-supervisor` from the directory that holds the running executable, and
it never copies a package-manager shim. A `cargo install` build has no
companions beside it, so it has nothing to install.

Releases are created automatically when changes land on `main`. A
Conventional Commit subject of type `feat` bumps the minor version; every
other subject bumps patch. The first release uses the current Cargo version
(`0.1.0`) unchanged.

Every published npm package includes `THIRD_PARTY_NOTICES.txt` with the
licenses of the bundled third-party Rust crates and vendored material.

## iSH bundle for iOS

Each release also attaches `leg-<version>-i686-unknown-linux-musl.tar.gz` for
hosts that run a Linux i386 userland, such as the [iSH](https://ish.app)
emulator on iOS. Unpack it inside the iSH filesystem and check the inner
checksums:

```
sha256sum --check SHA256SUMS
```

The archive holds `leg`, `leg-tui`, `leg-web`, and `leg-ui-supervisor`, the
compatibility manifest `ish-compatibility-v1.json`, and the inner
`SHA256SUMS`. The manifest records the exact target
(`i686-unknown-linux-musl`), the pinned Alpine release the bundle is tested
against (Alpine 3.20.3, image `alpine:3.20.3`, run under `linux/386`), and the
`--version` command for each binary.

This is an emulated Linux artifact, not a native iOS binary: it ships no iOS
SDK or Swift code, and the binaries cannot reach the Photos, Files, Shortcuts,
or any other iOS host API. They run with the guest's own Alpine userland and
its permissions.

### Guest inspection and provisioning

Inside the guest, `leg guest` inspects the guest and provisions the approved
Linux tools. Both commands answer with JSON:

```
leg guest bootstrap --json
leg guest provision <package> --json [--approval-token <token>] [--storage-budget-bytes <bytes>]
```

`leg guest bootstrap --json` reports the guest's Alpine release and
architecture, the pinned repository identity, the versions of the allowlisted
packages that are installed, and the free storage:

```json
{
  "schema": "leg.guest.bootstrap/v1",
  "guest": { "release": "3.20.3", "architecture": "x86" },
  "repository": {
    "release": "v3.20",
    "urls": [
      "https://dl-cdn.alpinelinux.org/alpine/v3.20/main",
      "https://dl-cdn.alpinelinux.org/alpine/v3.20/community"
    ]
  },
  "installed": { "bash": "5.2.26-r0" },
  "storage": { "free_bytes": 2147483648, "budget_bytes": 268435456 }
}
```

The guest is pinned to Alpine 3.20.3 on x86. A guest on another release or
architecture is refused with a structured error, so an incompatible guest never
reads as a healthy one.

`leg guest provision <package> --json` previews the exact candidate the
package would install — version, repository, download and install bytes, and
checksum — and issues a one-time approval token:

```json
{
  "schema": "leg.guest.provision/v1",
  "status": "approval_required",
  "package": "bash",
  "approval_required": true,
  "approval_token": "3f0c…",
  "candidate": {
    "version": "5.2.26-r0",
    "repository": "https://dl-cdn.alpinelinux.org/alpine/v3.20/main",
    "architecture": "x86",
    "download_bytes": 459098,
    "install_bytes": 1417216,
    "checksum": "sha1:5313307c0b7ae9288682f6402ade02cbf45042b7"
  },
  "verified": false,
  "cache_hit": false,
  "storage": { "free_bytes": 2147483648, "budget_bytes": 268435456 }
}
```

The preview changes nothing in the guest: the candidate is resolved against a
throwaway cache directory, and the guest's packages stay as they are. Once a
host approves the preview, repeating the command with the token installs that
exact snapshot:

```
leg guest provision bash --json --approval-token <token>
```

The approval re-resolves the candidate and installs nothing if the repository
now offers a different snapshot. The install runs `apk` with fixed arguments
against the pinned repositories, and the installed executable is verified
afterwards (`bash -lc 'printf ready'` for bash, the reported version for the
others). A request for a package the guest already holds at that version is
answered with `"status": "already_satisfied"`, and nothing is downloaded or
installed.

A package the guest holds at *another* version is refused before anything is
installed: the install would replace that version, and a failed install could
not put it back. Remove it from the guest first, then provision it again.

The approval token is a host-side credential: keep it out of any channel the
model can read, or the gate means nothing.

Only `bash`, `ffmpeg`, `python3`, and `ripgrep` can be provisioned, and only
from the pinned Alpine 3.20 repositories. Each package is resolved against the
repository that carries it, so the reported repository, version, and checksum
are the ones the install would use. The guest's own `apk` verifies the
repository signature and the package checksum before it installs anything; a
failure is reported as a structured error with a remediation, and a failed
request leaves the guest unchanged — a package whose executable fails
verification, or whose state cannot be written, is removed again:

```json
{
  "schema": "leg.guest.error/v1",
  "error": {
    "reason": "offline",
    "message": "apk --cache-dir … update failed: Can't connect to …",
    "remediation": "the guest could not reach the pinned repository; check the guest's network"
  }
}
```

The default storage budget is 256 MiB of installed packages;
`--storage-budget-bytes <bytes>` overrides it. The guest state — the
installed-package record and the pending approval tokens — lives in
`LEG_GUEST_STATE_DIR`, else `XDG_STATE_HOME/leg/guest`, else
`~/.local/state/leg/guest`.

The provisioned tools stay inside the guest: they are ordinary Alpine
packages, they cannot grant native iOS or Android permissions, and they do
not change how the desktop binaries install packages.

### The capability bridge

Inside the guest, `leg-bridge` carries a capability request to the host that
owns the native permission. A mobile Skill calls it through the existing
`bash` tool, with one request on standard input and one answer on standard
output:

```
printf '%s\n' '{"version":1,"request_id":"r-1","capability":"photos.list","input":{"limit":5}}' \
  | leg-bridge
```

The answer always has the same shape and exactly one of six statuses — `ok`,
`approval_required`, `denied`, `cancelled`, `unavailable`, or `error`:

```json
{"version":1,"request_id":"r-1","status":"ok","result":{},"error":null}
```

The bridge validates the request before the host sees it: an unknown version,
an unknown capability, an unknown field, a malformed document, an oversized
request, a reused request id, and a path that is not relative and normalized
are all refused without a dispatch. The request ids are kept unique by a
locked ledger in the bridge state directory, so two invocations that share it
cannot dispatch the same id. A capability that needs consent answers
`approval_required` with a review summary and a one-time token, and runs
nothing until the Skill replays that token with a fresh request id, the same
capability, and the same input.

The bridge talks to the host over one fixed transport — a Unix socket named
by `LEG_BRIDGE_HOST_SOCKET` — and answers `unavailable` when the host
configured none. It adds no tool to the desktop registry and changes no
Skills contract: the host owns the permission prompt, the dispatch, the
cancellation, and the user confirmation. The [capability bridge
contract](docs/capability-bridge.md) defines the schemas, the capability
names, the approval semantics, the limits, the audit summary, and the rule a
platform adapter has to keep.

## Native iOS host

The repository also carries a native iOS host under [`ios/`](ios/README.md):
a standalone Xcode project (`LegHost`) that embeds one pinned iSH revision. On
launch it verifies the pinned guest bundle and the pinned Alpine i386 root
against their SHA-256 digests, imports them into a guest root with the vendored
iSH importer, boots the vendored runtime and runs `/usr/local/bin/leg-tui` on a
pseudo terminal that is drawn with hterm. The terminal accepts the software
and the hardware keyboard, reports its size to the guest and forwards
interrupts; the keys the software keyboard cannot send, the interrupt among
them, are offered on the screen. It is built and installed separately from the
emulated bundle above, and it links no proprietary `leg` source: `leg` runs
inside the guest, as a separate program.

The host pins its two inputs by URL and SHA-256:

| Input | URL | SHA-256 |
| --- | --- | --- |
| leg guest bundle `v0.43.1` | `https://github.com/SHUKE-LABS/leg/releases/download/v0.43.1/leg-0.43.1-i686-unknown-linux-musl.tar.gz` | `c7ba65aa24e127039d0292ae1db8b8573fbc07565e5ea40805630dd58305277d` |
| Alpine i386 root `3.20.3` | `https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86/alpine-minirootfs-3.20.3-x86.tar.gz` | `4c7ba1c9e6642692dd5cb302e5e58943221901f2e15991af7d5cb4ded80dc7b9` |

Build, install, run and reset it with:

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcodebuild -project ios/LegHost.xcodeproj -scheme LegHost -sdk iphonesimulator \
    -configuration Release -derivedDataPath build/dd build
xcrun simctl install "iPhone 17 Pro" \
    build/dd/Build/Products/Release-iphonesimulator/LegHost.app
xcrun simctl launch "iPhone 17 Pro" com.shukelabs.leg.ioshost
xcrun simctl uninstall "iPhone 17 Pro" com.shukelabs.leg.ioshost
```

The last command resets the host: it removes the provisioned guest root, so
the next launch imports it again. See [ios/README.md](ios/README.md) for the
pinned inputs, the build, install and signing commands, the host fixtures and
the UI tests.

### On a physical iPhone

The same host runs on a physical iPhone. The device build needs a signing team,
and the signing needs the GUI session of the console user: a plain ssh session
cannot use its login keychain, and `codesign` fails with
`errSecInternalComponent`. Run the whole device build in that session, with
the complete command inside the wrapper:

```
u=$(id -u); n=$(id -un)
sudo -n launchctl asuser "$u" sudo -n -u "$n" env \
    DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer HOME="/Users/$n" \
    /bin/bash -c 'cd <repo> && xcodebuild -project ios/LegHost.xcodeproj -scheme LegHost \
        -sdk iphoneos -configuration Release -derivedDataPath build/dd-device \
        -destination "generic/platform=iOS" -allowProvisioningUpdates \
        DEVELOPMENT_TEAM=<team-id> build'
```

The install, the launch and the uninstall go through `devicectl`, and they work
from a plain ssh session:

```
xcrun devicectl device install app --device <device> \
    build/dd-device/Build/Products/Release-iphoneos/LegHost.app
xcrun devicectl device process launch --device <device> com.shukelabs.leg.ioshost
xcrun devicectl device uninstall app --device <device> com.shukelabs.leg.ioshost
```

The launch controls of the host follow a `--`, or `devicectl` reads them as
options of its own:

```
xcrun devicectl device process launch --device <device> \
    com.shukelabs.leg.ioshost -- -leghost-fixture success -leghost-provider anthropic
```

A Release build on a clean device stops at the provider boundary with
`IOS_PROVIDER_CONFIG_MISSING`: the host reads the provider configuration from
the Keychain of the device, and it has no UI to write that item yet (the
settings UI is a later slice), so nothing is provisioned, booted or started.
The guest path on a device is proved with the DEBUG build and the loopback
fixture.

The guest data lives in the container of the app. A relaunch keeps it: the
verified root, the workspace of the guest, the trail and the durable ledger of
the requests. An uninstall clears it: the next launch imports the root again
and the guest asks for a workspace again.

The checklist of a run on an iPhone 12, with the evidence of every item, is in
[ios/physical-device-validation.md](ios/physical-device-validation.md).

## Experimental TUI trial

The terminal interface is experimental and ships with the npm package.
For a self-contained trial with fixtures, download a `leg-tui-experimental-*` bundle
from a Linux or macOS CI run, or build one with the command in the
[TUI trial quickstart](companions/leg-tui/trial/QUICKSTART.md). Run it with a
disposable workspace and the local fixture; stop the TUI and remove the
unpacked bundle directory when finished. This trial does not change how the
regular `leg` binary is installed.

## Experimental Web trial

The Web companion is experimental and ships with the npm package.
For a self-contained trial with fixtures, get the Linux or macOS bundle from the CI run's
`leg-web-experimental-*` artifact, or build one with the command in the
[Web trial quickstart](companions/leg-web/trial/QUICKSTART.md). The bundle
embeds its page assets and runs locally; its tools use your OS account, with
the selected workspace as their working directory. The workspace is not a
sandbox. Stop the host with Ctrl-C and remove the unpacked bundle directory
when finished.

## Usage

```
ANTHROPIC_API_KEY=sk-... leg ask [--model <model>] "prompt"
ANTHROPIC_API_KEY=sk-... leg ask --image chart.png "Describe this chart"
```

Use repeatable `--image <path>` flags to attach local JPEG, PNG, GIF, or WebP
images to an `ask` turn. Image type is detected from the file contents; each
base64-encoded image is limited to 10 MB, with a 32 MB cap on the serialized
image request.

Prints the assistant reply on success. A provider or delivery failure
(bad credentials, unreachable base URL, etc.) leaves stdout empty, reports an
error including `kind: error` on stderr, and exits non-zero. Configuration
failures (missing or malformed environment values or hook config files) also
exit non-zero.
Also accepts `ANTHROPIC_AUTH_TOKEN` for bearer keys of Anthropic-compatible
endpoints, and `CLAUDE_CODE_OAUTH_TOKEN`. Claude subscription OAuth tokens
(including `claude setup-token`) are unsupported outside Claude Code; use an
Anthropic Console API key with `ANTHROPIC_API_KEY` instead. Other settings
include `ANTHROPIC_BASE_URL`, `LEG_MODEL`, `LEG_TIMEOUT_SECS`,
`LEG_STREAM_IDLE_TIMEOUT_SECS`, `LEG_BASH_TIMEOUT_SECS`, `LEG_MAX_TOKENS`,
`LEG_THINKING`, `LEG_THINKING_MAP`,
`LEG_CONTEXT_WINDOW_TOKENS`, `LEG_CONTEXT_RESERVE_TOKENS`,
`LEG_CONTEXT_KEEP_RECENT_TOKENS`,
`LEG_MAX_TOOL_ROUNDS`, `LEG_MAX_RETRIES`, `LEG_RETRY_BASE_DELAY_MS`,
`LEG_HOOKS_CONFIG`, `LEG_PRETOOL_HOOK`, `LEG_SYSTEM_PROMPT`, and
`LEG_EVENT_LOG`.

`LEG_TIMEOUT_SECS` defaults to 60 and bounds DNS lookup, connection setup,
request sending, response headers, and the total body read for non-streaming
responses. Response-header timeouts are retried on both streaming and
non-streaming request paths up to `LEG_MAX_RETRIES`. Successful streamed
responses may run longer while data keeps arriving;
`LEG_STREAM_IDLE_TIMEOUT_SECS` defaults to 120 and limits how long a stream may
remain silent between received chunks.

`LEG_MAX_TOKENS` sets the output-token limit requested for each provider reply
(default `32000`).

`LEG_THINKING` selects the provider-neutral reasoning level
(`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`), defaulting to
`high`. Leg maps the level through a built-in model capability table and
clamps unsupported levels down to the nearest supported level; it never raises
the requested level. `off` omits thinking/reasoning fields. For budget-style
models, at least 1,024 output tokens remain available for the visible answer.
For Anthropic's current Claude family, `opus`, `sonnet`, and `haiku` are
recognized aliases; a trailing `[1m]` is ignored for capability lookup. The
configured model id is still sent to the provider unchanged.

`LEG_THINKING_MAP` optionally supplies a JSON object mapping levels to provider
values, for example `{"high":"medium"}`; `null` marks a level unsupported.
This is useful for model ids not in the built-in table. An unknown model with
no usable override omits the thinking parameter and emits one warning naming
the model and `LEG_THINKING_MAP`. For budget-style models, non-null values must
be decimal budgets of at least 1,024; invalid values fail configuration
loading.

### Context-window budgeting

`LEG_CONTEXT_WINDOW_TOKENS` is the model's context window, in tokens. Set it
explicitly for `leg session` and session-backed `leg exchange`; leg does not
infer model limits. A missing or invalid window fails before a provider call.
`LEG_CONTEXT_RESERVE_TOKENS` defaults to `LEG_MAX_TOKENS`. The usable input
budget is `window - reserve`, and reserve must be positive and smaller than the
window. `LEG_CONTEXT_KEEP_RECENT_TOKENS` defaults to
`min(8192, (window - reserve) / 2)` and must be positive and smaller than the
usable input budget.

Before each provider request, leg estimates the complete serialized request
(system prompt, tool declarations, messages, and protocol framing) as
`ceil(UTF-8 bytes / 3)`. Provider-reported `input_tokens` calibrate later
estimates conservatively; missing usage never disables the guard. When a
session crosses its budget, leg summarizes dropped complete turns and keeps a
recent suffix, preserving the active user turn and any complete tool chain.
The internal summary call has no tools and may return at most
`min(4096, LEG_CONTEXT_RESERVE_TOKENS)` tokens. Leg injects that summary as a
system-context prefix. A provider context-window rejection triggers at most one
compact-and-retry; an active turn that still cannot fit returns
`kind: context_window`.

Tool results sent to a provider or internal summary are capped at 2,000 source
characters followed by `\n[tool result truncated by leg]`; the complete result
remains in the append-only session trail. Each compaction adds a
`baton.exchange/v1` `compaction` event before the request that uses it. Resume
uses the latest summary and retained suffix without rewriting earlier trail
lines. Older trails without compaction events remain readable and compact on
demand. Stateless calls keep their one-turn result format; if context settings
are supplied, their tool continuations still use the budget guard.

At the start of every `ask`, `exchange`, and session turn, leg discovers project
instructions from `AGENTS.md`. In a git repository it reads files from the
repository root down to the current working directory, with the nearest file
last; outside a repository it reads only the current working directory's file.
The resulting system prompt is `LEG_SYSTEM_PROMPT`'s file content followed by
those files, separated by blank lines. Discovery is refreshed for every turn
and is not stored in session history or trails. The [Skills v1
contract](docs/skills-v1.md) defines the separate selectable Skills layer:
`AGENTS.md` is always-on project instruction, while a Skill is loaded only
after explicit selection and never replaces project instructions.

The Skills v1 catalog discovers packages from project `.agents/skills`,
`.codex/skills`, `.claude/skills`, and `.pi/skills` roots, walking from the
current directory up to the repository root; without a repository, only the
current directory is checked. All project roots precede user roots. User
roots are `~/.agents/skills`, `$CODEX_HOME/skills` (default `~/.codex/skills`),
`~/.claude/skills`, and `~/.pi/agent/skills`. The nearest project directory
wins, followed by that root-kind order. Within a root, normalized relative
paths decide duplicate winners. Catalog warnings use `invalid_package`,
`unreadable_root`, `unreadable_path`, `unreadable_skill`, `invalid_root`,
`path_escape`, `package_changed_during_discovery`, `duplicate_skill`, and
`catalog_budget_exceeded`; missing roots are skipped. Discovery reads only the
bounded YAML frontmatter, never the Skill body or package resources. Catalog
entries are omitted whole to fit the supplied serialized JSON byte budget;
diagnostics remain visible and may exceed a very small budget. Models can call
`activate_skill(snapshot_id, name)` to load an exact catalog entry, then
`read_skill_resource(snapshot_id, name, relative_path)` for a package resource.
Successful calls return acknowledgements only; loaded content reaches the model
through the next provider request, while failures return bounded diagnostics.
Activations are turn-local and do not carry into a
later turn or resumed session, so the model must activate again against the
fresh catalog. Complete `SKILL.md` and resource files are limited to 64 KiB;
resource paths are limited to 512 bytes, and combined loaded Skill content is
limited to 256 KiB per turn. The [Skills v1 contract](docs/skills-v1.md)
documents path confinement and context-budget behavior. When context settings
are supplied, Skill metadata and loaded content count toward the input budget;
an activation or resource read that cannot fit fails before adding content.

For example, add a package to a workspace and request it by name:

```text
.agents/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft concise release notes from a change list.
---
Group changes by user-visible effect. Call out breaking changes separately.
```

```sh
leg ask "Use the release-notes Skill to summarize these changes: ..."
```

Leg first sends the package's metadata, then loads its instructions only after
the model calls `activate_skill` with the current snapshot and exact name.
Each new turn, including a resumed session, discovers a fresh snapshot and
starts with no activated body or resources. Precedence is configured
`LEG_SYSTEM_PROMPT`, project `AGENTS.md`, the caller's request, then activated
Skill instructions and resources. Bodies and resources do not enter session
history or trails; catalog metadata and activation outcomes are shown on
stderr, and tool calls and acknowledgements appear in a trail when recording
is enabled.

Anthropic Messages remains the default provider. OpenAI-compatible endpoints
can use either streamed wire protocol:

```
LEG_PROVIDER=openai-chat-completions OPENAI_API_KEY=sk-... leg ask "hello"
LEG_PROVIDER=openai-responses OPENAI_API_KEY=sk-... leg ask "hello"
```

Both OpenAI protocols share `OPENAI_API_KEY` and `OPENAI_BASE_URL`
(`https://api.openai.com/v1` by default). `OPENAI_BASE_URL` may name the API
root or end in `/v1`; leg appends the selected endpoint. `LEG_MODEL` is passed
through unchanged and defaults to `gpt-4.1-mini` for these protocols. OpenAI
credentials and base URL are independent of the Anthropic settings.

### Tool loop

`ask`, `session`, and `exchange` share one tool loop. It runs only when a
reply requests tools (`stop_reason: tool_use`): each call is executed and
its result sent back until the model answers. `ask` and `exchange` print only
that final reply; interactive `session` flushes assistant text as it streams,
while keeping tool arguments and results out of stdout.
`LEG_MAX_TOOL_ROUNDS` optionally sets a positive round limit; unset or blank is
unbounded. With a configured limit, if the reply still requests tools after
that many rounds, `leg` sends no further request and warns on stderr. A call
to an unregistered tool is answered with an error result.

### Transient provider retries

`LEG_MAX_RETRIES` sets the number of retries after the first provider request
(default `2`; `0` disables retries). `LEG_RETRY_BASE_DELAY_MS` sets the
exponential-backoff base (default `250` ms). Each retry uses full jitter from
zero through `min(base_delay * 2^(retry_number - 1), 30 seconds)`.
`Retry-After` values in delay-seconds or HTTP-date form take precedence and are
capped at 30 seconds.

Retries apply to transient connection failures (I/O, timeouts, DNS, connection
establishment, and proxy connection) and HTTP 408, 429, 500, 502, 503, 504, and
529. TLS/protocol setup failures outside those connection classes, other 4xx
responses, response-body read failures, and decode errors are surfaced without
retry. The `attempts` field on `response_ok` and `response_error` trail outcomes
counts all provider-call attempts in that exchange, including retries and calls
across tool-loop rounds; it is absent in older trail records, which remain
readable. `stop_reason` is present on `response_ok` when known and on
`response_error` when a reply hit its token limit; error records preserve the
provider's raw reason. It is absent in older records and when unknown.

`LEG_HOOKS_CONFIG` optionally names a hooks v1 JSON file. Otherwise leg checks
`.leg/hooks.json` in the current working directory, then the per-user config
file. The first available file wins; files are not merged. Each JSON entry has
an ID, one event, an executable path, and optional `enabled` and tool matcher
fields. Exact tool names and `*` are supported; selected entries keep file
order, and disabled entries are skipped. Invalid selected files fail
configuration. The full v1 schema, event list, platform paths, precedence,
validation rules, and legacy compatibility are documented in
[docs/hooks-v1.md](docs/hooks-v1.md). Project discovery is relative to the
current working directory and does not walk upward to the Git root. Project
hook files are trusted as-is. Hook commands run with the operating-system
permissions of the user running leg.

For example, this project hook runs before each turn:

```json
{
  "version": 1,
  "hooks": [
    { "id": "audit-turn", "event": "TurnStart", "command": "hooks/audit-turn" }
  ]
}
```

Hooks receive one JSON payload on stdin. Turn and tool payloads include the
event name and working directory; session turns also include their session ID
and turn index. A `TurnStart` payload looks like:

```json
{"hook_event_name":"TurnStart","cwd":"/workspace/project","session_id":"sess-123","turn_index":2,"prompt":"Review the change"}
```

`LEG_PRETOOL_HOOK` remains usable as a single pre-tool executable. It receives
the `PreToolUse` JSON payload on stdin, including `hook_event_name`, `cwd`,
`tool_name`, and `tool_input`; `session_id` and `turn_index` are included when
the call belongs to a session turn.
Exit 0 with empty or whitespace-only stdout, or `{"decision":"allow"}`,
allows the call. Exit 0 with
`{"decision":"deny","reason":"..."}` denies it and returns that reason to the
model. A non-zero exit, spawn failure, malformed, truncated, or invalid UTF-8
output, 30-second timeout, or cancellation denies with a generic reason.
Declarative hooks run sequentially for the six documented events;
`PreToolUse` is fail-closed, while other event failures are reported to stderr
without changing the operation result. See [docs/hooks-v1.md](docs/hooks-v1.md)
for payloads, ordering, and session-resume behavior.

### Tools

- `read` — returns a UTF-8 text file's contents. Args: `path` (relative to
  the working directory, or absolute), `offset` (1-indexed start line), and
  `limit` (max lines). Output is capped at 2000 lines or 50 KB, whichever
  comes first, keeping whole lines only. When content is withheld, the result
  ends with a continuation notice such as
  `[Showing lines 1-2000 of 5000. Use offset=2001 to continue.]` (or
  `[N more lines in file. Use offset=M to continue.]` when `limit` stopped
  early); a result reaching end of file carries no notice.
- `write` — writes `content` to a file as UTF-8. Args: `path` (relative to
  the working directory, or absolute) and `content`. Missing parent
  directories are created. A new file can always be written, but an existing
  file can be overwritten only after `read` has read it successfully in the
  same run; otherwise the call fails and the file is left untouched. On
  success it returns `Successfully wrote to <path>`.
- `edit` — replaces an exact string in a UTF-8 file. Args: `path`,
  `oldString`, `newString`, and optional `replaceAll` (default `false`).
  Matching is exact byte-for-byte text: no regex, no fuzzy or
  whitespace-tolerant fallback. The call fails, leaving the file untouched,
  when `oldString` is empty, equals `newString`, is not found, or matches more
  than once (overlapping occurrences count) without `replaceAll`. On success
  it returns `Successfully replaced N occurrence(s) in <path>.` followed by a
  unified diff (one line of context) capped at 32 rows; a longer diff ends with
  `... [diff truncated: N more lines]`.
- `bash` — runs `command` using `bash -lc` in the process working directory,
  as the caller's OS user. Optional `description` records a short explanation
  with the tool call. Optional `timeout` is a non-negative integer number of
  seconds, defaulting to 120. `LEG_BASH_TIMEOUT_SECS` sets this default to a
  positive integer; an explicit per-call `timeout` takes precedence. It runs
  synchronously; a timeout reports status `timed_out` and exit code `124`,
  retains output captured before termination,
  and terminates the shell and descendants (50 ms graceful period, then force
  termination; Windows uses a job object). It stops draining inherited output
  pipes after a 2-second guard. Each output stream is captured separately and
  capped at 2000 lines or 50 KB, whichever comes first, keeping its head and
  tail and showing the omitted-byte count. The tool result is JSON with
  `wall_time_seconds`, `status` (`exited` or `timed_out`), `exit_code`, `stdout`,
  `stderr`, `stdout_omitted_bytes`, and `stderr_omitted_bytes`. Leg's provider
  credential variables are removed from the child environment; login-shell
  startup files may re-export them. A non-zero command exit is returned as
  `exit_code`; a missing `bash` executable is a tool error.

#### Headless contract for agent callers

A caller driving `leg ask` or `leg exchange` sets the working directory:
every tool resolves relative paths against, and `bash` runs in, the process
cwd. The whole tool loop runs inside the one invocation — tool calls and
results are never written to stdout. On success, `ask` prints only the final
reply and `exchange` writes one response. On provider/delivery failure, both
exit non-zero: `ask` and plain-text `exchange` leave stdout empty and report
an error on stderr; envelope `exchange` writes its `kind:"error"` response
before reporting the failure. To observe tool steps, set `LEG_EVENT_LOG` on
`ask` or `exchange` and read the trail back with `leg log show`. The `bash`
tool needs `bash` on `PATH` (Git Bash on Windows).
`tests/headless_e2e.rs` exercises this contract end to end against a fake
provider.

On Unix, SIGINT or SIGTERM during `ask`, `exchange`, or `session` interrupts
the active turn, terminates a running `bash` process group, records an
`interrupted` outcome in any enabled trail, writes no further stdout, and
exits 130 or 143 respectively. A second signal exits immediately. An
interrupted session trail can be resumed with `leg session --resume`; Windows
keeps its default console-control behavior.

### Sessions

```
LEG_CONTEXT_WINDOW_TOKENS=200000 LEG_EVENT_LOG=trail.jsonl ANTHROPIC_API_KEY=sk-... leg session
```

Runs an interactive multi-turn REPL: each line typed is sent with the retained
conversation context, and assistant text is flushed as it streams. Ctrl-D or a
lone `/exit` line ends the session cleanly. Every turn (and, with
`LEG_EVENT_LOG` set, the session's start/end) is appended to the JSONL trail,
keyed by a `session_id` minted for the run.

```
leg session --resume trail.jsonl [--session <id>]
```

Reopens a prior session's trail, rehydrates its retained conversation history —
including each turn's tool rounds — and continues appending new turns to the
same file. A compacted trail restores its latest summary and retained suffix.
`--session <id>` selects
which session to resume when the trail holds more than one; it is required
in that case and otherwise optional.

### Log replay

```
leg log show [--file <path>]
leg log replay [--file <path>] [--index <N>]
```

`log show` prints every complete exchange in a JSONL trail (`--file`, or
`LEG_EVENT_LOG` when omitted), with each tool call made within it and that
call's result. `log replay` re-runs one logged exchange's
prompt — the last one, or `--index <N>` (1-based) — against the *current*
environment's credential, model, and base URL taken from the log entry;
timeout, max tokens, and system prompt still come from today's environment.
A tool-bearing exchange reruns only its prompt: the current tool loop
executes the tools afresh (stored tool results are never fed back). The
replay's own request, tool, and outcome lines are appended to `LEG_EVENT_LOG`
like any other `ask`.

Between a turn's `request` and its outcome, the trail records each dispatched
tool round as a `tool_round` line (`content`: the `tool_use` reply's blocks,
text included), then each of that round's calls as a `tool_call` line
(`tool_use_id`, `tool_name`, `input`), followed by exactly one `tool_result`
line with the same `tool_use_id`, a `status` of `completed`, `failed`, or
`denied`, and the tool's `result` or `error`. All three carry `schema` and
`ts_ms`, plus `session_id`/`turn_index` on session turns; sessionless
`ask`/`exchange` events omit those fields.

On a first-signal interruption, an active tool call gets a failed
`tool_result` before the turn's `response_error` outcome; the interrupted
session trail can therefore be resumed without reusing the failed turn.

### Exchange

```
leg exchange [--in <path>] [--out <path>] [--session <id>|--new-session|--new-session-id <id>] [--session-id-out <path>] [--stream-json]
```

Answers one `baton.message/v1` request (from `--in`, or stdin) with exactly
one response (to `--out`, or stdout); a plain-text request gets the reply
body alone. The tool loop runs inside that single exchange. `leg exchange`
is the headless entry point for adapters. If `LEG_EVENT_LOG` is non-blank,
it appends request, tool, and outcome events to the trail for `leg log show`.

By default, each exchange is cold and independent. Use `--new-session` to
create a persistent session, `--session <id>` to continue one, or
`--new-session-id <id>` to create one under a preallocated native `sess-...`
ID. The session selection flags are mutually exclusive. The preallocated ID
must not already have a trail; creation refuses an existing trail atomically.
`--session-id-out <path>` may accompany any session selection and writes the
session id plus a newline after the turn. Each session is stored as
`<id>.jsonl` in `$LEG_SESSION_DIR`, or `$XDG_STATE_HOME/leg/sessions`, or
`~/.local/state/leg/sessions` when neither variable is set. The store directory
is created when a new session is started. A missing id fails before contacting
the provider with `leg: no session found: <id>`.

Pass `--stream-json` to write a flushed NDJSON feed using the
`leg.exchange.stream/v1` schema. This mode always writes to stdout and cannot
be combined with `--out`. With `--session`, `--new-session`, or
`--new-session-id`,
`--session-id-out` remains available and writes that id after the turn. Each
line has `schema`, `event`, and a zero-based increasing
`seq`. Events are `turn_start`, `text_delta`, `tool_round`, `tool_call`,
`tool_result`, and `turn_end`. Provider-round and content-block indices start
at zero. Session records also carry `session_id` and `turn_index`. Stdout is
the NDJSON stream; human-readable warnings are written to stderr.

Example output (each line is one JSON value):

```jsonl
{"schema":"leg.exchange.stream/v1","event":"turn_start","seq":0,"provider":"anthropic","model":"claude-test","request":{"schema":"baton.message/v1","message_id":"m-1","conversation_id":"c-1","from":"external","to":"leg","in_reply_to":null,"kind":"request","body":"hello","ts_ms":1,"exchange":null}}
{"schema":"leg.exchange.stream/v1","event":"text_delta","seq":1,"round_index":0,"block_index":0,"text":"Hello"}
{"schema":"leg.exchange.stream/v1","event":"turn_end","seq":2,"capped":false,"response":{"schema":"baton.message/v1","message_id":"c-1-r-2-0","conversation_id":"c-1","from":"leg","to":"external","in_reply_to":"m-1","kind":"response","body":"Hello","ts_ms":2,"exchange":{"schema":"baton.exchange/v1","exchange":{"request":{"ts_ms":1,"model":"claude-test","base_url":"https://api.anthropic.com","prompt":"hello"},"outcome":{"event":"response_ok","ts_ms":2,"duration_ms":1,"reply":"Hello","input_tokens":1,"output_tokens":1,"stop_reason":"end_turn","attempts":1}}}}}
```

`turn_start.request` and `turn_end.response` are the correlated message
envelopes; the terminal response is authoritative. Text deltas are provisional
and are not repeated as a second final-text event. `turn_end.capped` is true
only when `LEG_MAX_TOOL_ROUNDS` stops a turn while the provider still requests
tools. Provider failures and capped turns still end with a terminal envelope.
Unix cancellation attempts an `interrupted` terminal record while stdout is
writable; a broken pipe stops further tool dispatch and exits non-zero. EOF
without `turn_end` means the exchange is incomplete. No records follow
`turn_end`. Unknown optional fields and events may be ignored by consumers of
this schema version. See [the stream contract](docs/exchange-stream.md) for
tool-event fields, session rules, and complete error/truncation behavior.

## Development checks

CI uses Rust 1.89.0 with the `rustfmt` and `clippy` components. Install that
toolchain and run the same lint checks locally:

```
rustup toolchain install 1.89.0 --component rustfmt --component clippy
cargo +1.89.0 fmt --all -- --check
cargo +1.89.0 clippy --locked --all-targets -- -D warnings
```

## CI-supported targets

- x86_64-unknown-linux-musl (static; cross-compiled)
- aarch64-unknown-linux-musl (static; cross-compiled)
- x86_64-apple-darwin
- aarch64-apple-darwin
- x86_64-pc-windows-msvc
- armv7-unknown-linux-musleabihf (cross-compiled; build-only, no native runner)
- i686-unknown-linux-musl (static; cross-compiled; ships only as the iSH/Alpine
  i386 bundle)

Linux release binaries use musl and do not require a minimum glibc version.
