//! The participant seam: an envelope-in / envelope-out boundary.
//!
//! [`Participant`] is the `ask` driver's infallible contract: a provider (or
//! delivery) failure is a *delivered* `kind: "error"` response, never a
//! propagated `Err`. [`LocalParticipant`] is the only implementation ported at
//! this slice — an in-process, LLM-backed participant that is a
//! [`crate::transport::Transport`] plus the metadata stamped on its nested
//! exchange record. Subprocess/mailbox/external-agent participants (baton's
//! `SubprocessParticipant`/`MailboxParticipant`/`ExternalAgentParticipant`) are
//! harness pieces (mailbox/service) out of scope for `leg`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::events::{Exchange, ExchangeMeta, Outcome, RequestRecord, now_ms};
use crate::message::{MessageEnvelope, MessageKind, WrappedExchange};
use crate::model::Prompt;
use crate::transport::Transport;

/// Answers a `baton.message/v1` request envelope with a response envelope.
///
/// Infallible by contract: a provider (or delivery) failure is a *delivered*
/// `kind: "error"` response, never a propagated `Err`.
pub trait Participant {
    /// Consumes a `request` envelope and returns the correlated response.
    fn respond(&self, request: &MessageEnvelope) -> MessageEnvelope;
}

/// An in-process, LLM-backed participant: a [`Transport`] plus exchange
/// metadata.
///
/// The response envelope preserves `conversation_id`, links `in_reply_to` to
/// the request, swaps addressing (`from`/`to`), and nests the
/// `baton.exchange/v1` record for the call it ran so the call — and its token
/// usage — is observable in-band. [`ExchangeMeta`] supplies the `model`/
/// `base_url` stamped on that nested record.
pub struct LocalParticipant<T: Transport> {
    transport: T,
    meta: ExchangeMeta,
}

impl<T: Transport> LocalParticipant<T> {
    /// Builds a participant over `transport`, stamping `meta` (`model` /
    /// `base_url`) onto the nested `baton.exchange/v1` record of each reply.
    pub fn new(transport: T, meta: ExchangeMeta) -> Self {
        Self { transport, meta }
    }
}

impl<T: Transport> Participant for LocalParticipant<T> {
    fn respond(&self, request: &MessageEnvelope) -> MessageEnvelope {
        let request_ts = now_ms();
        let start = Instant::now();
        let result = self.transport.send(&Prompt::new(request.body.as_str()));
        let duration_ms = start.elapsed().as_millis() as u64;
        let outcome_ts = now_ms();

        let request_record = RequestRecord {
            ts_ms: request_ts,
            model: self.meta.model.clone(),
            base_url: self.meta.base_url.clone(),
            prompt: request.body.clone(),
        };

        let (kind, body, outcome) = match result {
            Ok(reply) => {
                let outcome = Outcome::Ok {
                    ts_ms: outcome_ts,
                    duration_ms,
                    reply: reply.text.clone(),
                    input_tokens: reply.usage.input_tokens,
                    output_tokens: reply.usage.output_tokens,
                    stop_reason: reply.stop_reason.clone(),
                };
                (MessageKind::Response, reply.text, outcome)
            }
            Err(err) => {
                let outcome = Outcome::Error {
                    ts_ms: outcome_ts,
                    duration_ms,
                    kind: err.kind().to_string(),
                    message: err.to_string(),
                };
                (MessageKind::Error, err.to_string(), outcome)
            }
        };

        // Addressing swaps: the reply is from the request's recipient, to its
        // sender.
        let mut response = MessageEnvelope::new(
            fresh_message_id(&request.conversation_id, outcome_ts),
            request.conversation_id.clone(),
            request.to.clone(),
            request.from.clone(),
            kind,
            body,
            outcome_ts,
        );
        response.in_reply_to = Some(request.message_id.clone());
        response.exchange = Some(WrappedExchange::new(Exchange {
            request: request_record,
            outcome,
        }));
        response
    }
}

/// A process-lifetime counter making every synthesized response id distinct.
///
/// Millisecond timestamps do not separate emissions in a tight loop, so the
/// counter — not the timestamp — carries uniqueness.
static RESPONSE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Builds a fresh `message_id` for a response without adding a dependency.
///
/// Derived from the conversation id, the response timestamp, and a draw from
/// [`RESPONSE_SEQ`]: each call takes a value no other call in this process
/// takes, so two replies emitted within the same millisecond still differ.
fn fresh_message_id(conversation_id: &str, ts_ms: u64) -> String {
    let seq = RESPONSE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{conversation_id}-r-{ts_ms}-{seq}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LegError;
    use crate::model::AssistantReply;

    struct FakeTransport(Result<AssistantReply, ()>);

    impl Transport for FakeTransport {
        fn send_conversation(
            &self,
            _messages: &[crate::model::Message],
        ) -> crate::error::Result<AssistantReply> {
            match &self.0 {
                Ok(reply) => Ok(reply.clone()),
                Err(()) => Err(LegError::Auth("bad credentials".to_string())),
            }
        }
    }

    fn request() -> MessageEnvelope {
        let mut envelope = MessageEnvelope::new(
            "m-1",
            "c-1",
            "user",
            "assistant",
            MessageKind::Request,
            "hello",
            1_700_000_000_000,
        );
        envelope.in_reply_to = None;
        envelope
    }

    fn meta() -> ExchangeMeta {
        ExchangeMeta {
            model: "claude-test-model".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    #[test]
    fn success_reply_swaps_addressing_and_nests_ok_outcome() {
        let participant =
            LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
        let response = participant.respond(&request());

        assert_eq!(response.kind, MessageKind::Response);
        assert_eq!(response.body, "hi there");
        assert_eq!(response.from, "assistant");
        assert_eq!(response.to, "user");
        assert_eq!(response.conversation_id, "c-1");
        assert_eq!(response.in_reply_to.as_deref(), Some("m-1"));
        match &response.exchange.expect("wrapped").exchange.outcome {
            Outcome::Ok { reply, .. } => assert_eq!(reply, "hi there"),
            other => panic!("expected Ok outcome, got {other:?}"),
        }
    }

    #[test]
    fn provider_failure_is_delivered_as_error_envelope_not_a_panic_or_err() {
        let participant = LocalParticipant::new(FakeTransport(Err(())), meta());
        let response = participant.respond(&request());

        assert_eq!(response.kind, MessageKind::Error);
        assert_eq!(response.body, "authentication error: bad credentials");
        match &response.exchange.expect("wrapped").exchange.outcome {
            Outcome::Error { kind, .. } => assert_eq!(kind, "auth"),
            other => panic!("expected Error outcome, got {other:?}"),
        }
    }

    #[test]
    fn fresh_message_id_is_unique_across_calls() {
        let a = fresh_message_id("c-1", 1_000);
        let b = fresh_message_id("c-1", 1_000);
        assert_ne!(a, b);
    }
}
