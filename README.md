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

From a checkout, run `bash scripts/dev.sh` to update and install `leg` and its UI
companions.

Per-platform archives for every CI-supported target (`.tar.gz` on
Unix, `.zip` on Windows) are attached to each
[GitHub Release](https://github.com/SHUKE-LABS/leg/releases).

Releases are created automatically when changes land on `main`. A
Conventional Commit subject of type `feat` bumps the minor version; every
other subject bumps patch. The first release uses the current Cargo version
(`0.1.0`) unchanged.

Every published npm package includes `THIRD_PARTY_NOTICES.txt` with the
licenses of the bundled third-party Rust crates and vendored material.

## Experimental TUI trial

The separate terminal interface is experimental and is not part of the regular
`leg` installation or npm package. Download a `leg-tui-experimental-*` bundle
from a Linux or macOS CI run, or build one with the command in the
[TUI trial quickstart](companions/leg-tui/trial/QUICKSTART.md). Run it with a
disposable workspace and the local fixture; stop the TUI and remove the
unpacked bundle directory when finished. This trial does not change how the
regular `leg` binary is installed.

## Experimental Web trial

The separate Web companion is experimental and is not part of the regular
`leg` installation. Get the Linux or macOS bundle from the CI run's
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
failures (missing/malformed env vars) also exit non-zero.
Also accepts `ANTHROPIC_AUTH_TOKEN` for bearer keys of Anthropic-compatible
endpoints, and `CLAUDE_CODE_OAUTH_TOKEN`. Claude subscription OAuth tokens
(including `claude setup-token`) are unsupported outside Claude Code; use an
Anthropic Console API key with `ANTHROPIC_API_KEY` instead. Other settings
include `ANTHROPIC_BASE_URL`, `LEG_MODEL`, `LEG_TIMEOUT_SECS`,
`LEG_STREAM_IDLE_TIMEOUT_SECS`, `LEG_BASH_TIMEOUT_SECS`, `LEG_MAX_TOKENS`,
`LEG_MAX_TOOL_ROUNDS`, `LEG_MAX_RETRIES`, `LEG_RETRY_BASE_DELAY_MS`,
`LEG_PRETOOL_HOOK`, `LEG_SYSTEM_PROMPT`, and `LEG_EVENT_LOG`.

`LEG_TIMEOUT_SECS` defaults to 60 and bounds DNS lookup, connection setup,
request sending, response headers, and the total body read for non-streaming
responses. Successful streamed responses may run longer while data keeps
arriving; `LEG_STREAM_IDLE_TIMEOUT_SECS` defaults to 120 and limits how long a
stream may remain silent between received chunks.

`LEG_MAX_TOKENS` sets the output-token limit requested for each provider reply
(default `32000`).

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

`LEG_PRETOOL_HOOK` optionally names an executable to run before every tool
dispatch; unset or blank leaves dispatch unchanged. The hook receives this
JSON object on stdin:
`{"hook_event_name":"PreToolUse","tool_name":"<name>","tool_input":{...},"cwd":"<cwd>"}`.
Exit 0 with empty or whitespace-only stdout, or `{"decision":"allow"}`,
allows the call. Exit 0 with
`{"decision":"deny","reason":"..."}` denies it and returns that reason to the
model. A non-zero exit, spawn failure, 30-second timeout, or invalid output
denies with a generic reason. Hook denials are recorded in the trail with
`status: "denied"`.

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
LEG_EVENT_LOG=trail.jsonl ANTHROPIC_API_KEY=sk-... leg session
```

Runs an interactive multi-turn REPL: each line typed is sent with the full
prior conversation, and assistant text is flushed as it streams. Ctrl-D or a
lone `/exit` line ends the session cleanly. Every turn (and, with
`LEG_EVENT_LOG` set, the session's start/end) is appended to the JSONL trail,
keyed by a `session_id` minted for the run.

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

- x86_64-unknown-linux-gnu
- aarch64-unknown-linux-gnu
- x86_64-apple-darwin
- aarch64-apple-darwin
- x86_64-pc-windows-msvc
- armv7-unknown-linux-musleabihf (cross-compiled; build-only, no native runner)
