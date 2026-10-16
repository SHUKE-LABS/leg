# Hooks v1 configuration and runtime

Leg loads one hooks v1 file and runs its selected commands at tool, turn, and
interactive-session boundaries. Hook commands are executable paths; leg does
not invoke a shell or parse command arguments. Relative paths resolve from the
selected configuration file's directory.

## Configuration format

Configuration is a UTF-8 JSON object. `version` must be `1`, and `hooks` is an
array whose order is significant:

```json
{
  "version": 1,
  "hooks": [
    {
      "id": "before-shell",
      "event": "PreToolUse",
      "matcher": "bash",
      "command": "hooks/check-policy",
      "enabled": true
    }
  ]
}
```

Each entry requires `id`, `event`, and `command`. `matcher` is optional;
`enabled` is optional and defaults to `true`. The path is not executed or
checked for existence while configuration is loaded.

## Discovery and precedence

Leg selects the first available source and does not merge files:

1. A nonblank `LEG_HOOKS_CONFIG` value names an explicit file path.
2. `<current working directory>/.leg/hooks.json` is the project file. Discovery
   is relative to the process's current directory; it does not walk upward to
   the Git root.
3. The per-user file is `%APPDATA%\leg\hooks.json` on Windows. On other
   platforms it is `$XDG_CONFIG_HOME/leg/hooks.json`, or
   `$HOME/.config/leg/hooks.json` when `XDG_CONFIG_HOME` is unset or blank.

A missing implicit project file falls through to the user file. On Windows, an
unset or blank `APPDATA` skips the user source. Elsewhere, if both
`XDG_CONFIG_HOME` and `HOME` are unset or blank, the user source is skipped. If
no source exists, the hook set is empty. An existing project file, including
one with an empty `hooks` array, suppresses the user file. An explicit
environment path is authoritative: a missing or unreadable file is an error
and does not fall through. A selected but unreadable or invalid file is also
an error and does not fall through.

`LegConfig::from_env` loads the selected file once when configuration is
created. A configuration error fails commands that construct `LegConfig`;
commands that do not construct it do not read hook configuration.

## Events and selection

`event` must be one of these exact names:

- `PreToolUse`
- `PostToolUse`
- `TurnStart`
- `TurnEnd`
- `SessionStart`
- `SessionEnd`

Event matching is exact and case-sensitive. `matcher` is allowed only for
`PreToolUse` and `PostToolUse`. It is either `*` to match every tool, or an
exact case-sensitive tool name containing only ASCII letters, digits, `_`,
`-`, `.`, or `:`. An omitted matcher also matches every tool. Matchers are not
regular expressions; alternation and other pattern syntax are invalid. An
empty matcher or a matcher on a non-tool event is invalid.

For an event and tool, enabled matching entries run sequentially in JSON array
order. The legacy `LEG_PRETOOL_HOOK`, when configured, runs before declarative
`PreToolUse` entries. A veto or fail-closed `PreToolUse` error stops the
remaining entries. Failures from other events do not stop later hooks.

## Payloads

Each command receives one UTF-8 JSON object on stdin. Every payload includes
`hook_event_name` and `cwd`. `session_id` and `turn_index` appear when the hook
runs within a session turn. Tool hooks receive the tool's original input. The
payload fields are:

| Event | Event-specific fields | Boundary |
| --- | --- | --- |
| `PreToolUse` | `tool_name`, `tool_input` | Before dispatching a tool handler. |
| `PostToolUse` | `tool_name`, `tool_input`, `tool_output`, `tool_status` | After a non-denied tool result, including handler errors and unknown tools. It does not run after a `PreToolUse` veto. |
| `TurnStart` | `prompt` | Before the provider work for each `ask`, `exchange`, or interactive session turn. `prompt` concatenates the turn's text blocks; image content is omitted. |
| `TurnEnd` | `prompt`, `turn_status`, and either `reply` or `error` | After the tool loop returns for the turn. `turn_status` is `completed` or `failed`. |
| `SessionStart` | none | When a fresh interactive `leg session` starts. |
| `SessionEnd` | `turns` | On a clean exit from interactive `leg session`; `turns` is the session's total turn count. |

Tool status values are `completed` and `failed` for `PostToolUse`; a denied
call does not produce that event. Interactive session resume keeps the
existing boundary behavior: it does not run `SessionStart` again, and a clean
exit still runs `SessionEnd`. Interrupted sessions do not run `SessionEnd`.
Named `leg exchange` session turns receive `session_id` and `turn_index` on
their turn and tool events; SessionStart and SessionEnd hooks belong to the
interactive `leg session` boundaries.

For example, a session-scoped `PostToolUse` payload has this shape:

```json
{
  "hook_event_name": "PostToolUse",
  "cwd": "/workspace/project",
  "session_id": "sess-123",
  "turn_index": 2,
  "tool_name": "bash",
  "tool_input": { "command": "echo hi" },
  "tool_output": "hi",
  "tool_status": "completed"
}
```

`PreToolUse` keeps the Claude-compatible decision envelope. Exit 0 with empty
or whitespace-only stdout, or `{"decision":"allow"}`, allows the tool.
Exit 0 with `{"decision":"deny","reason":"..."}` denies it and returns
that reason in the tool result. A nonzero exit, spawn failure, malformed,
truncated, or invalid UTF-8 output, timeout, or cancellation fails closed with
the generic `denied by pre-tool hook: hook failed` result. A denial prevents
the handler and remaining `PreToolUse` hooks from running. The denied result
is surfaced with `status: "denied"` in the existing tool-result event stream.

All events use the existing fixed 30-second `HOOK_TIMEOUT` from
`src/tools/pretool.rs`; there is no per-hook timeout setting. The process
runner in `src/tools/process.rs` checks `interrupt::is_signaled()` to detect
cancellation, then terminates and reaps the child before returning.

`PostToolUse`, `TurnStart`, `TurnEnd`, `SessionStart`, and `SessionEnd` are
observational. Their stdout does not alter the operation. A spawn error,
nonzero exit, timeout, or cancellation produces a structured hook outcome and
one stderr warning per failure from the shared reporter; subsequent hooks
still run, and the tool, turn, or session result is unchanged.

## IDs, validation, and disabled entries

IDs are case-sensitive and must match
`[A-Za-z0-9][A-Za-z0-9._-]{0,127}`. IDs must be unique within the selected
file. Duplicate IDs are fatal, including when one entry is disabled.

The whole selected file fails to load if its JSON is malformed, its version is
not `1`, it contains unknown fields, an entry is invalid, or a command is
missing or empty. Leg never partially loads a file or silently chooses a
different hook after an error. A valid `enabled: false` entry is validated,
then omitted from selection.

## `LEG_PRETOOL_HOOK` compatibility and trust

`LEG_PRETOOL_HOOK` remains an optional executable path. It uses the
`PreToolUse` payload and decision behavior above. When both forms are
configured, the legacy command runs first, followed by matching declarative
entries in file order.

Hooks named by a project `.leg/hooks.json` are trusted as-is. Commands run with
the operating-system permissions of the user running leg; a project file can
therefore name commands that act with those permissions.

The event names are compatible with Claude Code for tool and session hooks;
`TurnStart` and `TurnEnd` are leg-specific. Leg uses a flat one-event-per-entry
list with stable IDs, explicit file order, and a versioned schema. Its tool
matchers are exact names or `*`, not Claude Code regular expressions.
References: [Claude Code hooks](https://code.claude.com/docs/en/hooks) and
[OpenCode plugins](https://opencode.ai/docs/plugins/).
