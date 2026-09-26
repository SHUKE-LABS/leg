//! Structured exchange-record types shared by the peer-message envelope.
//!
//! [`ExchangeMeta`] is the replay-relevant metadata ([`model`](ExchangeMeta::model)
//! / [`base_url`](ExchangeMeta::base_url)) known before a provider call is made.
//! [`Exchange`] (paired with [`RequestRecord`]/[`Outcome`]) is the record of one
//! completed call — nested inside a [`crate::message::MessageEnvelope`] via
//! [`crate::message::WrappedExchange`] so a reply is observable in-band, in
//! memory, with no side trail.
//!
//! Baton's upstream module of the same name additionally owns a JSONL side
//! trail (`LEG_EVENT_LOG`, `BATON_EVENT_LOG` upstream) and its
//! `ExchangeEvent`/`EventSink` write path — ported here (leg#3), trimmed of
//! roles/A2A/mailbox framing, which are out of scope for `leg`. The read path
//! (parsing/formatting that trail back) lives in [`crate::log`], which
//! imports `Exchange`/`RequestRecord`/`Outcome` from here rather than
//! defining its own deserialize mirrors, since these types are already
//! `Deserialize`.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::model::{ContentBlock, as_single_text};
use crate::tools::ToolEvent;

/// Schema discriminator stamped on the nested exchange record.
pub const SCHEMA: &str = "baton.exchange/v1";

/// Replay-relevant metadata about an exchange, known before the call is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeMeta {
    /// Model id the request targets.
    pub model: String,
    /// Base URL the request is sent to.
    pub base_url: String,
}

/// One request paired with its single outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// The recorded request (carries everything needed to replay it).
    pub request: RequestRecord,
    /// The recorded terminal outcome (success reply or failure).
    pub outcome: Outcome,
}

/// The replay-relevant fields of a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Wall-clock emission time, Unix epoch milliseconds.
    pub ts_ms: u64,
    /// Model id the request targeted.
    pub model: String,
    /// Base URL the request was sent to.
    pub base_url: String,
    /// The user prompt text.
    pub prompt: String,
    /// The turn's content blocks when it is not a single text block (images,
    /// tool results, multiple blocks); absent for a text-only prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    /// Session this turn belongs to; absent on single-turn `ask`/`exchange`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Monotonic turn number within the session; absent on `ask`/`exchange`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<u64>,
}

/// The terminal outcome of an exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Outcome {
    /// The call succeeded.
    #[serde(rename = "response_ok")]
    Ok {
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Time spent in the provider call, milliseconds.
        duration_ms: u64,
        /// The assistant reply text.
        reply: String,
        /// The reply's content blocks when it is not a single text block
        /// (tool calls, multiple blocks); omitted for a text-only reply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        /// Provider-reported input (prompt) tokens; omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Provider-reported output (completion) tokens; omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Provider-reported terminal reason; omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Session this outcome belongs to; absent on `ask`/`exchange`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Monotonic turn number matching the session request; absent when
        /// `session_id` is absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// The call failed; `kind` is the stable machine class.
    #[serde(rename = "response_error")]
    Error {
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Time spent before the failure resolved, milliseconds.
        duration_ms: u64,
        /// Stable machine-readable error class (mirrors [`crate::error::LegError::kind`]).
        kind: String,
        /// Human-readable error description.
        message: String,
        /// Session this outcome belongs to; absent on `ask`/`exchange`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Monotonic turn number matching the session request; absent when
        /// `session_id` is absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
}

/// The trail's `content` field for `blocks`: `None` for a text-only turn (one
/// text block, fully carried by `prompt`/`reply`), else the blocks verbatim.
///
/// Omitting the field for text-only turns keeps those trail lines
/// byte-identical to the pre-block format that baton also reads.
pub fn trail_content(blocks: &[ContentBlock]) -> Option<Vec<ContentBlock>> {
    match as_single_text(blocks) {
        Some(_) => None,
        None => Some(blocks.to_vec()),
    }
}

/// Current wall-clock time as Unix epoch milliseconds.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis() as u64
}

