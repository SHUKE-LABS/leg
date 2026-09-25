# leg architecture

`leg` is the standalone CLI client split out of
[`SHUKE-LABS/baton`](https://github.com/SHUKE-LABS/baton). It is a thin,
single-binary front end over one of baton's participant paths — the
**local path** (`LocalParticipant` + `Transport`), where one reply is one
user turn of Claude Messages-API calls. `leg ask`, `leg session` (with `--resume`), and
`leg log show`/`log replay` cover one-shot prompts, interactive multi-turn
REPL sessions, and JSONL trail inspection/replay respectively; `leg exchange`
covers the external-agent integration surface (see baton's
`docs/external-agent.md`).

Messages are ordered content blocks — `text`, `tool_use`, and
`tool_result` — so a reply's tool calls, and the tool results sent back on a
follow-up turn, round-trip through the transport, the JSONL trail, and
`leg session --resume`; each reply carries a structured `stop_reason`.
Text-only traffic stays byte-identical to the plain-string format. A tool
loop (`src/tools.rs`) wraps the transport: while a reply's `stop_reason` is
`tool_use`, it runs each call through a registry of synchronous handlers and
sends the `tool_result` blocks back, stopping after 10 tool-use rounds per
user turn. The registered tools are `read`, `write`, `edit`, and `bash`.
`read` and `write` share an in-process read-set. The `bash` tool
(`src/tools/bash.rs`) runs one `bash -lc <command>` synchronously in the
caller's working directory and OS identity. Its timeout defaults to 120 seconds
and can be configured with the positive-integer `LEG_BASH_TIMEOUT_SECS`
environment variable; an explicit per-call timeout overrides that default.
Timeout results retain partial stdout/stderr, report exit code 124, and
terminate the shell and descendants after a 50 ms graceful period (Windows
uses a job object). It stops draining inherited output pipes after a 2-second
guard. Each output stream is capped at 2000 lines or 50 KB with head/tail
truncation and an omitted-byte count. The JSON tool result includes wall time,
status, exit code, and separate stdout/stderr fields. The tool loop persists
each dispatched round on the trail
as a `tool_round` line (the `tool_use` reply's blocks), then per
call a `tool_call` line and one matching `tool_result` line — enough for
`--resume` to rebuild the turn's history verbatim.

`leg` owns none of the A2A envelope, multi-participant orchestration,
mailbox, or session-supervision machinery — that is baton's job. For the
full harness model (participant paths, the `baton.message/v1` envelope, the
module layout), see baton's
[`docs/architecture.md`](https://github.com/SHUKE-LABS/baton/blob/main/docs/architecture.md).
