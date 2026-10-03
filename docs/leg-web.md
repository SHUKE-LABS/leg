# Leg Web host

`leg-web` is the local HTTP companion for browser clients. It embeds its
bootstrap page and JavaScript in the binary; running it needs no Node runtime,
CDN, or external service. The experimental interface in #84 can use the API
below without adding HTTP dependencies to core `leg`.

## Experimental trial bundle

Build a self-contained Linux or macOS bundle from the repository root with
`bash scripts/build-web-trial.sh --output /tmp/leg-web-trial-build`. The build
uses the root and companion lockfiles separately, and includes the compatible
native binaries, dependency notices, the quickstart, the fixture, and the
shared task/report materials. CI uploads `leg-web-experimental-<platform>-<revision>`
artifacts after its Linux/macOS and Chromium/Firefox checks pass. The artifact
is experimental and does not enter the regular release or npm packages. See
the [bundle quickstart](../companions/leg-web/trial/QUICKSTART.md) for
prerequisites, fixture setup, start/stop, and removal.

## Start and trust boundary

Build `leg`, `leg-ui-supervisor`, and the Web host, then start it:

```sh
cargo build --locked --bin leg
cargo build --locked --manifest-path companions/Cargo.toml -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path companions/Cargo.toml -p leg-web
companions/target/debug/leg-web --no-open \
  --leg-bin "$PWD/target/debug/leg" \
  --supervisor-bin "$PWD/companions/target/debug/leg-ui-supervisor"
```

The host binds only `127.0.0.1` and asks the OS for a free port. `--bind` is
accepted only as `127.0.0.1:0`; other addresses and fixed ports are rejected.
It prints the actual address and a one-time launch URL. By default it opens
that URL in the system browser; pass `--no-open` to keep startup in the
terminal. `--state-dir`, `--leg-bin`, and `--supervisor-bin` select the shared
catalog and native executables. `--event-buffer` and `--receipt-limit` set
the retention sizes; defaults are 256 and 128. Run `leg-web --help` for the
launch warning:

> Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.

The host takes a kernel-released process lock in the canonical catalog
directory. A second host for that catalog exits with an already-running
diagnostic before it can start a leg child. Provider credentials remain in the
launching environment and are never stored by the Web host.

## Launch token

Each host creates a cryptographically random 256-bit token. The token appears
once in the printed launch URL's fragment. URL fragments are not sent in HTTP
requests. The embedded page moves the token to that tab's `sessionStorage` and
removes the fragment from the address bar. Browser API calls send it in an
`Authorization: Bearer` header. The Web UI uses authenticated `fetch`
for event streams; native `EventSource` cannot set that header.

The token is not stored in the catalog, receipts, static assets, query strings,
server logs, or API responses. It rotates when the host restarts. There is no
CORS policy. The host checks every request's Host against the bound loopback
authority, rejects hostile Origin values, and requires the exact same Origin
for mutations. Static assets have no token and contain no session data.

## API

Every `/api/` request requires the launch token and the actual loopback Host.
Mutations also require the exact same-origin `Origin`. JSON bodies are limited
to 64 KiB and reject unknown fields. Errors use stable short codes and do not
echo credentials, paths, request bodies, or tokens.

| Method and path | Purpose |
| --- | --- |
| `GET /api/sessions` | List catalog sessions and parsed trail state. |
| `POST /api/sessions` | Create a draft with optional `name` and absolute `cwd`. |
| `POST /api/sessions/select` | Select a known session for a `tab_id`; selection is host memory only. |
| `GET /api/sessions/{id}` | Read one session. |
| `PATCH /api/sessions/{id}` | Rename or clear a session name. |
| `PUT /api/sessions/{id}/workspace` | Set an existing absolute workspace while idle. |
| `GET /api/sessions/{id}/snapshot` | Read the authoritative session, host turn ID, cursor, next request ID, active provisional state, and last receipt. |
| `POST /api/sessions/{id}/submit` | Submit `{ "request_id": N, "prompt": "..." }`. |
| `POST /api/sessions/{id}/stop` | Idempotently stop the host-owned run through the shared driver. |
| `GET /api/sessions/{id}/events?after=N` | Reconnect to authenticated server-sent events using `fetch`. |

The host never exposes a shell or arbitrary file operation. Workspace selection
only records a canonical existing directory; leg uses it as its working
directory, not as a sandbox. Session listing, locking, trail parsing, cwd
validation, and process ownership reuse `leg-ui-client`.

## Submission and event recovery

