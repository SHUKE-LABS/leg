# leg architecture

`leg` is the standalone CLI client split out of
[`SHUKE-LABS/baton`](https://github.com/SHUKE-LABS/baton). It is a thin,
single-binary front end over one of baton's participant paths — the
**local path** (`LocalParticipant` + `Transport`), where one reply is one
Claude Messages-API exchange. `leg ask`, `leg session` (with `--resume`), and
`leg log show`/`log replay` cover one-shot prompts, interactive multi-turn
REPL sessions, and JSONL trail inspection/replay respectively; `leg exchange`
covers the external-agent integration surface (see baton's
`docs/external-agent.md`).

Messages are ordered content blocks — `text`, `tool_use`, and
`tool_result` — so a reply's tool calls, and the tool results sent back on a
follow-up turn, round-trip through the transport, the JSONL trail, and
`leg session --resume`; each reply carries a structured `stop_reason`.
Text-only traffic stays byte-identical to the plain-string format. `leg`
does not execute tools or loop on `stop_reason: tool_use`.

`leg` owns none of the A2A envelope, multi-participant orchestration,
mailbox, or session-supervision machinery — that is baton's job. For the
full harness model (participant paths, the `baton.message/v1` envelope, the
module layout), see baton's
[`docs/architecture.md`](https://github.com/SHUKE-LABS/baton/blob/main/docs/architecture.md).
