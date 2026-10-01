# Companion client contract

`leg-ui-client` is the shared subprocess driver for human companion programs.
It lives in the independent `companions` Cargo workspace; the root `leg`
package does not depend on it and is not a workspace member. Build and test it
with:

```sh
cargo test --locked --manifest-path companions/Cargo.toml -p leg-ui-client
```

Companion executables install `leg-ui-supervisor` beside themselves and call
the library API:

```rust,ignore
let client = leg_ui_client::Client::new(leg_ui_client::ClientConfig::default());
let mut turn = client.start(TurnRequest::new(
    prompt,
    working_directory,
    LegSession::Existing(session_id),
))?;
while let Some(event) = turn.observe()? {
    // Render validated live text and tool events.
}
let outcome = turn.wait()?;
```

An event loop that reads `CatalogTurn` on a worker thread can retain a cloneable
`turn.stop_handle()` and request Stop from its input thread while the worker
continues draining stream events.

`start` accepts one whole prompt for one session. The prompt is sent as UTF-8 on
stdin and stdin is closed before provider work begins. The native executable is
run directly, with an explicit working directory and canonical
`LEG_SESSION_DIR`; no shell or launcher process owns the turn. Provider
configuration and `LEG_PRETOOL_HOOK` are inherited by leg. The companion does
not resolve credentials, retry requests, rehydrate history, run tools, or write
authoritative trails.

## Shared session catalog

TUI and Web use `SessionCatalog` from the same companion package. The catalog
stores names, the selected canonical working directory, timestamps, display
metadata, and separate per-interface drafts. It never stores process
environment values or provider credentials.

The default state root is `$XDG_STATE_HOME/leg-ui` on Linux, falling back to
`~/.local/state/leg-ui`; on macOS it is
`~/Library/Application Support/leg-ui`. Windows uses
`%LOCALAPPDATA%/leg-ui`. Set `LEG_UI_STATE_DIR` or
`SessionCatalogConfig::state_dir` to choose another root. Metadata is kept in
`catalog.json`; leg-owned JSONL trails are kept in `sessions/`.
Catalog-managed turns always pass that `sessions/` directory as
`LEG_SESSION_DIR`, even if the launching process inherited another value.
The lower-level `Client` API retains its standalone session-store resolution.

Opening or browsing the catalog reads trails without running leg or executing
tools. The browser pairs tool calls with their results and represents successful,
failed, interrupted, and incomplete turns. A compatible future event is
ignored while known history is kept. Missing or malformed trails produce
warnings and read-only entries. A trail without catalog metadata is shown as a
recovered, read-only session until a person selects an existing workspace; the
catalog never derives a workspace from tool output. If a saved workspace
disappears, submission stays blocked until a replacement is selected.

Catalog updates use a cross-process lock and atomic file replacement. New UI
sessions remain draft records until leg emits the session id at `turn_start`;
that id is recorded before later turn success or failure. Reopening or importing
a session only reads its trail. Starting another prompt and retrying a failed
or incomplete prompt are explicit operations. Retry keeps the original prompt
and returns a warning that tools may run again. The supervisor's session lock
is held until owned leg and tool processes finish; catalog workspace changes
and retries require that lock to be idle.

Use `SessionCatalog::export_transcript` to export the parsed leg trail. It
contains trail events only, without catalog metadata or inherited environment
values. Failed and incomplete turns are displayed but are not included in
resumed provider history; leg reconstructs history from complete successful
turns in its own trail.

For example, a host UI creates a draft and submits it explicitly:

```rust,ignore
let catalog = SessionCatalog::open(SessionCatalogConfig::default())?;
let draft = catalog.create_draft(
    SessionInterface::Tui,
    Some("Build session".into()),
    Some(&working_directory),
)?;
let mut turn = catalog.start_new(&draft.id, SessionInterface::Tui, prompt)?;
while let Some(event) = turn.observe()? {
    // Render validated events; turn_start binds the leg session id to the draft.
}
let outcome = turn.wait()?;
```

## Binary selection

The default resolver finds `leg` on `PATH`. A host UI may set
`ClientConfig::leg_bin` for its `--leg-bin` option. Direct native source/archive
installations are used as-is. The published `@shukelabs/leg` npm launcher is
recognized by its exported platform resolver and is resolved through Node to
the installed platform package's native `leg` executable. Symlinked global
PATH entries are canonicalized before recognition. A different script wrapper
is rejected with instructions to select a native binary.

