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

Catalog updates use a cross-process lock and atomic file replacement. Starting
a new session writes a tokenized pending attempt with the controller's process
birth identity and a candidate session ID, then releases the catalog lock
before startup. The supervisor records its own process identity before it may
launch leg. Once the managed trail exists, it writes a token-matched handoff
for that exact session before the controller binds the draft at `turn_start`.
Reopening or reading the catalog reconciles unfinished attempts from those
records, process identities, the session lock, and the trail; it never starts
leg or calls a provider. It binds a completed attempt only when the handoff and
trail match and all owners and locks are gone. A live or ambiguous attempt
stays pending with an actionable warning. Stale handoff, failure, and recovery
updates cannot change a newer attempt.

Reopening or importing a session only reads its trail. Starting another
prompt and retrying a failed or incomplete prompt are explicit operations.
Retry keeps the original prompt and returns the shared warning
`Retry sends this prompt again and may repeat tool side effects.` The
supervisor's session lock is held until owned leg and tool processes finish;
catalog workspace changes and retries require that lock to be idle.
Display status inspects an existing primary lock while holding its short-lived
coordination sidecar, named by appending `.coord` to the primary `.lock` path.
A missing primary lock means idle and creates neither that primary lock nor a
coordination sidecar. Supervisors use the same sidecar only while inspecting or
initially acquiring the primary lock, then release it immediately; waiting for
session creation happens outside it. The sidecar coordinates inspection, while
only the persistent primary session lock represents ownership through leg and
tool cleanup.

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
sessions acquire only their per-session lock, so they can start while an
unrelated new session is still binding. New-session creation reserves a
native `sess-...` candidate ID. The supervisor acquires that ID's session lock
before launching leg; a short store creation guard serializes startup until
the token-matched trail handoff is durable. Leg creates the trail with
exclusive file creation, so a preexisting trail is never
overwritten. Different sessions can run concurrently. A competing start
returns `StartError::Busy` without invoking a provider.

Older pending drafts may have no attempt identity or session association. They
remain `Unknown`; recovery preserves both the draft and any orphan trail and
does not resend the prompt. Before copying one into a new conversation, stop
the companion that owned it, wait for its supervisor and native `leg` process
to finish cleanup, and confirm those processes have exited in the operating
system's process viewer. If ownership cannot be confirmed, leave the draft
pending and do not submit it again.

To preserve a legacy draft, keep its Web tab open, copy the composer text and
the `Workspace:` path, then choose **New conversation**, enter that same
workspace, and paste the text into the new composer. The old draft and any
orphan trail remain in the catalog. Web keeps unsent composer text in that
tab's session storage. If the text was saved in catalog metadata by an
interface such as the TUI, print pending drafts with no session handoff from
`catalog.json` under the state root described above. Set `CATALOG` to that
file's full path:

```sh
python3 - "$CATALOG" <<'PY'
import json, sys

with open(sys.argv[1], encoding="utf-8") as catalog_file:
    sessions = json.load(catalog_file)["sessions"]
for draft_id, record in sorted(sessions.items()):
    attempt = record.get("pending_attempt") or {}
    if not record.get("pending_new_turn") or attempt.get("native_session_id"):
        continue
    print(f"draft: {draft_id}; name: {record.get('name') or ''}")
    print("workspace:", record.get("cwd") or "")
    for interface, text in sorted(record.get("drafts", {}).items()):
        print(f"\n[{interface} draft]\n{text}")
PY
```

Use the printed workspace and desired interface draft when creating the new
conversation. Keep the old catalog entry and trail so the original attempt's
history is not lost. If the composer text is no longer in the Web tab or in
catalog metadata, it cannot be recovered from the trail; the trail remains
preserved for inspection.

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
the UI to display. The wrapped response also carries the provider's
`stop_reason`, including `max_tokens`, so a UI can distinguish a truncated
answer. Provider/error responses remain failures, interrupted turns are
`Stopped`, and EOF without `turn_end`, malformed framing, or an exit/outcome
disagreement is incomplete. The client never retries or replays the prompt.
Provider credentials are never included in client metadata, and stderr/error
diagnostics redact inherited credential values.

