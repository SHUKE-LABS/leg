# Hooks v1 configuration contract

This document defines the v1 configuration and selection contract for leg
hooks. It covers configuration loading and selection; lifecycle dispatch for
declarative hooks is a separate slice.

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
`enabled` is optional and defaults to `true`. `command` names one executable
path, with no shell or argument parsing. Relative command paths are resolved
from the selected configuration file's directory. The path is not executed or
checked for existence while configuration is loaded.

`PreToolUse`, `PostToolUse`, `SessionStart`, and `SessionEnd` use Claude Code
command-hook event names; `TurnStart` and `TurnEnd` are leg-specific. Claude
Code command hooks use regular-expression matchers in event-keyed groups. Leg
deliberately uses a flat one-event-per-entry list with stable IDs, explicit
file order, and a versioned top-level schema, and limits matchers to exact tool
names or `*`.

OpenCode's plugin documentation shows a different open-source design: named
tool events such as `tool.execute.before`/`tool.execute.after`, with project
plugins under `.opencode/plugins/` and global plugins under
`~/.config/opencode/plugins/`, loaded at startup. Leg adopts neither plugin
callbacks nor multiple loaded source layers; the comparison supports keeping
event names and selection rules explicit in the v1 contract. References:
[Claude Code hooks](https://code.claude.com/docs/en/hooks) and
[OpenCode plugins](https://opencode.ai/docs/plugins/).

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

## Events and matchers

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

## IDs, validation, and disabled entries

IDs are case-sensitive and must match
`[A-Za-z0-9][A-Za-z0-9._-]{0,127}`. IDs must be unique within the selected
file. Duplicate IDs are fatal, including when one entry is disabled.

The whole selected file fails to load if its JSON is malformed, its version is
not `1`, it contains unknown fields, an entry is invalid, or a command is
missing or empty. Leg never partially loads a file or silently chooses a
different hook after an error. A valid `enabled: false` entry is validated,
then omitted from selection.

For an event and tool, leg returns enabled matching entries in JSON array
order. There is only one selected file, so entries from different sources are
never combined.

## `LEG_PRETOOL_HOOK` compatibility

`LEG_PRETOOL_HOOK` remains an optional executable path. With no declarative
file it keeps its existing `PreToolUse` stdin and decision behavior. When both
forms are configured, selection treats the legacy hook as additive and places
it before matching declarative `PreToolUse` entries; declarative entries follow
file order. The existing legacy execution path is unchanged. This contract
slice loads and selects declarative entries but does not execute them.

Hooks named by a project `.leg/hooks.json` are trusted as-is. When hook
execution is provided, commands run with the operating-system permissions of
the user running leg; a project file can therefore name commands that act with
those permissions.