Before a turn, the client runs the resolved executable's `--version` and
`--help`, with credentials removed, and requires the `--stream-json` capability.
Missing binaries, npm platform packages, Node.js, or stream support produce
recoverable setup errors before a paid request. The private helper can be
installed beside the UI binary or selected with `LEG_UI_SUPERVISOR_BIN`.

## Ownership and outcomes

The companion supervisor holds a kernel-released exclusive lock in the
canonical session store until both leg and its tools have exited. Existing
sessions use one lock per session ID. New-session creation briefly uses a store
guard and transfers ownership to the ID emitted by `turn_start` before exposing
that event, so another process cannot start the newly created session during
the handoff. Different sessions can run concurrently. A competing start
returns `StartError::Busy` without invoking a provider.

The supervisor is a separate process. Dropping a view does not drop the
controller's turn handle; dropping the controller handle closes its control
pipe, and controller process death does the same. The supervisor then sends
SIGINT to the owned leg process, lets leg clean up its active bash group for up
to two seconds, and only then force-kills observed owned descendants after
checking each process's start identity and process group. The session lock
stays held until cleanup finishes. Forced termination yields
`TurnOutcome::Incomplete { forced: true, .. }`.

`observe` validates `leg.exchange.stream/v1`, zero-based sequence order, event
fields, and request/response correlation. Unknown event names in this schema
are surfaced as `StreamEvent::Unknown`; an unsupported schema or malformed
record is a protocol error. The terminal response is authoritative; deltas
are provisional. `wait` returns success only for a correlated response with a
zero leg exit status; its `capped` flag carries the core tool-round warning for
the UI to display. Provider/error responses remain failures, interrupted turns
are `Stopped`, and EOF without `turn_end`, malformed framing, or an
exit/outcome disagreement is incomplete. The client never retries or replays
the prompt. Provider credentials are never included in client metadata, and
stderr/error diagnostics redact inherited credential values.

CI runs the companion suite on native Linux and macOS hosts. Its Stop and
abrupt-controller cleanup tests check for an interrupted session trail and
process disappearance on both platforms. Companion dependencies remain in
`companions/Cargo.lock`; verify the core graph with:

```sh
cargo metadata --locked --manifest-path Cargo.toml
```

## Experimental Web host

The authenticated loopback browser host and #84 first-session workbench are
documented in [leg-web.md](leg-web.md), including its launch token, API and
snapshot contract, retention limits, browser controls, accessibility checklist,
startup options, and fake-provider/browser validation. Session history browsing
and execution inspection remain #85.

## Experimental terminal UI

`leg-tui` is a separate companion package. Build the native `leg` binary and
the companion supervisor, then launch the UI with the native binary path:

```sh
cargo build --locked --bin leg
cargo build --locked --manifest-path companions/Cargo.toml -p leg-ui-client --bin leg-ui-supervisor
cargo run --locked --manifest-path companions/Cargo.toml -p leg-tui -- --leg-bin "$PWD/target/debug/leg"
```

The UI inherits provider configuration from its launching environment. Choose
an existing workspace directory, review the first-run warning and its keyboard
guide, then compose a prompt. Enter inserts a newline; Ctrl-S sends a nonblank
prompt. Left/Right move by grapheme, Home/End move within the current line, and
Backspace/Delete remove a grapheme. Ctrl-Z undoes and Ctrl-Y redoes; a bracketed
paste is one edit, preserves Unicode and line breaks, normalizes CRLF/CR to LF,
and discards other control characters. A literal `?` is prompt text. F1 opens
help, F2 opens the keyboard action menu, and Esc closes either overlay. Ctrl-C
stops a running turn; when idle it exits and keeps the draft. Editing stays
available during a turn, but another Ctrl-S is rejected while busy. The first-run
warning explains that the workspace is the tool working directory, not a
sandbox. Sending clears the editor for the next draft; if a turn stops or fails
before you edit that draft, the submitted prompt is restored.

The Linux PTY smoke test uses the local fake provider and explicit native
binary paths:

```sh
cargo build --locked --bin leg
cargo build --locked --manifest-path companions/Cargo.toml -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path companions/Cargo.toml -p leg-tui
python3 companions/leg-tui/tests/pty_smoke.py \
  --tui-bin companions/target/debug/leg-tui \
  --leg-bin target/debug/leg \
  --supervisor-bin companions/target/debug/leg-ui-supervisor
```
