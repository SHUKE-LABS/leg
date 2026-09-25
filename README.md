# leg

`leg` binary skeleton.

## Quickstart

```
export ANTHROPIC_API_KEY=sk-...

leg ask "hello"

LEG_EVENT_LOG=trail.jsonl leg session
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

or build from source directly:

```
cargo install --git https://github.com/SHUKE-LABS/leg --locked
```

Per-platform archives for every CI-supported target (`.tar.gz` on
Unix, `.zip` on Windows) are attached to each
[GitHub Release](https://github.com/SHUKE-LABS/leg/releases).

Every published npm package includes `THIRD_PARTY_NOTICES.txt` with the
licenses of the bundled third-party Rust crates and vendored material.

## Usage

```
ANTHROPIC_API_KEY=sk-... leg ask [--model <model>] "prompt"
```

Prints the assistant reply on success. A provider or delivery failure
(bad credentials, unreachable base URL, etc.) leaves stdout empty, reports an
error including `kind: error` on stderr, and exits non-zero. Configuration
failures (missing/malformed env vars) also exit non-zero.
Also accepts `ANTHROPIC_AUTH_TOKEN`/`CLAUDE_CODE_OAUTH_TOKEN`,
`ANTHROPIC_BASE_URL`, `LEG_MODEL`, `LEG_TIMEOUT_SECS`, `LEG_MAX_TOKENS`,
`LEG_SYSTEM_PROMPT`, and `LEG_EVENT_LOG`.

### Tool loop

`ask`, `session`, and `exchange` share one tool loop. It runs only when a
reply requests tools (`stop_reason: tool_use`): each call is executed and
its result sent back until the model answers, and only that final reply is
printed. A user turn stops after 10 tool-use rounds; if the reply still
requests tools, `leg` sends no further request and warns on stderr. A call
to an unregistered tool is answered with an error result.

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
  seconds, defaulting to 10. It runs synchronously; a timeout reports status
  `timed_out` and exit code `124`, retains output captured before termination,
  and terminates the shell and descendants (50 ms graceful period, then force
  termination; Windows uses a job object). It stops draining inherited output
  pipes after a 2-second guard. Each output stream is captured separately and
  capped at 2000 lines or 50 KB, whichever comes first, keeping its head and
  tail and showing the omitted-byte count. The tool result is JSON with
  `wall_time_seconds`, `status` (`exited` or `timed_out`), `exit_code`, `stdout`,
  `stderr`, `stdout_omitted_bytes`, and `stderr_omitted_bytes`. A non-zero
  command exit is returned as `exit_code`; a missing `bash` executable is a
  tool error.

#### Headless contract for agent callers

A caller driving `leg ask` or `leg exchange` sets the working directory:
every tool resolves relative paths against, and `bash` runs in, the process
cwd. The whole tool loop runs inside the one invocation — tool calls and
results are never written to stdout. On success, `ask` prints only the final
reply and `exchange` writes one response. On provider/delivery failure, both
exit non-zero: `ask` and plain-text `exchange` leave stdout empty and report
an error on stderr; envelope `exchange` writes its `kind:"error"` response
before reporting the failure. To observe tool steps, set `LEG_EVENT_LOG` on
`ask` and read the trail back with `leg log show`. The `bash` tool needs
`bash` on `PATH` (Git Bash on Windows). `tests/headless_e2e.rs` exercises
this contract end to end against a fake provider.

### Sessions

```
LEG_EVENT_LOG=trail.jsonl ANTHROPIC_API_KEY=sk-... leg session
```

Runs an interactive multi-turn REPL: each line typed is sent with the full
prior conversation, and the reply is printed. Ctrl-D or a lone `/exit` line
ends the session cleanly. Every turn (and, with `LEG_EVENT_LOG` set, the
session's start/end) is appended to the JSONL trail, keyed by a `session_id`
minted for the run.

```
leg session --resume trail.jsonl [--session <id>]
```

Reopens a prior session's trail, rehydrates its conversation history —
including each turn's tool rounds — and continues appending new turns to the
same file. `--session <id>` selects
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
line with the same `tool_use_id`, a `status` of `completed` or `failed`, and
the tool's `result` or `error`. All three carry `schema` and `ts_ms`, plus
`session_id`/`turn_index` on session turns. `leg exchange` writes no trail.

### Exchange

```
leg exchange [--in <path>] [--out <path>]
```

Answers one `baton.message/v1` request (from `--in`, or stdin) with exactly
one response (to `--out`, or stdout); a plain-text request gets the reply
body alone. The tool loop runs inside that single exchange. `leg exchange`
is the headless entry point for adapters.

## CI-supported targets

- x86_64-unknown-linux-gnu
- aarch64-unknown-linux-gnu
- x86_64-apple-darwin
- aarch64-apple-darwin
- x86_64-pc-windows-msvc
- armv7-unknown-linux-musleabihf (cross-compiled; build-only, no native runner)
