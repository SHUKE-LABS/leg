use serde_json::Value;
use thiserror::Error;

pub const STREAM_SCHEMA: &str = "leg.exchange.stream/v1";

#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TurnStart {
        seq: u64,
        request: Value,
        provider: String,
        model: String,
        session_id: Option<String>,
        turn_index: Option<u64>,
    },
    TextDelta {
        seq: u64,
        round_index: u64,
        block_index: u64,
        text: String,
    },
    ToolRound {
        seq: u64,
        round_index: u64,
        content: Value,
    },
    ToolCall {
        seq: u64,
        round_index: u64,
        tool_use_id: String,
        tool_name: String,
        input: Value,
    },
    ToolResult {
        seq: u64,
        round_index: u64,
        tool_use_id: String,
        tool_name: String,
        status: String,
        output: Value,
    },
    TurnEnd {
        seq: u64,
        response: Value,
        capped: bool,
        session_id: Option<String>,
        turn_index: Option<u64>,
    },
    /// A future event from the supported schema. Consumers may ignore it.
    Unknown {
        seq: u64,
        event: String,
        record: Value,
    },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StreamFailure {
    #[error("the leg stream record is not valid JSON")]
    InvalidJson,
    #[error("the leg stream record is missing a required field: {0}")]
    MissingField(&'static str),
    #[error("the leg stream record has an invalid field: {0}")]
    InvalidField(&'static str),
    #[error(
        "installed leg uses unsupported stream schema {0:?}; install a leg build with {STREAM_SCHEMA}"
    )]
    UnsupportedSchema(String),
    #[error("leg stream sequence is {actual}; expected {expected}")]
    Sequence { expected: u64, actual: u64 },
    #[error("leg stream did not start with turn_start")]
    MissingTurnStart,
    #[error("leg stream contains more than one turn_start")]
    DuplicateTurnStart,
    #[error("leg stream contains records after turn_end")]
    RecordAfterTurnEnd,
    #[error("leg response is not correlated with its request")]
    CorrelationMismatch,
}

#[derive(Default)]
pub(crate) struct StreamDecoder {
    next_seq: u64,
    started: bool,
    ended: bool,
    request_message_id: Option<String>,
    conversation_id: Option<String>,
}

impl StreamDecoder {
    pub(crate) fn accept(&mut self, bytes: &[u8]) -> Result<StreamEvent, StreamFailure> {
        if self.ended {
            return Err(StreamFailure::RecordAfterTurnEnd);
        }
        let record: Value =
            serde_json::from_slice(bytes).map_err(|_| StreamFailure::InvalidJson)?;
        let object = record
            .as_object()
            .ok_or(StreamFailure::InvalidField("record"))?;
        let schema = required_str(object.get("schema"), "schema")?;
        if schema != STREAM_SCHEMA {
            return Err(StreamFailure::UnsupportedSchema(schema.to_string()));
        }
        let event = required_str(object.get("event"), "event")?;
        let seq = required_u64(object.get("seq"), "seq")?;
        if seq != self.next_seq {
            return Err(StreamFailure::Sequence {
                expected: self.next_seq,
                actual: seq,
            });
        }
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .ok_or(StreamFailure::InvalidField("seq"))?;

        if !self.started && event != "turn_start" {
            return Err(StreamFailure::MissingTurnStart);
        }
        if self.started && event == "turn_start" {
            return Err(StreamFailure::DuplicateTurnStart);
        }

        let parsed = match event {
            "turn_start" => {
                let request = object
                    .get("request")
                    .cloned()
                    .ok_or(StreamFailure::MissingField("request"))?;
                let request_object = request
                    .as_object()
                    .ok_or(StreamFailure::InvalidField("request"))?;
                if request_object.get("schema").and_then(Value::as_str) != Some("baton.message/v1")
                {
                    return Err(StreamFailure::InvalidField("request.schema"));
                }
                let message_id =
                    required_str(request_object.get("message_id"), "request.message_id")?;
                let conversation_id = required_str(
                    request_object.get("conversation_id"),
                    "request.conversation_id",
                )?;
                self.request_message_id = Some(message_id.to_string());
                self.conversation_id = Some(conversation_id.to_string());
                self.started = true;
                let (session_id, turn_index) = optional_session(object)?;
                StreamEvent::TurnStart {
                    seq,
                    request,
                    provider: required_str(object.get("provider"), "provider")?.to_string(),
                    model: required_str(object.get("model"), "model")?.to_string(),
                    session_id,
                    turn_index,
                }
            }
            "text_delta" => StreamEvent::TextDelta {
                seq,
                round_index: required_u64(object.get("round_index"), "round_index")?,
                block_index: required_u64(object.get("block_index"), "block_index")?,
                text: required_str(object.get("text"), "text")?.to_string(),
            },
            "tool_round" => {
                let content = object
                    .get("content")
                    .cloned()
                    .ok_or(StreamFailure::MissingField("content"))?;
                if !content.is_array() {
                    return Err(StreamFailure::InvalidField("content"));
                }
                StreamEvent::ToolRound {
                    seq,
                    round_index: required_u64(object.get("round_index"), "round_index")?,
                    content,
                }
            }
            "tool_call" => StreamEvent::ToolCall {
                seq,
                round_index: required_u64(object.get("round_index"), "round_index")?,
                tool_use_id: required_str(object.get("tool_use_id"), "tool_use_id")?.to_string(),
                tool_name: required_str(object.get("tool_name"), "tool_name")?.to_string(),
                input: object
                    .get("input")
                    .cloned()
                    .ok_or(StreamFailure::MissingField("input"))?,
            },
            "tool_result" => {
                let status = required_str(object.get("status"), "status")?;
                if !matches!(status, "completed" | "failed" | "denied") {
                    return Err(StreamFailure::InvalidField("status"));
                }
                StreamEvent::ToolResult {
                    seq,
                    round_index: required_u64(object.get("round_index"), "round_index")?,
                    tool_use_id: required_str(object.get("tool_use_id"), "tool_use_id")?
                        .to_string(),
                    tool_name: required_str(object.get("tool_name"), "tool_name")?.to_string(),
                    status: status.to_string(),
                    output: object
                        .get("output")
                        .cloned()
                        .ok_or(StreamFailure::MissingField("output"))?,
                }
            }
            "turn_end" => {
                let response = object
                    .get("response")
                    .cloned()
                    .ok_or(StreamFailure::MissingField("response"))?;
                self.validate_correlation(&response)?;
                let (session_id, turn_index) = optional_session(object)?;
                let capped = object
                    .get("capped")
                    .and_then(Value::as_bool)
                    .ok_or(StreamFailure::InvalidField("capped"))?;
                self.ended = true;
                StreamEvent::TurnEnd {
                    seq,
                    response,
                    capped,
                    session_id,
                    turn_index,
                }
            }
            unknown => StreamEvent::Unknown {
                seq,
                event: unknown.to_string(),
                record,
            },
        };
        Ok(parsed)
    }

