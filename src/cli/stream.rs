//! The opt-in `leg exchange --stream-json` wire format.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

use serde_json::{Map, Value, json};

use super::*;
use crate::config::Provider;
use crate::events::{RequestRecord, trail_content};
use crate::tools::OwnedToolEvent;
use crate::transport::Transport;

const STREAM_SCHEMA: &str = "leg.exchange.stream/v1";

pub(super) fn execute_exchange_stream_json(
    in_path: Option<&str>,
    session: Option<ExchangeSession>,
    session_id_out: Option<&str>,
) -> Result<()> {
    match session {
        None => execute_cold_exchange(in_path),
        Some(session) => execute_session_exchange(in_path, session, session_id_out),
    }
}

fn execute_cold_exchange(in_path: Option<&str>) -> Result<()> {
    let config = LegConfig::from_env()?;
    let provider = config.provider;
    let meta = exchange_meta(&config);
    let raw = read_exchange_input(in_path)?;
    let (request, _) = parse_exchange_request(&raw);
    let transport = build_transport(config);
    let mut sink = open_event_sink();
    let stdout = std::io::stdout();
    let writer = Rc::new(RefCell::new(StreamJsonWriter::new(stdout.lock())));

    execute_cold_exchange_core(
        &transport,
        &meta,
        provider,
        &request,
        sink.as_mut(),
        &writer,
    )
}

fn execute_session_exchange(
    in_path: Option<&str>,
    session: ExchangeSession,
    session_id_out: Option<&str>,
) -> Result<()> {
    let store_dir = session_store_dir()?;
    let (mut resumed, session_path, create_new) = match session {
        ExchangeSession::Existing(session_id) => {
            let path = exchange_session_path(&store_dir, &session_id)
                .ok_or_else(|| LegError::SessionNotFound(session_id.clone()))?;
            let resumed = load_exchange_session(&path, &session_id)?;
            (resumed, path, false)
        }
        ExchangeSession::New => {
            let session_id = new_session_id();
            let path = exchange_session_path(&store_dir, &session_id)
                .ok_or_else(|| LegError::Config("generated an invalid session id".to_string()))?;
            (
                ResumedSession {
                    session_id,
                    conversation: Conversation::new(),
                    prior_turns: 0,
                    next_turn_index: 0,
                },
                path,
                true,
            )
        }
    };

    let config = LegConfig::from_env()?;
    let provider = config.provider;
    let meta = exchange_meta(&config);
    let raw = read_exchange_input(in_path)?;
    let (request, _) = parse_exchange_request(&raw);

    if create_new {
        std::fs::create_dir_all(&store_dir).map_err(|err| {
            LegError::Io(format!(
                "failed to create session store {:?}: {err}",
                store_dir
            ))
        })?;
    }

    let SessionEventSink {
        sink,
        session_write_error,
    } = open_session_event_sink(&session_path, create_new)?;
    let mut sink = sink;
    let transport = build_transport(config);
    let stdout = std::io::stdout();
    let writer = Rc::new(RefCell::new(StreamJsonWriter::new(stdout.lock())));
    let result = execute_session_exchange_core(
        &transport,
        &meta,
        provider,
        &request,
        sink.as_mut(),
        &mut resumed,
        Rc::clone(&session_write_error),
        &writer,
    );

    if let Some(path) = session_id_out {
        write_session_id_out(path, &resumed.session_id)?;
    }
    interrupt::check()?;
    match (result, session_write_error.borrow().clone()) {
        (Ok(()), Some(error)) => Err(LegError::Io(format!(
            "failed to record session trail: {error}"
        ))),
        (result, _) => result,
    }
}

