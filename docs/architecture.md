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

`leg` owns none of the A2A envelope, multi-participant orchestration,
mailbox, or session-supervision machinery — that is baton's job. For the
full harness model (participant paths, the `baton.message/v1` envelope, the
module layout), see baton's
[`docs/architecture.md`](https://github.com/SHUKE-LABS/baton/blob/main/docs/architecture.md).
