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

`start` accepts one whole prompt for one session. The prompt is sent as UTF-8 on
stdin and stdin is closed before provider work begins. The native executable is
run directly, with an explicit working directory and canonical
`LEG_SESSION_DIR`; no shell or launcher process owns the turn. Provider
configuration and `LEG_PRETOOL_HOOK` are inherited by leg. The companion does
not resolve credentials, retry requests, rehydrate history, run tools, or write
authoritative trails.

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

The initial native trial targets are Linux and macOS. Process-tree cleanup
tests run on both platforms and check that an interrupted session trail is
recorded as well as checking process disappearance. Companion dependencies
remain in `companions/Cargo.lock`; verify the core graph with:

```sh
cargo metadata --locked --manifest-path Cargo.toml
```