fn execute_cold_exchange_core<T: Transport, W: Write>(
    transport: &ToolLoop<T>,
    meta: &ExchangeMeta,
    provider: Provider,
    request: &MessageEnvelope,
    sink: &mut dyn EventSink,
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
) -> Result<()> {
    write_turn_start(writer, request, meta, provider, None)?;
    let request_ts_ms = now_ms();
    emit(
        sink,
        &ExchangeEvent::request(request_ts_ms, meta, &request.body),
    );

    let content = [ContentBlock::text(request.body.clone())];
    let start = Instant::now();
    let call = run_streaming_turn(
        transport,
        &[Message::new(Role::User, content.to_vec())],
        sink,
        None,
        writer,
    );
    let duration_ms = start.elapsed().as_millis() as u64;
    let result = match interrupt::error() {
        Some(error) => Err(error),
        None => call.result,
    };
    let attempts = Some(call.attempts);
    let outcome_ts_ms = now_ms();
    let mut turn_error = None;
    let (kind, body, outcome, capped) = match result {
        Ok(turn) => {
            let outcome = Outcome::Ok {
                ts_ms: outcome_ts_ms,
                duration_ms,
                reply: turn.reply.text.clone(),
                content: trail_content(&turn.reply.content),
                input_tokens: turn.reply.usage.input_tokens,
                output_tokens: turn.reply.usage.output_tokens,
                stop_reason: turn
                    .reply
                    .stop_reason
                    .as_ref()
                    .map(|reason| reason.as_str().to_string()),
                attempts,
                session_id: None,
                turn_index: None,
            };
            (MessageKind::Response, turn.reply.text, outcome, turn.capped)
        }
        Err(error) => {
            let is_interrupted = matches!(error, LegError::Interrupted { .. });
            let outcome = Outcome::Error {
                ts_ms: outcome_ts_ms,
                duration_ms,
                kind: error.kind().to_string(),
                message: error.to_string(),
                attempts,
                session_id: None,
                turn_index: None,
            };
            if is_interrupted {
                turn_error = Some(error);
            }
            (
                MessageKind::Error,
                outcome_message(&outcome),
                outcome,
                false,
            )
        }
    };
    let request_record = RequestRecord {
        ts_ms: request_ts_ms,
        model: meta.model.clone(),
        base_url: meta.base_url.clone(),
        prompt: request.body.clone(),
        content: None,
        session_id: None,
        turn_index: None,
    };
    let response = build_response_envelope(
        request,
        request_record,
        kind,
        body,
        outcome_ts_ms,
        outcome.clone(),
    );
    emit(sink, &ExchangeEvent::from_outcome(&outcome));

    let terminal = write_turn_end(writer, &response, capped, None, turn_error.is_some());
    if let Some(error) = turn_error {
        let _ = terminal;
        return Err(error);
    }
    terminal?;
    if response.kind == MessageKind::Error {
        return Err(delivered_turn_failure(&response));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_session_exchange_core<T: Transport, W: Write>(
    transport: &ToolLoop<T>,
    meta: &ExchangeMeta,
    provider: Provider,
    request: &MessageEnvelope,
    sink: &mut dyn EventSink,
    resumed: &mut ResumedSession,
    session_write_error: Rc<RefCell<Option<String>>>,
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
) -> Result<()> {
    let session_id = resumed.session_id.clone();
    let turn_index = resumed.next_turn_index;
    write_turn_start(
        writer,
        request,
        meta,
        provider,
        Some((&session_id, turn_index)),
    )?;

    let request_ts_ms = now_ms();
    resumed.conversation.push_user(request.body.as_str());
    let start = Instant::now();
    let stderr = std::io::stderr();
    let mut warning = stderr.lock();
    let counters = StreamRoundCounters::default();
    let call = timed_session_exchange(
        sink,
        TimedSessionExchangeContext {
            meta,
            prompt: &request.body,
            session_id: &session_id,
            turn_index,
            max_tool_rounds: transport.max_tool_rounds(),
        },
        &mut warning,
        |sink| {
            let writer_for_tools = Rc::clone(writer);
            let counters_for_tools = &counters;
            let mut observe_tool = |event: OwnedToolEvent| {
                let trail_event = event.as_tool_event();
                emit(
                    sink,
                    &ExchangeEvent::from_tool_event(
                        now_ms(),
                        trail_event,
                        Some((session_id.as_str(), turn_index)),
                    ),
                );
                write_tool_event(&writer_for_tools, counters_for_tools, event)
            };
            let writer_for_text = Rc::clone(writer);
            let counters_for_text = &counters;
            let mut observe_stream =
                |event| write_text_delta(&writer_for_text, counters_for_text, event);
            transport.run_streaming_observed_fallible_with_attempts(
                resumed.conversation.messages(),
                &mut observe_tool,
                &mut observe_stream,
            )
        },
    );
    let duration_ms = start.elapsed().as_millis() as u64;
    let attempts = Some(call.attempts);
    resumed.next_turn_index += 1;

    let persistence_error = session_write_error.borrow().clone();
    let mut result = call.result;
    let mut persistence_failure = false;
    if result.is_ok()
        && let Some(error) = persistence_error
    {
        let error = LegError::Io(format!("failed to record session trail: {error}"));
        persistence_failure = true;
        result = Err(error);
    }

    let outcome_ts_ms = now_ms();
    let is_interrupted = matches!(&result, Err(LegError::Interrupted { .. }));
    let (kind, body, outcome, capped, terminal_error) = match result {
        Ok(turn) => {
            for message in turn.transcript {
                resumed.conversation.push(message);
            }
            resumed.conversation.push(session_reply_message(
                turn.reply.content.clone(),
                turn.capped,
            ));
            let outcome = Outcome::Ok {
                ts_ms: outcome_ts_ms,
                duration_ms,
                reply: turn.reply.text.clone(),
                content: trail_content(&turn.reply.content),
                input_tokens: turn.reply.usage.input_tokens,
                output_tokens: turn.reply.usage.output_tokens,
                stop_reason: turn
                    .reply
                    .stop_reason
                    .as_ref()
                    .map(|reason| reason.as_str().to_string()),
                attempts,
                session_id: Some(session_id.clone()),
                turn_index: Some(turn_index),
            };
            (
                MessageKind::Response,
                turn.reply.text,
                outcome,
                turn.capped,
                None,
            )
        }
        Err(error) => {
            resumed.conversation.pop();
            let body = error.to_string();
            let outcome = Outcome::Error {
                ts_ms: outcome_ts_ms,
                duration_ms,
                kind: error.kind().to_string(),
                message: body.clone(),
                attempts,
                session_id: Some(session_id.clone()),
                turn_index: Some(turn_index),
            };
            (MessageKind::Error, body, outcome, false, Some(error))
        }
    };
    let request_record = RequestRecord {
        ts_ms: request_ts_ms,
        model: meta.model.clone(),
        base_url: meta.base_url.clone(),
        prompt: request.body.clone(),
        content: None,
        session_id: Some(session_id.clone()),
        turn_index: Some(turn_index),
    };
    let response = build_response_envelope(
        request,
        request_record,
        kind,
        body,
        outcome_ts_ms,
        outcome.clone(),
    );
    let terminal = write_turn_end(
        writer,
        &response,
        capped,
        Some((&session_id, turn_index)),
        is_interrupted,
    );

    if is_interrupted {
        let _ = terminal;
        return Err(terminal_error.expect("interrupted turns retain their signal error"));
    }
    terminal?;
    if persistence_failure {
        return Err(terminal_error.expect("session persistence failure is an error outcome"));
    }
    if response.kind == MessageKind::Error {
        return Err(delivered_turn_failure(&response));
    }
    Ok(())
}

fn run_streaming_turn<T: Transport, W: Write>(
    transport: &ToolLoop<T>,
    history: &[Message],
    sink: &mut dyn EventSink,
    session: Option<(&str, u64)>,
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
) -> TransportCall<TurnOutcome> {
    let counters = StreamRoundCounters::default();
    let writer_for_tools = Rc::clone(writer);
    let mut observe_tool = |event: OwnedToolEvent| {
        let trail_event = event.as_tool_event();
        emit(
            sink,
            &ExchangeEvent::from_tool_event(now_ms(), trail_event, session),
        );
        write_tool_event(&writer_for_tools, &counters, event)
    };
    let writer_for_text = Rc::clone(writer);
    let mut observe_stream = |event| write_text_delta(&writer_for_text, &counters, event);
    transport.run_streaming_observed_fallible_with_attempts(
        history,
        &mut observe_tool,
        &mut observe_stream,
    )
}

#[derive(Default)]
struct StreamRoundCounters {
    provider_round: Cell<u64>,
    tool_round: Cell<u64>,
}

fn write_tool_event<W: Write>(
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
    counters: &StreamRoundCounters,
    event: OwnedToolEvent,
) -> Result<()> {
    match event {
        OwnedToolEvent::Round { content } => {
            let round_index = counters.provider_round.get();
            counters.tool_round.set(round_index);
            counters.provider_round.set(round_index.saturating_add(1));
            writer.borrow_mut().write_event(
                "tool_round",
                json!({"round_index": round_index, "content": content}),
            )
        }
        OwnedToolEvent::Call { id, name, input } => writer.borrow_mut().write_event(
            "tool_call",
            json!({
                "round_index": counters.tool_round.get(),
                "tool_use_id": id,
                "tool_name": name,
                "input": input,
            }),
        ),
        OwnedToolEvent::Result {
            id,
            name,
            output,
            status,
        } => {
            let fields = json!({
                "round_index": counters.tool_round.get(),
                "tool_use_id": id,
                "tool_name": name,
                "status": status,
                "output": output,
            });
            if status == crate::events::ToolStatus::Failed && interrupt::error().is_some() {
                writer
                    .borrow_mut()
                    .write_event_best_effort("tool_result", fields)
            } else {
                writer.borrow_mut().write_event("tool_result", fields)
            }
        }
    }
}

fn write_text_delta<W: Write>(
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
    counters: &StreamRoundCounters,
    event: StreamEvent,
) -> Result<()> {
    let Some((block_index, text)) = stream_delta_text(&event) else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    writer.borrow_mut().write_event(
        "text_delta",
        json!({
            "round_index": counters.provider_round.get(),
            "block_index": block_index,
            "text": text,
        }),
    )
}

fn stream_delta_text(event: &StreamEvent) -> Option<(u64, &str)> {
    let StreamEvent::ContentBlockDelta { index, delta } = event else {
        return None;
    };
    let delta_type = delta.get("type").and_then(Value::as_str);
    let text = match delta_type {
        Some("text_delta") => delta.get("text").and_then(Value::as_str),
        Some("response.output_text.delta" | "response.refusal.delta") => {
            delta.get("delta").and_then(Value::as_str)
        }
        _ => delta
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .or_else(|| {
                delta
                    .pointer("/choices/0/delta/refusal")
                    .and_then(Value::as_str)
            }),
    }?;
    Some((*index as u64, text))
}

fn write_turn_start<W: Write>(
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
    request: &MessageEnvelope,
    meta: &ExchangeMeta,
    provider: Provider,
    session: Option<(&str, u64)>,
) -> Result<()> {
    let mut fields = Map::new();
    fields.insert(
        "request".to_string(),
        serde_json::to_value(request).expect("request serializes"),
    );
    fields.insert(
        "provider".to_string(),
        Value::String(provider_name(provider).to_string()),
    );
    fields.insert("model".to_string(), Value::String(meta.model.clone()));
    insert_session_coordinates(&mut fields, session);
    writer
        .borrow_mut()
        .write_event("turn_start", Value::Object(fields))
}

fn write_turn_end<W: Write>(
    writer: &Rc<RefCell<StreamJsonWriter<W>>>,
    response: &MessageEnvelope,
    capped: bool,
    session: Option<(&str, u64)>,
    interrupted: bool,
) -> Result<()> {
    let mut fields = Map::new();
    fields.insert(
        "response".to_string(),
        serde_json::to_value(response).expect("response serializes"),
    );
    fields.insert("capped".to_string(), Value::Bool(capped));
    insert_session_coordinates(&mut fields, session);
    if interrupted {
        writer
            .borrow_mut()
            .write_event_best_effort("turn_end", Value::Object(fields))
    } else {
        writer
            .borrow_mut()
            .write_event("turn_end", Value::Object(fields))
    }
}

fn insert_session_coordinates(fields: &mut Map<String, Value>, session: Option<(&str, u64)>) {
    if let Some((session_id, turn_index)) = session {
        fields.insert(
            "session_id".to_string(),
            Value::String(session_id.to_string()),
        );
        fields.insert("turn_index".to_string(), Value::from(turn_index));
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::OpenAiChatCompletions => "openai-chat-completions",
        Provider::OpenAiResponses => "openai-responses",
    }
}

fn outcome_message(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok { reply, .. } => reply.clone(),
        Outcome::Error { message, .. } => message.clone(),
    }
}

struct StreamJsonWriter<W: Write> {
    output: W,
    seq: u64,
    failed: bool,
    ended: bool,
}

impl<W: Write> StreamJsonWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            seq: 0,
            failed: false,
            ended: false,
        }
    }

    fn write_event(&mut self, event: &str, fields: Value) -> Result<()> {
        self.write_record(event, fields, false)
    }

    fn write_event_best_effort(&mut self, event: &str, fields: Value) -> Result<()> {
        self.write_record(event, fields, true)
    }

    fn write_record(&mut self, event: &str, fields: Value, ignore_interrupt: bool) -> Result<()> {
        if self.failed {
            return Err(LegError::Io(
                "stream output is no longer writable".to_string(),
            ));
        }
        if self.ended {
            return Err(LegError::Io(
                "stream record attempted after turn_end".to_string(),
            ));
        }
        if !ignore_interrupt {
            interrupt::check()?;
        }
        let mut record = match fields {
            Value::Object(fields) => fields,
            _ => unreachable!("stream event fields must be an object"),
        };
        record.insert(
            "schema".to_string(),
            Value::String(STREAM_SCHEMA.to_string()),
        );
        record.insert("event".to_string(), Value::String(event.to_string()));
        record.insert("seq".to_string(), Value::from(self.seq));
        let mut bytes =
            serde_json::to_vec(&Value::Object(record)).expect("JSON values always serialize");
        bytes.push(b'\n');
        let result = self
            .output
            .write_all(&bytes)
            .and_then(|()| self.output.flush());
        if let Err(error) = result {
            self.failed = true;
            return Err(io_err(error));
        }
        self.seq = self.seq.saturating_add(1);
        self.ended = event == "turn_end";
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    use crate::model::AssistantReply;
    use crate::transport::Transport;

    struct BufferedTransport {
        reply: Result<AssistantReply>,
        calls: Rc<Cell<usize>>,
        histories: Rc<RefCell<Vec<Vec<Message>>>>,
    }

    impl Transport for BufferedTransport {
        fn send_conversation(&self, _messages: &[Message]) -> Result<AssistantReply> {
            self.calls.set(self.calls.get() + 1);
            self.histories.borrow_mut().push(_messages.to_vec());
            match &self.reply {
                Ok(reply) => Ok(reply.clone()),
                Err(error) => Err(LegError::Transport(error.to_string())),
            }
        }
    }

    struct FailingSink;

    impl EventSink for FailingSink {
        fn record(&mut self, _event: &ExchangeEvent) -> io::Result<()> {
            Err(io::Error::other("session disk full"))
        }
    }

    fn request(body: &str) -> MessageEnvelope {
        MessageEnvelope::new("m-1", "c-1", "user", "leg", MessageKind::Request, body, 1)
    }

    fn parse_records(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8(bytes.to_vec())
            .expect("UTF-8 output")
            .lines()
            .map(|line| serde_json::from_str(line).expect("NDJSON record"))
            .collect()
    }

    fn jsonl_code_block(source: &str) -> Vec<Value> {
        let mut inside = false;
        let mut records = Vec::new();
        for line in source.lines() {
            if line.trim() == "```jsonl" {
                inside = true;
                continue;
            }
            if inside && line.trim() == "```" {
                break;
            }
            if inside {
                records.push(serde_json::from_str(line).expect("JSONL example record"));
            }
        }
        assert!(!records.is_empty(), "source must include a jsonl block");
        records
    }

    fn assert_sequence(records: &[Value]) {
        for (seq, record) in records.iter().enumerate() {
            assert_eq!(record["schema"], STREAM_SCHEMA);
            assert_eq!(record["seq"], seq as u64);
            assert!(record["event"].is_string());
        }
    }

    #[test]
    fn buffered_transport_emits_start_and_authoritative_terminal_envelope() {
        let calls = Rc::new(Cell::new(0));
        let histories = Rc::new(RefCell::new(Vec::new()));
        let transport = ToolLoop::new(
            BufferedTransport {
                reply: Ok(AssistantReply::new("hello")),
                calls: Rc::clone(&calls),
                histories: Rc::clone(&histories),
            },
            ToolRegistry::new(),
            None,
        );
        let mut bytes = Vec::new();
        let writer = Rc::new(RefCell::new(StreamJsonWriter::new(&mut bytes)));
        let mut sink = NoopSink;
        let (request, _) = parse_exchange_request("line one\nline two\n");
        execute_cold_exchange_core(
            &transport,
            &ExchangeMeta {
                model: "test-model".to_string(),
                base_url: "https://provider.test".to_string(),
            },
            Provider::Anthropic,
            &request,
            &mut sink,
            &writer,
        )
        .expect("exchange succeeds");
        drop(writer);

        let records = parse_records(&bytes);
        assert_eq!(calls.get(), 1);
        assert_eq!(
            histories.borrow()[0][0].content,
            [ContentBlock::text("line one\nline two")]
        );
        assert_eq!(records.len(), 2, "buffered transport has no text deltas");
        assert_eq!(records[0]["schema"], STREAM_SCHEMA);
        assert_eq!(records[0]["event"], "turn_start");
        assert_eq!(records[0]["seq"], 0);
        assert_eq!(records[0]["request"]["body"], "line one\nline two");
        assert_eq!(records[0]["provider"], "anthropic");
        assert_eq!(records[1]["event"], "turn_end");
        assert_eq!(records[1]["seq"], 1);
        assert_eq!(records[1]["capped"], false);
        assert_eq!(records[1]["response"]["body"], "hello");
        assert_eq!(
            records[1]["response"]["in_reply_to"],
            records[0]["request"]["message_id"]
        );
    }

    #[test]
    fn provider_failure_emits_one_error_terminal_record() {
        let calls = Rc::new(Cell::new(0));
        let transport = ToolLoop::new(
            BufferedTransport {
                reply: Err(LegError::Auth("bad key".to_string())),
                calls: Rc::clone(&calls),
                histories: Rc::new(RefCell::new(Vec::new())),
            },
            ToolRegistry::new(),
            None,
        );
        let mut bytes = Vec::new();
        let writer = Rc::new(RefCell::new(StreamJsonWriter::new(&mut bytes)));
        let mut sink = NoopSink;
        let error = execute_cold_exchange_core(
            &transport,
            &ExchangeMeta {
                model: "test-model".to_string(),
                base_url: "https://provider.test".to_string(),
            },
            Provider::Anthropic,
            &request("hello"),
            &mut sink,
            &writer,
        )
        .unwrap_err();
        drop(writer);

        let records = parse_records(&bytes);
        assert_eq!(calls.get(), 1);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "turn_start");
        assert_eq!(records[1]["event"], "turn_end");
        assert_eq!(records[1]["response"]["kind"], "error");
        assert_eq!(
            records[1]["response"]["exchange"]["exchange"]["outcome"]["event"],
            "response_error"
        );
        assert_eq!(error.kind(), "turn_failure");
    }

    #[test]
    fn writer_refuses_records_after_turn_end() {
        let mut bytes = Vec::new();
        let mut writer = StreamJsonWriter::new(&mut bytes);
        writer
            .write_event("turn_end", json!({"capped": false}))
            .unwrap();
        assert!(
            writer
                .write_event("text_delta", json!({"text": "late"}))
                .is_err()
        );
    }

    #[test]
    fn readme_contract_and_help_examples_are_parsable_ndjson() {
        assert_sequence(&jsonl_code_block(include_str!("../../README.md")));
        assert_sequence(&jsonl_code_block(include_str!(
            "../../docs/exchange-stream.md"
        )));

        let help = help_text();
        let examples = help
            .split_once("Example NDJSON:\n")
            .expect("help has example heading")
            .1
            .lines()
            .take_while(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).expect("help example record"))
            .collect::<Vec<Value>>();
        assert_sequence(&examples);
    }

    #[test]
    fn failed_required_session_recording_is_an_error_terminal_outcome() {
        let calls = Rc::new(Cell::new(0));
        let transport = ToolLoop::new(
            BufferedTransport {
                reply: Ok(AssistantReply::new("not committed")),
                calls: Rc::clone(&calls),
                histories: Rc::new(RefCell::new(Vec::new())),
            },
            ToolRegistry::new(),
            None,
        );
        let session_write_error = Rc::new(RefCell::new(None));
        let mut sink: Box<dyn EventSink> = Box::new(CompositeEventSink {
            session: Box::new(FailingSink),
            event_log: Box::new(NoopSink),
            session_write_error: Rc::clone(&session_write_error),
        });
        let mut resumed = ResumedSession {
            session_id: "sess-test".to_string(),
            conversation: Conversation::new(),
            prior_turns: 0,
            next_turn_index: 0,
        };
        let mut bytes = Vec::new();
        let writer = Rc::new(RefCell::new(StreamJsonWriter::new(&mut bytes)));
        let error = execute_session_exchange_core(
            &transport,
            &ExchangeMeta {
                model: "test-model".to_string(),
                base_url: "https://provider.test".to_string(),
            },
            Provider::Anthropic,
            &request("hello"),
            sink.as_mut(),
            &mut resumed,
            session_write_error,
            &writer,
        )
        .unwrap_err();
        drop(writer);

        let records = parse_records(&bytes);
        assert_eq!(calls.get(), 1);
        assert_eq!(records[0]["session_id"], "sess-test");
        assert_eq!(records[1]["event"], "turn_end");
        assert_eq!(records[1]["response"]["kind"], "error");
        assert_eq!(
            records[1]["response"]["body"],
            "io error: failed to record session trail: session disk full"
        );
        assert_eq!(
            records[1]["response"]["exchange"]["exchange"]["outcome"]["event"],
            "response_error"
        );
        assert!(resumed.conversation.messages().is_empty());
        assert!(matches!(error, LegError::Io(_)));
    }
}
