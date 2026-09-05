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

## Usage

```
ANTHROPIC_API_KEY=sk-... leg ask [--model <model>] "prompt"
```

Prints the assistant reply on success. A provider or delivery failure
(bad credentials, unreachable base URL, etc.) prints a `baton.message/v1`
envelope with `"kind":"error"` instead of exiting non-zero — only a
configuration failure (missing/malformed env vars) exits non-zero.
Also accepts `ANTHROPIC_AUTH_TOKEN`/`CLAUDE_CODE_OAUTH_TOKEN`,
`ANTHROPIC_BASE_URL`, `LEG_MODEL`, `LEG_TIMEOUT_SECS`, `LEG_MAX_TOKENS`,
`LEG_SYSTEM_PROMPT`, and `LEG_EVENT_LOG`.

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

Reopens a prior session's trail, rehydrates its conversation history, and
continues appending new turns to the same file. `--session <id>` selects
which session to resume when the trail holds more than one; it is required
in that case and otherwise optional.

### Log replay

```
leg log show [--file <path>]
leg log replay [--file <path>] [--index <N>]
```

`log show` prints every complete exchange in a JSONL trail (`--file`, or
`LEG_EVENT_LOG` when omitted). `log replay` re-runs one logged exchange's
prompt — the last one, or `--index <N>` (1-based) — against the *current*
environment's credential, model, and base URL taken from the log entry;
timeout, max tokens, and system prompt still come from today's environment.
The replay's own request and outcome are appended to `LEG_EVENT_LOG` like
any other `ask`.

## CI-supported targets

- x86_64-unknown-linux-gnu
- aarch64-unknown-linux-gnu
- x86_64-apple-darwin
- aarch64-apple-darwin
- x86_64-pc-windows-msvc
- armv7-unknown-linux-musleabihf (cross-compiled; build-only, no native runner)
