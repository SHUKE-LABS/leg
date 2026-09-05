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
//! trail (`BATON_EVENT_LOG`) and its `ExchangeEvent`/`EventSink` write path;
//! that machinery is not ported here — no acceptance criterion for `leg ask`
//! needs a persisted trail, and reading/replaying one is a later slice
//! (`leg log`, leg#3). `Exchange`/`RequestRecord`/`Outcome` themselves are
//! relocated here (rather than a `log` module, per that upstream module's
//! split) since a later `leg log` can import them from here without a
//! breaking move.

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