CI runs the companion suite on native Linux and macOS hosts. Its Stop and
abrupt-controller cleanup tests check for an interrupted session trail and
process disappearance on both platforms. A native Windows ConPTY lane checks
the TUI Stop path against the owned tool process tree.

The native Windows build 26300 ConPTY behavior run passed, including
`stop_owned_tool_process_tree`. The harness drives ConPTY directly through
pywinpty and does not cover Windows Terminal frontend behavior.

Companion dependencies remain in `companions/Cargo.lock`; verify the core
graph with:

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
the companion supervisor, then launch the UI with their native binary paths:

```sh
cargo build --locked --bin leg
cargo build --locked --manifest-path companions/Cargo.toml -p leg-ui-client --bin leg-ui-supervisor
cargo run --locked --manifest-path companions/Cargo.toml -p leg-tui -- \
  --leg-bin "$PWD/target/debug/leg" \
  --supervisor-bin "$PWD/companions/target/debug/leg-ui-supervisor"
```

The UI inherits provider configuration from its launching environment. It
requires terminals on both stdin and stdout; `leg-tui --help` also works with
redirected streams. On first use, choose a workspace directory, review the
warning and keyboard guide, then compose a prompt. Later starts open the session
picker when the catalog contains sessions. The first-run warning reads exactly:

> Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.

When another interface has already populated the catalog, the TUI still shows
the warning before starting its first new session or sending a prompt. Enter
records the acknowledgement in the catalog for later TUI sessions.

The minimum terminal size is 80 columns by 24 rows. Below that size the UI asks
you to resize; it keeps the draft and active turn, and rejects sends until the
terminal is large enough again. `NO_COLOR` and `TERM=dumb` disable color while
text labels continue to identify every status.

The TUI redraws for visible changes and coalesces stream updates to no more than
one frame every 34 ms. It processes at most 64 queued turn messages per event
loop cycle. A due repaint may wait one extra batch after a full batch, then draws
even if the next batch is full, so sustained streams cannot suppress redraws.
Elapsed-time redraws run only while a turn is active. Rendered transcript rows
are cached by turn revision, terminal width, and color policy, so unchanged turns
are not reparsed or wrapped again. Deterministic tests track draw and message
counts, rebuilt turns, and generated rows, including a simulated idle interval
and dual-session stream. Run them with:

```sh
cargo test --locked --manifest-path companions/Cargo.toml -p leg-tui
```

The conversation uses a two-row header, a one-row footer, and a composer that
grows from one to four content rows as the draft wraps. The remaining height
goes to the conversation. The session rail is hidden below 105 columns; at
wider sizes F8 toggles it, while F3 always opens the full session picker. The
inspector docks only when it leaves at least 60 columns for the conversation
and 36 for details; otherwise it opens as an Esc-dismissible overlay. Long
header values are elided to fit, and the session picker retains complete
titles and workspace paths plus the current model when known.

Enter inserts a newline; Ctrl-S sends a nonblank prompt. Left/Right move by
grapheme, Home/End move within the current line, and Backspace/Delete remove a
grapheme. Ctrl-Z undoes and Ctrl-Y redoes; a bracketed paste is one edit,
preserves Unicode and line breaks, normalizes CRLF/CR to LF, and discards other
control characters. A literal `?` is prompt text. F1 opens the full keyboard
guide. F2 and Ctrl-P open the same searchable command palette; type to filter
action labels case-insensitively, use Up/Down to select, Enter to invoke an
enabled action, and Esc to return to the unchanged composer draft and cursor.
Palette typing and paste stay out of the prompt. Ctrl-C first stops the viewed
active turn. If the viewed session is idle while background turns are active,
Ctrl-C opens a named Stop chooser; Escape cancels, and Enter stops only the
selected run while it remains active. If no TUI turns are active, Ctrl-C saves
drafts and exits. An active turn owned by another interface disables Exit so it
cannot be abandoned silently. External SIGINT and SIGTERM
use the shared turn controller to stop an active turn, wait for process
cleanup, save the draft, and restore the terminal. Editing stays available
during a turn, but another Ctrl-S is rejected while busy.