/// A single lifecycle event for the JSONL exchange trail (`LEG_EVENT_LOG`, or
/// a `--resume` session file).
///
/// Serialized as JSONL: the `event` tag selects the kind and `schema` carries
/// [`SCHEMA`]. Trimmed from baton's upstream `ExchangeEvent` — no role/A2A/
/// mailbox framing, which are out of scope for `leg`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ExchangeEvent {
    /// Emitted before the provider call. Carries enough to replay the exchange.
    Request {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Model id the request targets.
        model: String,
        /// Base URL the request is sent to.
        base_url: String,
        /// The user prompt text.
        prompt: String,
        /// The turn's content blocks when it is not a single text block;
        /// omitted for a text-only prompt (see [`trail_content`]).
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        /// Session this turn belongs to, when emitted from `leg session`.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Monotonic turn number within the session, starting at 0.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// Emitted when the provider call succeeds.
    ResponseOk {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Time spent in the provider call, milliseconds.
        duration_ms: u64,
        /// The assistant reply text.
        reply: String,
        /// The reply's content blocks when it is not a single text block;
        /// omitted for a text-only reply (see [`trail_content`]).
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        /// Provider-reported input (prompt) tokens; omitted when unknown.
        #[serde(skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Provider-reported output (completion) tokens; omitted when unknown.
        #[serde(skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Provider-reported terminal reason; omitted when unknown.
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Session this outcome belongs to, when emitted for a session turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Monotonic turn number matching the session request.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// Emitted when the provider call fails.
    ResponseError {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Time spent before the failure resolved, milliseconds.
        duration_ms: u64,
        /// Stable machine-readable error class (see [`crate::error::LegError::kind`]).
        kind: String,
        /// Human-readable error description.
        message: String,
        /// Session this outcome belongs to, when emitted for a session turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Monotonic turn number matching the session request.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// Emitted once per dispatched tool round, before that round's
    /// `tool_call` lines: the `tool_use` reply's full content blocks, so a
    /// resume can rebuild the round (its text and call grouping) verbatim.
    ToolRound {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// The `tool_use` reply's content blocks, in reply order.
        content: Vec<ContentBlock>,
        /// Session this round belongs to, when emitted for a session turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Turn number of the session request this round runs within.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// Emitted just before the tool loop dispatches one `tool_use` call,
    /// between the turn's `request` and its outcome.
    ToolCall {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Provider-assigned call id, echoed by the matching `tool_result`.
        tool_use_id: String,
        /// The called tool's name.
        tool_name: String,
        /// The call's JSON arguments.
        input: serde_json::Value,
        /// Session this call belongs to, when emitted for a session turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Turn number of the session request this call runs within.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// Emitted exactly once per `tool_call`, when that call finishes.
    ToolResult {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// The `tool_call` this result answers.
        tool_use_id: String,
        /// The called tool's name.
        tool_name: String,
        /// Whether the call completed or failed; the only error signal.
        status: ToolStatus,
        /// The tool's output; present only when `completed`.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// The failure message; present only when `failed`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Session this result belongs to, when emitted for a session turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Turn number of the session request this call runs within.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// Emitted once by `leg session` at the start of a run, before any turn.
    SessionStart {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// Stable id for this session run, carried by every turn's `request`.
        session_id: String,
    },
    /// Emitted once by `leg session` on a clean exit (EOF / `/exit`).
    SessionEnd {
        /// Schema discriminator ([`SCHEMA`]).
        schema: &'static str,
        /// Wall-clock emission time, Unix epoch milliseconds.
        ts_ms: u64,
        /// The session this closes; equals the matching `SessionStart.session_id`.
        session_id: String,
        /// Count of turns whose `request` was emitted in this session.
        turns: u64,
    },
}

/// The terminal status of a persisted `tool_result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// The tool ran and returned output.
    Completed,
    /// The tool was unknown or its handler returned an error.
    Failed,
    /// The pre-tool hook vetoed the call before its handler ran.
    Denied,
}

/// Read-side mirror of a `tool_round` trail line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolRoundRecord {
    /// The `tool_use` reply's content blocks, in reply order.
    pub content: Vec<ContentBlock>,
}

/// Read-side mirror of a `tool_call` trail line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolCallRecord {
    /// Wall-clock emission time, Unix epoch milliseconds.
    pub ts_ms: u64,
    /// Provider-assigned call id.
    pub tool_use_id: String,
    /// The called tool's name; empty when a legacy line omits it.
    #[serde(default)]
    pub tool_name: String,
    /// The call's JSON arguments.
    #[serde(default)]
    pub input: serde_json::Value,
}

/// Read-side mirror of a `tool_result` trail line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolResultRecord {
    /// Wall-clock emission time, Unix epoch milliseconds.
    pub ts_ms: u64,
    /// The `tool_call` this result answers.
    pub tool_use_id: String,
    /// The called tool's name; empty when a legacy line omits it.
    #[serde(default)]
    pub tool_name: String,
    /// Whether the call completed or failed.
    pub status: ToolStatus,
    /// The tool's output, when `completed`.
    #[serde(default)]
    pub result: Option<String>,
    /// The failure message, when `failed`.
    #[serde(default)]
    pub error: Option<String>,
}

impl ExchangeEvent {
    /// Builds the persisted form of a tool-loop [`ToolEvent`], stamped with
    /// the session turn's `(session_id, turn_index)` when there is one.
    pub fn from_tool_event(ts_ms: u64, event: ToolEvent<'_>, turn: Option<(&str, u64)>) -> Self {
        let session_id = turn.map(|(id, _)| id.to_string());
        let turn_index = turn.map(|(_, index)| index);
        match event {
            ToolEvent::Round { content } => ExchangeEvent::ToolRound {
                schema: SCHEMA,
                ts_ms,
                content: content.to_vec(),
                session_id,
                turn_index,
            },
            ToolEvent::Call { id, name, input } => ExchangeEvent::ToolCall {
                schema: SCHEMA,
                ts_ms,
                tool_use_id: id.to_string(),
                tool_name: name.to_string(),
                input: input.clone(),
                session_id,
                turn_index,
            },
            ToolEvent::Result {
                id,
                name,
                output,
                status,
            } => {
                let (result, error) = match status {
                    ToolStatus::Completed => (Some(output.to_string()), None),
                    ToolStatus::Failed | ToolStatus::Denied => (None, Some(output.to_string())),
                };
                ExchangeEvent::ToolResult {
                    schema: SCHEMA,
                    ts_ms,
                    tool_use_id: id.to_string(),
                    tool_name: name.to_string(),
                    status,
                    result,
                    error,
                    session_id,
                    turn_index,
                }
            }
        }
    }

    /// Builds the request event for a single-turn `ask`/`exchange` (no
    /// session framing).
    pub fn request(ts_ms: u64, meta: &ExchangeMeta, prompt: &str) -> Self {
        ExchangeEvent::Request {
            schema: SCHEMA,
            ts_ms,
            model: meta.model.clone(),
            base_url: meta.base_url.clone(),
            prompt: prompt.to_string(),
            content: None,
            session_id: None,
            turn_index: None,
        }
    }

    /// Builds a session turn's request event, stamped with the run's
    /// `session_id` and this turn's `turn_index`.
    pub fn session_request(
        ts_ms: u64,
        meta: &ExchangeMeta,
        prompt: &str,
        session_id: &str,
        turn_index: u64,
    ) -> Self {
        ExchangeEvent::Request {
            schema: SCHEMA,
            ts_ms,
            model: meta.model.clone(),
            base_url: meta.base_url.clone(),
            prompt: prompt.to_string(),
            content: None,
            session_id: Some(session_id.to_string()),
            turn_index: Some(turn_index),
        }
    }

    /// Builds the session-start marker stamping the run's `session_id`.
    pub fn session_start(ts_ms: u64, session_id: &str) -> Self {
        ExchangeEvent::SessionStart {
            schema: SCHEMA,
            ts_ms,
            session_id: session_id.to_string(),
        }
    }

    /// Builds the session-end marker, recording the run's `session_id` and the
    /// number of turns emitted.
    pub fn session_end(ts_ms: u64, session_id: &str, turns: u64) -> Self {
        ExchangeEvent::SessionEnd {
            schema: SCHEMA,
            ts_ms,
            session_id: session_id.to_string(),
            turns,
        }
    }

    /// Builds the success outcome event for a single-turn `ask`/`exchange`
    /// (no session framing).
    pub fn response_ok(
        ts_ms: u64,
        duration_ms: u64,
        reply: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        stop_reason: Option<&str>,
    ) -> Self {
        Self::response_ok_inner(
            ts_ms,
            duration_ms,
            reply,
            input_tokens,
            output_tokens,
            stop_reason,
            None,
            None,
        )
    }

    /// Builds a success outcome for a session turn, carrying the same
    /// `session_id`/`turn_index` as its request.
    #[allow(clippy::too_many_arguments)]
    pub fn session_response_ok(
        ts_ms: u64,
        duration_ms: u64,
        reply: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        stop_reason: Option<&str>,
        session_id: &str,
        turn_index: u64,
    ) -> Self {
        Self::response_ok_inner(
            ts_ms,
            duration_ms,
            reply,
            input_tokens,
            output_tokens,
            stop_reason,
            Some(session_id),
            Some(turn_index),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn response_ok_inner(
        ts_ms: u64,
        duration_ms: u64,
        reply: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        stop_reason: Option<&str>,
        session_id: Option<&str>,
        turn_index: Option<u64>,
    ) -> Self {
        ExchangeEvent::ResponseOk {
            schema: SCHEMA,
            ts_ms,
            duration_ms,
            reply: reply.to_string(),
            content: None,
            input_tokens,
            output_tokens,
            stop_reason: stop_reason.map(str::to_string),
            session_id: session_id.map(str::to_string),
            turn_index,
        }
    }

    /// Builds the failure outcome event for a single-turn `ask`/`exchange`
    /// (no session framing).
    pub fn response_error(ts_ms: u64, duration_ms: u64, err: &crate::error::LegError) -> Self {
        Self::response_error_inner(ts_ms, duration_ms, err, None, None)
    }

    /// Builds a failure outcome for a session turn, carrying the same
    /// `session_id`/`turn_index` as its request.
    pub fn session_response_error(
        ts_ms: u64,
        duration_ms: u64,
        err: &crate::error::LegError,
        session_id: &str,
        turn_index: u64,
    ) -> Self {
        Self::response_error_inner(ts_ms, duration_ms, err, Some(session_id), Some(turn_index))
    }

    fn response_error_inner(
        ts_ms: u64,
        duration_ms: u64,
        err: &crate::error::LegError,
        session_id: Option<&str>,
        turn_index: Option<u64>,
    ) -> Self {
        ExchangeEvent::ResponseError {
            schema: SCHEMA,
            ts_ms,
            duration_ms,
            kind: err.kind().to_string(),
            message: err.to_string(),
            session_id: session_id.map(str::to_string),
            turn_index,
        }
    }

    /// Attaches `blocks` as the `content` of a `request` or `response_ok`
    /// event, keeping the field absent when `blocks` is text-only (see
    /// [`trail_content`]). A no-op on every other event kind.
    pub fn with_content(mut self, blocks: &[ContentBlock]) -> Self {
        match &mut self {
            ExchangeEvent::Request { content, .. } | ExchangeEvent::ResponseOk { content, .. } => {
                *content = trail_content(blocks);
            }
            _ => {}
        }
        self
    }

    /// Mirrors an already-recorded [`RequestRecord`] (from a [`Participant`]'s
    /// in-band [`Exchange`]) onto the flat JSONL trail, so `ask`/`exchange`'s
    /// single call through [`crate::participant::LocalParticipant`] and
    /// `session`'s direct calls emit exactly the same wire shape.
    ///
    /// [`Participant`]: crate::participant::Participant
    pub fn from_request_record(request: &RequestRecord) -> Self {
        ExchangeEvent::Request {
            schema: SCHEMA,
            ts_ms: request.ts_ms,
            model: request.model.clone(),
            base_url: request.base_url.clone(),
            prompt: request.prompt.clone(),
            content: request.content.clone(),
            session_id: request.session_id.clone(),
            turn_index: request.turn_index,
        }
    }

    /// Mirrors an already-recorded [`Outcome`] onto the flat JSONL trail; see
    /// [`ExchangeEvent::from_request_record`].
    pub fn from_outcome(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Ok {
                ts_ms,
                duration_ms,
                reply,
                content,
                input_tokens,
                output_tokens,
                stop_reason,
                session_id,
                turn_index,
            } => ExchangeEvent::ResponseOk {
                schema: SCHEMA,
                ts_ms: *ts_ms,
                duration_ms: *duration_ms,
                reply: reply.clone(),
                content: content.clone(),
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                stop_reason: stop_reason.clone(),
                session_id: session_id.clone(),
                turn_index: *turn_index,
            },
            Outcome::Error {
                ts_ms,
                duration_ms,
                kind,
                message,
                session_id,
                turn_index,
            } => ExchangeEvent::ResponseError {
                schema: SCHEMA,
                ts_ms: *ts_ms,
                duration_ms: *duration_ms,
                kind: kind.clone(),
                message: message.clone(),
                session_id: session_id.clone(),
                turn_index: *turn_index,
            },
        }
    }
}

/// Sink for exchange events.
///
/// Implementations persist or discard events; the orchestration code is
/// written against this trait so recording can be toggled without branching
/// the exchange logic.
pub trait EventSink {
    /// Records a single event. Returns an error only if persistence failed;
    /// the caller decides whether that is fatal (it is not, on `leg`'s
    /// paths — a sink failure is downgraded to a stderr warning).
    fn record(&mut self, event: &ExchangeEvent) -> io::Result<()>;
}

/// A shared handle records through the sink it wraps, so one opened trail can
/// be written by both a driver and the tool loop's observer.
impl<S: EventSink + ?Sized> EventSink for std::rc::Rc<std::cell::RefCell<S>> {
    fn record(&mut self, event: &ExchangeEvent) -> io::Result<()> {
        self.borrow_mut().record(event)
    }
}

impl<S: EventSink + ?Sized> EventSink for Box<S> {
    fn record(&mut self, event: &ExchangeEvent) -> io::Result<()> {
        (**self).record(event)
    }
}

/// An [`EventSink`] that discards everything. Used when recording is disabled.
pub struct NoopSink;

impl EventSink for NoopSink {
    fn record(&mut self, _event: &ExchangeEvent) -> io::Result<()> {
        Ok(())
    }
}

/// An [`EventSink`] that writes one JSON object per line to a [`Write`].
///
/// Each event is flushed immediately so a consumer tailing the sink sees the
/// request line before the (possibly slow) response line.
pub struct WriterSink<W: Write> {
    writer: W,
}

impl<W: Write> WriterSink<W> {
    /// Creates a sink that writes JSONL to `writer`.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write> EventSink for WriterSink<W> {
    fn record(&mut self, event: &ExchangeEvent) -> io::Result<()> {
        let line = serde_json::to_string(event).map_err(io::Error::other)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn meta() -> ExchangeMeta {
        ExchangeMeta {
            model: "claude-test-model".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    #[test]
    fn request_event_serializes_with_schema_and_replay_fields() {
        let event = ExchangeEvent::request(1_700_000_000_000, &meta(), "hello");
        let value: Value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["event"], "request");
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["ts_ms"], 1_700_000_000_000u64);
        assert_eq!(value["model"], "claude-test-model");
        assert_eq!(value["base_url"], "https://api.anthropic.com");
        assert_eq!(value["prompt"], "hello");
        assert!(value.get("session_id").is_none());
        assert!(value.get("turn_index").is_none());
    }

    #[test]
    fn session_request_event_carries_session_framing() {
        let event = ExchangeEvent::session_request(1, &meta(), "hi", "sess-1", 2);
        let value: Value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["session_id"], "sess-1");
        assert_eq!(value["turn_index"], 2);
    }

    #[test]
    fn response_ok_event_omits_absent_optional_fields() {
        let event = ExchangeEvent::response_ok(1, 2, "hi", None, None, None);
        let value: Value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["event"], "response_ok");
        assert!(value.get("input_tokens").is_none());
        assert!(value.get("output_tokens").is_none());
        assert!(value.get("stop_reason").is_none());
        assert!(value.get("session_id").is_none());
    }

    #[test]
    fn session_start_and_end_serialize_with_session_id() {
        let start = ExchangeEvent::session_start(1, "sess-1");
        let value: Value = serde_json::to_value(&start).expect("serializes");
        assert_eq!(value["event"], "session_start");
        assert_eq!(value["session_id"], "sess-1");

        let end = ExchangeEvent::session_end(2, "sess-1", 3);
        let value: Value = serde_json::to_value(&end).expect("serializes");
        assert_eq!(value["event"], "session_end");
        assert_eq!(value["turns"], 3);
    }

    #[test]
    fn response_error_event_serializes_kind_and_message() {
        let err = crate::error::LegError::Auth("bad credentials".to_string());
        let event = ExchangeEvent::response_error(1, 2, &err);
        let value: Value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["event"], "response_error");
        assert_eq!(value["kind"], "auth");
        assert_eq!(value["message"], err.to_string());
    }

    #[test]
    fn writer_sink_writes_one_json_line_per_event() {
        let mut buf = Vec::new();
        {
            let mut sink = WriterSink::new(&mut buf);
            sink.record(&ExchangeEvent::request(1, &meta(), "hello"))
                .expect("records");
            sink.record(&ExchangeEvent::response_ok(2, 1, "hi", None, None, None))
                .expect("records");
        }
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(first["event"], "request");
        let second: Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(second["event"], "response_ok");
    }

    #[test]
    fn tool_round_event_serializes_its_field_names() {
        let content = [
            ContentBlock::text("calling"),
            ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "echo".to_string(),
                input: serde_json::json!({"text": "hi"}),
            },
        ];
        let event = ExchangeEvent::from_tool_event(
            4,
            ToolEvent::Round { content: &content },
            Some(("sess-1", 3)),
        );
        assert_eq!(
            serde_json::to_value(&event).expect("serializes"),
            serde_json::json!({
                "event": "tool_round",
                "schema": SCHEMA,
                "ts_ms": 4,
                "content": [
                    {"type": "text", "text": "calling"},
                    {"type": "tool_use", "id": "toolu_1", "name": "echo", "input": {"text": "hi"}},
                ],
                "session_id": "sess-1",
                "turn_index": 3,
            })
        );
    }

    #[test]
    fn tool_call_event_serializes_its_field_names() {
        let input = serde_json::json!({"text": "hi"});
        let event = ExchangeEvent::from_tool_event(
            5,
            ToolEvent::Call {
                id: "toolu_1",
                name: "echo",
                input: &input,
            },
            None,
        );
        let value: Value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "event": "tool_call",
                "schema": SCHEMA,
                "ts_ms": 5,
                "tool_use_id": "toolu_1",
                "tool_name": "echo",
                "input": {"text": "hi"},
            })
        );
    }

    #[test]
    fn tool_result_event_serializes_completed_failed_and_denied_payloads() {
        let ok = ExchangeEvent::from_tool_event(
            6,
            ToolEvent::Result {
                id: "toolu_1",
                name: "echo",
                output: "echo: hi",
                status: ToolStatus::Completed,
            },
            Some(("sess-1", 2)),
        );
        assert_eq!(
            serde_json::to_value(&ok).expect("serializes"),
            serde_json::json!({
                "event": "tool_result",
                "schema": SCHEMA,
                "ts_ms": 6,
                "tool_use_id": "toolu_1",
                "tool_name": "echo",
                "status": "completed",
                "result": "echo: hi",
                "session_id": "sess-1",
                "turn_index": 2,
            })
        );

        let failed = ExchangeEvent::from_tool_event(
            7,
            ToolEvent::Result {
                id: "toolu_2",
                name: "missing",
                output: "unknown tool: missing",
                status: ToolStatus::Failed,
            },
            None,
        );
        assert_eq!(
            serde_json::to_value(&failed).expect("serializes"),
            serde_json::json!({
                "event": "tool_result",
                "schema": SCHEMA,
                "ts_ms": 7,
                "tool_use_id": "toolu_2",
                "tool_name": "missing",
                "status": "failed",
                "error": "unknown tool: missing",
            })
        );

        let denied = ExchangeEvent::from_tool_event(
            8,
            ToolEvent::Result {
                id: "toolu_3",
                name: "bash",
                output: "denied by pre-tool hook: role policy",
                status: ToolStatus::Denied,
            },
            None,
        );
        assert_eq!(
            serde_json::to_value(&denied).expect("serializes"),
            serde_json::json!({
                "event": "tool_result",
                "schema": SCHEMA,
                "ts_ms": 8,
                "tool_use_id": "toolu_3",
                "tool_name": "bash",
                "status": "denied",
                "error": "denied by pre-tool hook: role policy",
            })
        );
    }

    #[test]
    fn noop_sink_discards_events() {
        let mut sink = NoopSink;
        sink.record(&ExchangeEvent::request(1, &meta(), "hello"))
            .expect("noop never fails");
    }
}