The snapshot supplies the next per-session monotonically increasing request
ID. The host persists its high-water mark and an accepted receipt before it
starts native leg work. For a draft, acceptance also reserves the native
`sess-...` ID and records its alias before startup; the accepted receipt may
still name the draft until `turn_start` confirms the binding. Snapshot and
event requests using that draft ID continue to resolve to the bound session,
so a reconnect can recover the native ID from the snapshot. Binding and
catalog work run outside the Tokio request worker and outside other sessions'
acceptance locks. A retry with the same ID and prompt hash returns the existing
receipt; using that ID with different content conflicts. New work for the same
logical session while a run is active is busy. IDs older than the retained
receipts remain stale because the high-water mark is durable. Receipts are
bounded to 128 per session by default; prompts are not copied into host state.

An unrelated bound session can accept and start while a draft is waiting for
its `turn_start`; a second draft can also receive its durable accepted receipt
while native session creation is serialized. Stop reaches a run during startup
or binding, and a late binding cannot restart a stopped run. A failed start or
host restart leaves an incomplete receipt for snapshot and event recovery; the
host never silently resubmits it.

Each accepted run receives a host turn ID. Host event cursors increase
monotonically per session and are persisted before an event is published. The
in-memory event buffer retains the newest 256 events by default. Reconnect with the last
seen cursor catches up without duplicates. If that cursor has expired, the
host sends a `reset` event with a current snapshot and cursor. While a run is
active, snapshots include provisional text and tool state; after completion,
the shared catalog's parsed trail is authoritative.

The receipt and host event stream do not infer success from EOF or a closed
browser connection. Completion comes from the shared driver's validated
response and process status. A lost HTTP response can be retried using the
same request ID. Closing a browser tab leaves the run alone. Stop targets only
the run owned by this host. Graceful host shutdown stops owned runs; host crash
closes the supervisor control pipe so the companion can clean up owned
processes. On restart, accepted/running receipts become incomplete and include
catalog/driver evidence. The host does not replay their prompts; a person must
submit again with a new request ID. If that explicit submission repeats the
catalog's recoverable prompt, the host uses the shared `prepare_retry` and
`confirm_retry` path; a different prompt starts a new turn.

## Browser workbench

Open the one-time launch URL printed by `leg-web`. The page keeps its launch
token in per-tab session storage and sends no provider credentials. On the
first screen, enter an absolute path to an existing folder on the host and
start a conversation. The browser cannot browse the host filesystem. Leg runs
tools as the host OS user with that folder as its working directory; it is not
a sandbox. Provider configuration comes from the environment that launched
`leg-web`, and missing binaries or setup are reported with local recovery
steps.

The transcript shows provisional streamed text and tool activity. The
effective provider and model appear from leg's `turn_start` metadata; the UI
does not invent a default. A terminal turn reply replaces its provisional
text. Stop interrupts the host-owned run. If the browser reloads or the host
connection drops, the page reconnects to a snapshot and event cursor without
resubmitting the prompt. A send whose HTTP response is lost is reconciled with
its saved request ID and prompt hash. If the host proves it did not accept the
send, **Retry same send** reuses that ID and exact text; it never creates a new
turn automatically.

Earlier replies keep their document nodes while a later turn streams, so you
can keep a link focused and select or copy history as new text arrives. Each
tab keeps its own composer draft. The transcript shows the submitted prompt
from that tab's pending send or the host snapshot, while the draft stays in the
composer.

Use **Filter by title** in the session rail to narrow the already loaded
session list. **Search this transcript** searches the selected snapshot's
prompts, replies, failure details, outcomes, and tool names, inputs, statuses,
results, and errors locally. Previous and Next move through matches; Clear removes the
query, and a no-match message is shown when nothing matches. Search includes
the full transcript text even when a large tool result is shown as a summary.
It does not submit a request or load tool details. Searching and switching
sessions leave the composer draft intact.

Copy is always an explicit action on a prompt, reply, code block, or tool input,
result, or error. Reply and tool text retain their newlines and Unicode. The UI
reports clipboard success; if clipboard access is denied or unavailable, it
shows selectable text for manual copying. Selecting or viewing transcript text
does not copy it.

**Download transcript** creates a local JSON file with schema
`leg-web.transcript/v1`. Its allowlist is `turn_index`, `prompt`, optional
`reply`, optional `failure_message`, `outcome`, and `tools`. Each tool includes
`tool_name`, `input`, and optional `result` containing `status`, optional
`result`, and optional `error`. The file omits session/catalog identifiers and
names, workspace paths, display metadata, launch tokens, and authorization
headers or keys. Credential-like fields are also removed recursively from
structured tool inputs, including generic token fields and token names using
camel, snake, or kebab case; authorization or auth headers, keys, or tokens;
API, access, refresh, session, bearer, ID, or launch tokens; client secrets and
secret, private, or signing keys; credentials; and passwords. Transcript text
and free-form tool output are kept as recorded.
Searching, copying, viewing, and downloading do not call a provider or execute
tools.