F3 opens the session picker. It shows each title, workspace, recent activity,
turn count, and state. Use `/` to filter titles, Up/Down to select, Enter to
reopen, N to create, R to rename, W to replace a missing workspace, and S to
search. Busy, recovered, read-only, missing-workspace, and unverified ownership
states are labeled in the picker and have an actionable status when opened.
Reopening only reads history; it does not send a request. Each open session keeps
its own composer draft and scroll position. An active turn continues in the
background while another session is viewed. The footer and picker name each
background session and show its text state. Returning to it does not submit the
prompt again.

Ctrl-F searches session titles and the complete sanitized transcript source,
including assistant text and full tool inputs/results outside the viewport or
compact tool row. Up/Down moves between matches. Enter opens the matching session
at the source row; a tool input/output match also opens that exact call's details.
Ctrl-U clears the query, and Esc closes search. Ctrl-Up/Down focuses the previous
or next tool row without editing the composer. F4 opens that call's inspector;
Escape closes it and returns to the same focused row. With no focused tool row,
F4 keeps its per-turn inspector behavior. Inspector Up/Down selects fields; `[` and
`]` select a neighboring turn. Long inspector fields can be read with
Shift-PageUp/Down.

Each tool call occupies one elided row at the terminal width, with its text state.
Rows show the path for `read`, `write`, and `edit`, with `read` offset/limit when
supplied. Bash rows show its description or command; unknown tools show their
name and first string input when available. Call IDs stay in details, where they
pair same-name calls with their own results. Pending, running, completed, failed,
denied, interrupted, and missing-result states stay distinct. A completed bash
command with a nonzero decoded exit code shows `exit N`; a timeout is labeled as
timed out.

The inspector shows the complete sanitized input. Bash JSON result envelopes,
including the core's JSON-string form, show stdout and stderr separately with
their exit code, status, and supplied omitted-byte counts. The full literal bash
result remains available as its own field; malformed or unknown envelopes fall
back to literal output. Read output stays literal. Supplied edit diffs receive
addition, removal, and context styling and retain their truncation marker. No
diff is created for write or missing patch data. All details use the same
terminal-control sanitation as the transcript. A failed, interrupted, or
incomplete turn is never replayed by viewing it.

F5 requests a terminal OSC 52 copy of the selected inspector field, including
readable bash output fields or the full literal input/result; copying is explicit
and never executes the text. If the terminal blocks clipboard access, F7 saves
the selected field to a file. F6 exports transcript data only, without catalog
metadata or inherited keys, and asks before replacing an existing file.
The command palette lists session browsing/switching, new conversation, rename,
workspace selection/replacement, search, inspect, copy/save, export, retry,
rail visibility, Stop, help, and Exit. Unavailable actions remain visible with
their reason. State is refreshed when an action is invoked, so a completed run
or a new owner cannot make a stale Stop or retry act on a different operation.
The footer hints at actions enabled for the viewed session; status and named
background activity keep priority. F1 remains the complete keyboard guide.
Ctrl-R in the inspector and palette retry offer an explicit retry only for the
latest failed, interrupted, or incomplete turn. Confirm with Y after reading
`Retry sends this prompt again and may repeat tool side effects.` Press N or Esc
to cancel; Enter does not retry.

User prompts, assistant messages, tool placeholders, outcomes, errors, and
warnings have separate labels and spacing, including when color is disabled.
Assistant headings, lists, inline code, and fenced code receive lightweight
Markdown styling. Code content and indentation stay literal. Unsupported or
malformed inline syntax stays readable as source, and an unfinished streamed
fence shows its content as code. Inspector copy/save and transcript export keep
the sanitized source and original line breaks; Markdown formatting and visual
wrapping are only for the reader.