    fn validate_correlation(&self, response: &Value) -> Result<(), StreamFailure> {
        let object = response
            .as_object()
            .ok_or(StreamFailure::InvalidField("response"))?;
        if object.get("schema").and_then(Value::as_str) != Some("baton.message/v1") {
            return Err(StreamFailure::InvalidField("response.schema"));
        }
        let message = required_str(object.get("in_reply_to"), "response.in_reply_to")?;
        let conversation = required_str(object.get("conversation_id"), "response.conversation_id")?;
        let kind = required_str(object.get("kind"), "response.kind")?;
        if !matches!(kind, "response" | "error")
            || self.request_message_id.as_deref() != Some(message)
            || self.conversation_id.as_deref() != Some(conversation)
        {
            return Err(StreamFailure::CorrelationMismatch);
        }
        Ok(())
    }
}

fn required_str<'a>(
    value: Option<&'a Value>,
    name: &'static str,
) -> Result<&'a str, StreamFailure> {
    value
        .and_then(Value::as_str)
        .ok_or(StreamFailure::MissingField(name))
}

fn required_u64(value: Option<&Value>, name: &'static str) -> Result<u64, StreamFailure> {
    value
        .and_then(Value::as_u64)
        .ok_or(StreamFailure::MissingField(name))
}

fn optional_session(
    object: &serde_json::Map<String, Value>,
) -> Result<(Option<String>, Option<u64>), StreamFailure> {
    let session_id = object.get("session_id");
    let turn_index = object.get("turn_index");
    match (session_id, turn_index) {
        (None, None) => Ok((None, None)),
        (Some(id), Some(index)) => Ok((
            Some(
                id.as_str()
                    .ok_or(StreamFailure::InvalidField("session_id"))?
                    .to_string(),
            ),
            Some(
                index
                    .as_u64()
                    .ok_or(StreamFailure::InvalidField("turn_index"))?,
            ),
        )),
        _ => Err(StreamFailure::InvalidField("session coordinates")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn_start(seq: u64) -> Value {
        json!({
            "schema": STREAM_SCHEMA,
            "event": "turn_start",
            "seq": seq,
            "provider": "anthropic",
            "model": "test-model",
            "request": {
                "schema": "baton.message/v1",
                "message_id": "request-1",
                "conversation_id": "conversation-1"
            }
        })
    }

    #[test]
    fn future_events_in_the_supported_schema_are_preserved() {
        let mut decoder = StreamDecoder::default();
        decoder
            .accept(&serde_json::to_vec(&turn_start(0)).unwrap())
            .unwrap();

        let record = json!({
            "schema": STREAM_SCHEMA,
            "event": "future_event",
            "seq": 1,
            "new_field": "future value"
        });
        assert!(matches!(
            decoder.accept(&serde_json::to_vec(&record).unwrap()),
            Ok(StreamEvent::Unknown { event, .. }) if event == "future_event"
        ));
    }

    #[test]
    fn unsupported_stream_schema_is_actionable() {
        let record = json!({
            "schema": "leg.exchange.stream/v2",
            "event": "turn_start",
            "seq": 0
        });
        assert_eq!(
            StreamDecoder::default()
                .accept(&serde_json::to_vec(&record).unwrap())
                .unwrap_err(),
            StreamFailure::UnsupportedSchema("leg.exchange.stream/v2".to_string())
        );
    }

    #[test]
    fn turn_end_requires_matching_request_and_response_envelopes() {
        let mut decoder = StreamDecoder::default();
        decoder
            .accept(&serde_json::to_vec(&turn_start(0)).unwrap())
            .unwrap();
        let end = json!({
            "schema": STREAM_SCHEMA,
            "event": "turn_end",
            "seq": 1,
            "capped": false,
            "response": {
                "schema": "baton.message/v1",
                "message_id": "response-1",
                "conversation_id": "conversation-1",
                "in_reply_to": "request-1",
                "kind": "response"
            }
        });
        assert!(matches!(
            decoder.accept(&serde_json::to_vec(&end).unwrap()),
            Ok(StreamEvent::TurnEnd { .. })
        ));
    }
}