For a finished failed or interrupted turn, **Retry turn** explicitly submits
that turn's recorded prompt with the host's next request ID and leaves the
composer draft unchanged. It is disabled while the session is busy or
read-only, or a submission is unresolved. The warning is: “Retry sends this
prompt again and may repeat tool side effects.” **Retry same send** is separate:
it appears only when the host proves that a pending submission was not
accepted, then reuses that send's ID and exact text for reconciliation.

Press Enter for a newline. Press Ctrl+Enter or Cmd+Enter, or select **Send**,
to submit. Send is disabled while leg is running; a draft can still be edited
and failed prompts remain available. Markdown formatting uses local DOM
rendering: raw HTML is shown as text, unsupported or unsafe links remain text,
and model/tool output cannot load remote images or other resources.

Each turn's tool inspector is opened with its **Show details** button. It shows
the tool name and ID, literal arguments, result or error, and call/result
observation times. The local companion records times for new streamed tool
events; older saved calls without time metadata say **Timestamp unavailable**.
Calls in a running turn are **Pending**. A call with no result in an
interrupted, incomplete, or failed turn is labeled **Interrupted** or
**Missing outcome**. Denied and failed results remain separate, and capped,
incomplete, and catalog warnings remain visible apart from normal success.
Expanding a card only renders the saved display data; it does not contact the
provider.

Assistant replies use the available transcript width; submitted prompts stay
narrower and right-aligned. Soft fades at the transcript edges help clipped
lines read as scrollable content. Long histories use a bounded scrolling window
so the page does not mount every turn or tool detail at once. Focus the
transcript and use Home, End, Page Up, or Page Down to reach any part of the
conversation.
Unmounted turns are not included in the browser's Find search (Ctrl+F); use
transcript navigation to reach older content first. The browser E2E reports
session-selection and inspector feedback through the first animation frame,
plus mounted turn/detail counts and the Chromium version.

Session history browsing and detailed execution inspection are covered by
#85. The manual keyboard and screen-reader checklist is
[`companions/leg-web/tests/keyboard-screen-reader-checklist.md`](../companions/leg-web/tests/keyboard-screen-reader-checklist.md).

## Validation

Run the host package tests with:

```sh
cargo test --locked --manifest-path companions/Cargo.toml -p leg-web
cargo build --locked --bin leg
cargo build --locked --manifest-path companions/Cargo.toml -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path companions/Cargo.toml -p leg-web
python3 companions/leg-web/tests/lifecycle_smoke.py \
  companions/target/debug/leg-web \
  target/debug/leg \
  companions/target/debug/leg-ui-supervisor
python3 -m pip install --requirement companions/leg-web/tests/browser-requirements.txt
python3 -m playwright install chromium firefox
python3 companions/leg-web/tests/browser_e2e.py --browser chromium \
  companions/target/debug/leg-web \
  target/debug/leg \
  companions/target/debug/leg-ui-supervisor
python3 companions/leg-web/tests/browser_e2e.py --browser firefox \
  companions/target/debug/leg-web \
  target/debug/leg \
  companions/target/debug/leg-ui-supervisor
```

Package tests cover loopback binding, host ownership, authentication and
same-origin checks, malformed input, unknown IDs, traversal attempts, token
bootstrap, durable receipt/high-water boundaries, and event cursor recovery.
The fake-provider lifecycle smoke covers a lost submit response and duplicate
request, two tabs, disconnect/reconnect, expired cursors, an incomplete
provider stream, Stop, host crash cleanup, graceful shutdown, and restart
without prompt replay. CI runs these checks on Linux and macOS; Windows runs
the Web package tests and builds the host binary. The Chromium browser E2E
covers exact multiline input and one-submission counts across IME, Enter,
repeated clicks, network and provider retries, and refresh; live tool activity
and Stop; safe Markdown output; large tool summaries; history scroll anchoring;
and the required viewport sizes. Browser E2E runs with the Playwright-pinned
Chromium and Firefox builds on Linux; CI also runs the native host checks on
Linux and macOS. Its unauthenticated and hostile-origin POST attempts must be
rejected without changing the session request ID or reaching the fixture.