The two-row conversation header shows the viewed session title, workspace,
model, turn state, elapsed time, and active tool when present. It shows Idle,
Starting, Running, Stopping, Succeeded, Failed, Interrupted, Incomplete, Capped,
or `Succeeded (truncated)` as text, including when color is disabled. A missing
terminal outcome is incomplete; forced cleanup is labeled separately from a
graceful interruption. Streamed text is grouped by round and block, then
reconciled with the authoritative terminal response so earlier tool rounds stay visible once.
Tool summaries are bounded to one row, while full output remains navigable in the
inspector. The TUI removes whole ANSI, CSI, and OSC sequences, including
sequences split across stream deltas, from provider text, tool data, errors, and
stderr.

PageUp and PageDown move by visible transcript rows; Ctrl-End follows the newest
text. The viewport anchors each row to its source message/block and byte offset,
so new stream chunks and response reconciliation keep the same reading position
and show a new-content indication. Resize keeps that source position as wrapping
changes. Search jumps into the matching block, and long unbroken text wraps
without splitting graphemes. Sending clears the editor for a new draft. On
failure, interruption, or incomplete cleanup, the submitted prompt returns only
when the editor has not changed; a newer editable draft is kept. Ctrl-S on the
unchanged failed prompt opens an explicit retry confirmation with `Retry sends
this prompt again and may repeat tool side effects.` Press Y to retry or N/Esc to
cancel. Enter remains a newline and never retries.

Focused Rust rendering/navigation checks use a generated 10,000-line answer
with a long unbroken Chinese and emoji line. The native Linux/macOS PTY smoke
test uses the local fake provider and explicit binary paths. It covers 80x24 and
120x40 layouts, responsive rail and inspector, resize recovery, signal cleanup,
non-TTY startup, color-disabled output, session creation/rename/reopen after
restart, palette filtering/cancellation/availability and stale-action races,
enabled-action footer hints, command-palette captures at 80x24 and 120x40,
history search and inspection, cross-interface busy rejection, named
background Stop selection, draft and scroll retention, copy/export
confirmation, a 1,000-turn history, composer and active-turn workflows, and the
delayed Stop regression. The history test records open and inspector response
times against the 200 ms target on the reference machine; catalog loading is
measured separately. It uses the pinned Python VT parser in
`companions/leg-tui/tests/requirements.txt`; install that test-only dependency
in a virtual environment before running it:

The focused `--tool-summary-only` PTY check covers same-name call pairing,
decoded and nonzero bash results, truncation and literal fallbacks, edit diff,
denial, missing results, unknown tools, search, copy/save, and the Escape return
to a focused row. Its 80x24 and 120x40 views are recorded in
`companions/leg-tui/tests/captures/tool-summary-80x24.txt` and
`companions/leg-tui/tests/captures/tool-summary-120x40.txt`.

Command-palette captures are checked in at
[80x24](../companions/leg-tui/tests/captures/command-palette-80x24.txt) and
[120x40](../companions/leg-tui/tests/captures/command-palette-120x40.txt).

Representative fixture captures are checked in at
[80x24](../companions/leg-tui/tests/captures/workbench-80x24.txt) and
[120x40](../companions/leg-tui/tests/captures/workbench-120x40.txt).

```sh
python3 -m venv /tmp/leg-pty-test-venv
/tmp/leg-pty-test-venv/bin/python -m pip install --requirement companions/leg-tui/tests/requirements.txt
cargo build --locked --bin leg
cargo build --locked --manifest-path companions/Cargo.toml -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path companions/Cargo.toml -p leg-tui
cargo build --locked --manifest-path companions/Cargo.toml -p leg-web
/tmp/leg-pty-test-venv/bin/python companions/leg-tui/tests/pty_smoke.py \
  --tui-bin companions/target/debug/leg-tui \
  --leg-bin target/debug/leg \
  --supervisor-bin companions/target/debug/leg-ui-supervisor \
  --web-bin companions/target/debug/leg-web
```
