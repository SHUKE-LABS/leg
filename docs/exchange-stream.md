# `leg.exchange.stream/v1`

`leg exchange --stream-json` opts into an NDJSON stream on stdout. It accepts
the same plain-text or `baton.message/v1` input as `leg exchange`; `--in`,
`--session`, and `--new-session` keep their existing meanings. The
`--new-session-id <id>` option creates a session using a caller-reserved native
`sess-...` ID and refuses to overwrite an existing trail. The stream is
the complete stdout protocol for that invocation, so `--out` cannot be used.
`--session-id-out` remains valid with any session selection and still writes
the id plus a newline after the turn.

Without `--stream-json`, `leg exchange` keeps its existing one-response stdout
contract. Streaming uses the same provider, tool loop, session history, and
correlated response envelope.

## Records

Each line is one JSON object. Every record contains:

- `schema`: `leg.exchange.stream/v1`
- `event`: one of the events below
- `seq`: a zero-based, strictly increasing sequence number, scoped to one
  invocation

The event-specific fields are:

| Event | Fields |
| --- | --- |
| `turn_start` | `request`: full correlated `baton.message/v1` request envelope; `provider`; resolved `model`; optional `session_id` and `turn_index` |
| `text_delta` | `round_index`, `block_index`, and the UTF-8 `text` fragment |
| `tool_round` | `round_index` and the ordered `content` blocks from the provider's tool-use reply |
| `tool_call` | `round_index`, `tool_use_id`, `tool_name`, and JSON `input` |
| `tool_result` | `round_index`, `tool_use_id`, `tool_name`, `status`, and `output` |
| `turn_end` | Full correlated `response` envelope, explicit boolean `capped`, and optional `session_id` and `turn_index` |

`provider` is `anthropic`, `openai-chat-completions`, or `openai-responses`.
Session coordinates are omitted on cold exchanges. A new session's id appears
in `turn_start`, even when the first provider turn fails.

`round_index` counts provider calls in this user turn, starting at zero. A
`tool_round` identifies the provider call that requested the tools; its
`tool_call` and `tool_result` records use that same index. `block_index` is the
zero-based content-block index assigned by the provider stream assembler.
Multiple text blocks keep their separate indices. Text is emitted from stream
deltas only, never again from the final response. A buffered transport can
therefore produce a start and terminal record without any `text_delta`.

Tool records preserve provider block order and call ids. `tool_round.content`
contains the complete ordered block list, including text and tool calls.
`tool_call` is written before dispatch. Its matching `tool_result` follows
dispatch and reports `completed`, `failed`, or `denied`, including an
interrupted active tool as `failed`.

## Ordering and completion

`turn_start` is flushed before the first provider call. Text and tool records
are flushed as they are observed. Normally completed exchanges emit exactly
one `turn_start` and one `turn_end`; provider failures and capped turns also
emit `turn_end`. The terminal response follows session outcome recording. If a
required session-trail write fails, the terminal response reports an error
instead of advertising the provider reply as committed.

`turn_end.response` is authoritative. Deltas are provisional, including text
emitted before a provider stream later fails. Consumers must treat EOF without
`turn_end` as an incomplete exchange, never as success. A successful terminal
record is the final record; no records follow it.

`capped` is true only when `LEG_MAX_TOOL_ROUNDS` ends the turn while the
provider still requests tools. In that case the final response envelope
contains the capped reply and does not claim the unanswered tool calls ran.

Failures before `turn_start` keep the existing stderr/nonzero behavior and
write no stream records. A provider failure after start ends with a correlated
error envelope. On Unix, signal handling and process-tree cleanup keep their
existing behavior; when stdout remains writable, leg makes a best-effort
attempt to emit an `interrupted` terminal envelope. A broken pipe stops further
tool dispatch and exits nonzero. Signal or pipe truncation may leave EOF
without a terminal record.

Unknown optional fields and event names may be ignored by consumers of this
version. Every record shown here is parsable NDJSON:

```jsonl
{"schema":"leg.exchange.stream/v1","event":"turn_start","seq":0,"provider":"anthropic","model":"claude-test","request":{"schema":"baton.message/v1","message_id":"m-1","conversation_id":"c-1","from":"external","to":"leg","in_reply_to":null,"kind":"request","body":"Inspect notes.txt","ts_ms":1,"exchange":null}}
{"schema":"leg.exchange.stream/v1","event":"text_delta","seq":1,"round_index":0,"block_index":0,"text":"I will inspect it. "}
{"schema":"leg.exchange.stream/v1","event":"tool_round","seq":2,"round_index":0,"content":[{"type":"text","text":"I will inspect it. "},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"notes.txt"}}]}
{"schema":"leg.exchange.stream/v1","event":"tool_call","seq":3,"round_index":0,"tool_use_id":"toolu_1","tool_name":"read","input":{"path":"notes.txt"}}
{"schema":"leg.exchange.stream/v1","event":"tool_result","seq":4,"round_index":0,"tool_use_id":"toolu_1","tool_name":"read","status":"completed","output":"notes loaded"}
{"schema":"leg.exchange.stream/v1","event":"text_delta","seq":5,"round_index":1,"block_index":0,"text":"The note says hello."}
{"schema":"leg.exchange.stream/v1","event":"turn_end","seq":6,"capped":false,"response":{"schema":"baton.message/v1","message_id":"c-1-r-2-0","conversation_id":"c-1","from":"leg","to":"external","in_reply_to":"m-1","kind":"response","body":"The note says hello.","ts_ms":2,"exchange":{"schema":"baton.exchange/v1","exchange":{"request":{"ts_ms":1,"model":"claude-test","base_url":"https://api.anthropic.com","prompt":"Inspect notes.txt"},"outcome":{"event":"response_ok","ts_ms":2,"duration_ms":1,"reply":"The note says hello.","input_tokens":1,"output_tokens":1,"stop_reason":"end_turn","attempts":2}}}}}
```
