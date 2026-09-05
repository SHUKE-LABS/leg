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
    /// Session this turn belongs to; absent on the single-turn `ask` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Monotonic turn number within the session; absent on the `ask` path.
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
        /// Provider-reported input (prompt) tokens; omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Provider-reported output (completion) tokens; omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Provider-reported terminal reason; omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Session this outcome belongs to; absent on the `ask` path.
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
        /// Session this outcome belongs to; absent on the `ask` path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Monotonic turn number matching the session request; absent when
        /// `session_id` is absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
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

impl ExchangeEvent {
    /// Builds the request event for the single-turn `ask` path (no session
    /// framing).
    pub fn request(ts_ms: u64, meta: &ExchangeMeta, prompt: &str) -> Self {
        ExchangeEvent::Request {
            schema: SCHEMA,
            ts_ms,
            model: meta.model.clone(),
            base_url: meta.base_url.clone(),
            prompt: prompt.to_string(),
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

    /// Builds the success outcome event for the single-turn `ask` path (no
    /// session framing).
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
            input_tokens,
            output_tokens,
            stop_reason: stop_reason.map(str::to_string),
            session_id: session_id.map(str::to_string),
            turn_index,
        }
    }

    /// Builds the failure outcome event for the single-turn `ask` path (no
    /// session framing).
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

    /// Mirrors an already-recorded [`RequestRecord`] (from a [`Participant`]'s
    /// in-band [`Exchange`]) onto the flat JSONL trail, so `ask`'s single call
    /// through [`crate::participant::LocalParticipant`] and `session`'s direct
    /// calls emit exactly the same wire shape.
    ///
    /// [`Participant`]: crate::participant::Participant
    pub fn from_request_record(request: &RequestRecord) -> Self {
        ExchangeEvent::Request {
            schema: SCHEMA,
            ts_ms: request.ts_ms,
            model: request.model.clone(),
            base_url: request.base_url.clone(),
            prompt: request.prompt.clone(),
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
    fn noop_sink_discards_events() {
        let mut sink = NoopSink;
        sink.record(&ExchangeEvent::request(1, &meta(), "hello"))
            .expect("noop never fails");
    }
}
