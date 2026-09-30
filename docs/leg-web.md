# Leg Web host

`leg-web` is the local HTTP companion for browser clients. It embeds its
bootstrap page and JavaScript in the binary; running it needs no Node runtime,
CDN, or external service. The experimental interface in #84 can use the API
below without adding HTTP dependencies to core `leg`.

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
`Authorization: Bearer` header. The Web UI should use authenticated `fetch`
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
starts leg. A retry with the same ID and prompt hash returns the existing
receipt; using that ID with different content conflicts. New work while a run
is active is busy. IDs older than the retained receipts remain stale because
the high-water mark is durable. Receipts are bounded to 128 per session by
default; prompts are not copied into host state.

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
```

Package tests cover loopback binding, host ownership, authentication and
same-origin checks, malformed input, unknown IDs, traversal attempts, token
bootstrap, durable receipt/high-water boundaries, and event cursor recovery.
The fake-provider lifecycle smoke covers a lost submit response and duplicate
request, two tabs, disconnect/reconnect, expired cursors, an incomplete
provider stream, Stop, host crash cleanup, graceful shutdown, and restart
without prompt replay. CI runs these checks on Linux and macOS; Windows runs
the Web package tests and builds the host binary.
