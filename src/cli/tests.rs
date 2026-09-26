use super::*;
use crate::model::AssistantReply;
use crate::transport::Transport;

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn config_with_bash_timeout(timeout_secs: u64) -> LegConfig {
    LegConfig::from_lookup(|key| match key {
        "ANTHROPIC_API_KEY" => Some("test-key".to_string()),
        "LEG_BASH_TIMEOUT_SECS" => Some(timeout_secs.to_string()),
        _ => None,
    })
    .expect("config loads")
}

fn bash_available() -> bool {
    let mut probe = std::process::Command::new("bash");
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH") {
        probe.env("PATH", path);
    }
    probe
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct FakeTransport(std::result::Result<AssistantReply, LegError>);

impl Transport for FakeTransport {
    fn send_conversation(&self, _messages: &[crate::model::Message]) -> Result<AssistantReply> {
        match &self.0 {
            Ok(reply) => Ok(reply.clone()),
            Err(LegError::Auth(msg)) => Err(LegError::Auth(msg.clone())),
            Err(LegError::Server {
                status,
                error_type,
                message,
            }) => Err(LegError::Server {
                status: *status,
                error_type: error_type.clone(),
                message: message.clone(),
            }),
            Err(other) => Err(LegError::Transport(other.to_string())),
        }
    }
}

/// A [`Transport`] that records every call's full message history and
/// answers from a queue of canned replies, in order — lets a test assert
/// exactly what history a later turn sent, not just its printed reply.
struct CapturingTransport {
    replies: std::cell::RefCell<std::collections::VecDeque<AssistantReply>>,
    calls: std::cell::RefCell<Vec<Vec<crate::model::Message>>>,
}

impl CapturingTransport {
    fn new(replies: Vec<AssistantReply>) -> Self {
        Self {
            replies: std::cell::RefCell::new(replies.into()),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl Transport for CapturingTransport {
    fn send_conversation(&self, messages: &[crate::model::Message]) -> Result<AssistantReply> {
        self.calls.borrow_mut().push(messages.to_vec());
        Ok(self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("test queued enough replies for every expected call"))
    }
}

/// Wraps a test transport in a tool loop with no registered tools.
fn looped<T: Transport>(transport: T) -> ToolLoop<T> {
    ToolLoop::new(transport, ToolRegistry::new(), None)
}

fn meta() -> ExchangeMeta {
    ExchangeMeta {
        model: "claude-test-model".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
    }
}

#[test]
fn run_ask_prints_only_reply_text_on_success() {
    let participant =
        LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
    let mut buf = Vec::new();
    let mut sink = NoopSink;
    run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
        .expect("infallible per Participant contract");
    assert_eq!(String::from_utf8(buf).unwrap(), "hi there\n");
}

#[test]
fn run_ask_propagates_delivery_failure_without_writing_stdout() {
    let participant = LocalParticipant::new(
        FakeTransport(Err(LegError::Server {
            status: 503,
            error_type: Some("api_error".to_string()),
            message: "overloaded".to_string(),
        })),
        meta(),
    );
    let mut buf = Vec::new();
    let mut sink = NoopSink;
    let err = run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
        .expect_err("delivered errors must fail the command");
    assert_eq!(err.kind(), "turn_failure");
    assert_eq!(
        err.to_string(),
        "turn failed (kind: error): provider server error (503, api_error): overloaded"
    );
    assert!(buf.is_empty(), "failed ask must leave stdout empty");
}

#[test]
fn run_ask_emits_request_and_response_ok_events_to_the_sink() {
    let participant =
        LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
    let mut buf = Vec::new();
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
            .expect("infallible per Participant contract");
    }
    let text = String::from_utf8(trail).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["event"], "request");
    assert_eq!(first["prompt"], "hello");
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["event"], "response_ok");
    assert_eq!(second["reply"], "hi there");
}

#[test]
fn run_ask_with_image_content_sends_and_records_the_full_user_turn() {
    let content = vec![
        ContentBlock::Image {
            source: crate::model::ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "iVBORw==".to_string(),
            },
        },
        ContentBlock::text("describe"),
    ];
    let transport = CapturingTransport::new(vec![AssistantReply::new("answer")]);
    let participant = LocalParticipant::new(looped(&transport), meta());
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        run_ask_with_content(
            &participant,
            &meta(),
            "describe",
            &content,
            Vec::new(),
            &mut sink,
        )
        .expect("ask completes");
    }

    assert_eq!(
        transport.calls.borrow()[0],
        vec![Message::new(Role::User, content.clone())]
    );
    let report = crate::log::parse_jsonl(std::io::Cursor::new(trail)).expect("parses trail");
    assert_eq!(report.exchanges[0].request.prompt, "describe");
    assert_eq!(report.exchanges[0].request.content, Some(content));
}

/// A [`Transport`] that panics if called — proves `run_ask` records the
/// `request` event *before* invoking the provider (must be able to
/// observe a request line even if the call that follows never returns),
/// matching [`ExchangeEvent::Request`]'s documented ordering contract.
struct PanicTransport;

impl Transport for PanicTransport {
    fn send_conversation(&self, _messages: &[crate::model::Message]) -> Result<AssistantReply> {
        panic!("run_ask must not have called the transport yet");
    }
}

/// A sink that panics on its first `record` call whose event is not
/// `request` — proves the request line is the *first* thing recorded,
/// i.e. emitted before the provider call runs (which, for
/// [`PanicTransport`], never returns at all).
struct RequestFirstSink {
    seen_request: bool,
}

impl EventSink for RequestFirstSink {
    fn record(&mut self, event: &ExchangeEvent) -> std::io::Result<()> {
        match event {
            ExchangeEvent::Request { .. } if !self.seen_request => {
                self.seen_request = true;
                Ok(())
            }
            ExchangeEvent::Request { .. } => panic!("request recorded more than once"),
            _ => {
                assert!(self.seen_request, "outcome recorded before request");
                Ok(())
            }
        }
    }
}

#[test]
fn run_ask_records_the_request_event_before_invoking_the_transport() {
    // PanicTransport aborts the test the moment `run_ask` reaches the
    // provider call, so simply completing this call proves the request
    // line was recorded first (and RequestFirstSink additionally asserts
    // ordering for any outcome recorded, on the success path elsewhere).
    let participant = LocalParticipant::new(PanicTransport, meta());
    let mut buf = Vec::new();
    let mut sink = RequestFirstSink {
        seen_request: false,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
    }));
    assert!(
        result.is_err(),
        "PanicTransport should have been reached and panicked"
    );
    assert!(sink.seen_request, "request event must be recorded first");
}

/// Exercises the actual `run_ask` path (not just the `ExchangeEvent`
/// constructors) through a real [`WriterSink`], asserting the emitted
/// lines' field names/order/omissions match baton's own
/// `parses_valid_two_line_exchange` fixture (`baton/src/log.rs`) exactly
/// — everything except the wall-clock `ts_ms`/`duration_ms` values, which
/// neither `leg` nor baton makes deterministic in tests, so this
/// compares against a template built from the *actual* values `run_ask`
/// produced rather than fixed literals.
#[test]
fn run_ask_wire_lines_match_batons_field_order_and_omitted_fields() {
    let participant =
        LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
    let mut buf = Vec::new();
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
            .expect("infallible per Participant contract");
    }
    let text = String::from_utf8(trail).unwrap();
    let mut lines = text.lines();
    let request_line = lines.next().expect("request line");
    let response_line = lines.next().expect("response line");
    assert!(lines.next().is_none(), "exactly two lines for a plain ask");

    let request_value: serde_json::Value = serde_json::from_str(request_line).unwrap();
    let ts_ms = request_value["ts_ms"].as_u64().expect("ts_ms");
    assert_eq!(
        request_line,
        format!(
            r#"{{"event":"request","schema":"baton.exchange/v1","ts_ms":{ts_ms},"model":"claude-test-model","base_url":"https://api.anthropic.com","prompt":"hello"}}"#
        ),
    );

    let response_value: serde_json::Value = serde_json::from_str(response_line).unwrap();
    let r_ts_ms = response_value["ts_ms"].as_u64().expect("ts_ms");
    let duration_ms = response_value["duration_ms"].as_u64().expect("duration_ms");
    assert_eq!(
        response_line,
        format!(
            r#"{{"event":"response_ok","schema":"baton.exchange/v1","ts_ms":{r_ts_ms},"duration_ms":{duration_ms},"reply":"hi there"}}"#
        ),
    );
}

#[test]
fn no_arguments_is_a_no_op() {
    assert_eq!(parse_args(&argv(&[])).unwrap(), None);
}

#[test]
fn version_flags_parse() {
    assert_eq!(
        parse_args(&argv(&["--version"])).unwrap(),
        Some(Command::Version)
    );
    assert_eq!(parse_args(&argv(&["-V"])).unwrap(), Some(Command::Version));
}

#[test]
fn help_flags_parse() {
    assert_eq!(parse_args(&argv(&["--help"])).unwrap(), Some(Command::Help));
    assert_eq!(parse_args(&argv(&["-h"])).unwrap(), Some(Command::Help));
}

#[test]
fn help_text_documents_usage_env_and_failure_contract() {
    let text = help_text();
    assert!(text.contains("leg ask [--model <model>] [--image <path> ...] <prompt>"));
    assert!(text.contains("JPEG, PNG, GIF, and WebP"));
    assert!(text.contains("ANTHROPIC_API_KEY"));
    assert!(text.contains("ANTHROPIC_AUTH_TOKEN is for bearer keys"));
    assert!(text.contains("Anthropic-compatible endpoints"));
    assert!(text.contains("Claude subscription OAuth tokens"));
    assert!(text.contains("`claude setup-token`"));
    assert!(text.contains("unsupported outside Claude Code"));
    assert!(text.contains("LEG_MODEL"));
    assert!(text.contains("LEG_BASH_TIMEOUT_SECS"));
    assert!(text.contains("LEG_MAX_TOOL_ROUNDS"));
    assert!(text.contains("LEG_EVENT_LOG"));
    assert!(text.contains("`ask`, cold `exchange`"));
    assert!(text.contains("named exchange sessions always write their"));
    assert!(text.contains("headless entry point"));
    assert!(text.contains("exit non-zero"));
    assert!(text.contains("kind:\"error\""));
    assert!(text.contains("--session <id>"));
    assert!(text.contains("--new-session"));
    assert!(text.contains("--session-id-out"));
    assert!(text.contains("XDG_STATE_HOME/leg/sessions"));
}

#[test]
fn ask_parses_positional_prompt() {
    assert_eq!(
        parse_args(&argv(&["ask", "hello"])).unwrap(),
        Some(Command::Ask {
            prompt: "hello".to_string(),
            model: None,
            images: vec![],
        })
    );
}

#[test]
fn ask_parses_model_override_before_or_after_prompt() {
    assert_eq!(
        parse_args(&argv(&["ask", "--model", "claude-opus-4-8", "hello"])).unwrap(),
        Some(Command::Ask {
            prompt: "hello".to_string(),
            model: Some("claude-opus-4-8".to_string()),
            images: vec![],
        })
    );
    assert_eq!(
        parse_args(&argv(&["ask", "hello", "--model", "claude-opus-4-8"])).unwrap(),
        Some(Command::Ask {
            prompt: "hello".to_string(),
            model: Some("claude-opus-4-8".to_string()),
            images: vec![],
        })
    );
}

#[test]
fn ask_parses_repeatable_images_before_and_after_the_prompt() {
    assert_eq!(
        parse_args(&argv(&[
            "ask",
            "--image",
            "first.png",
            "describe",
            "--image",
            "second.jpg"
        ]))
        .unwrap(),
        Some(Command::Ask {
            prompt: "describe".to_string(),
            model: None,
            images: vec!["first.png".to_string(), "second.jpg".to_string()],
        })
    );
}

#[test]
fn ask_without_prompt_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["ask"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn ask_with_blank_prompt_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["ask", "   "])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn ask_with_extra_positional_argument_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["ask", "hello", "extra"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn model_flag_without_value_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["ask", "--model"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn image_flag_without_path_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["ask", "hello", "--image"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn unrecognised_argument_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["bogus"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn session_bare_parses_with_no_resume() {
    assert_eq!(
        parse_args(&argv(&["session"])).unwrap(),
        Some(Command::Session { resume: None })
    );
}

#[test]
fn session_resume_parses_file_and_optional_session_id() {
    assert_eq!(
        parse_args(&argv(&["session", "--resume", "/tmp/x.jsonl"])).unwrap(),
        Some(Command::Session {
            resume: Some(ResumeArgs {
                file: "/tmp/x.jsonl".to_string(),
                session_id: None,
            })
        })
    );
    assert_eq!(
        parse_args(&argv(&[
            "session",
            "--resume",
            "/tmp/x.jsonl",
            "--session",
            "sess-1"
        ]))
        .unwrap(),
        Some(Command::Session {
            resume: Some(ResumeArgs {
                file: "/tmp/x.jsonl".to_string(),
                session_id: Some("sess-1".to_string()),
            })
        })
    );
}

#[test]
fn session_without_resume_rejects_bare_session_flag() {
    assert!(matches!(
        parse_args(&argv(&["session", "--session", "sess-1"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn log_show_parses_optional_file() {
    assert_eq!(
        parse_args(&argv(&["log", "show"])).unwrap(),
        Some(Command::LogShow { file: None })
    );
    assert_eq!(
        parse_args(&argv(&["log", "show", "--file", "/tmp/x.jsonl"])).unwrap(),
        Some(Command::LogShow {
            file: Some("/tmp/x.jsonl".to_string())
        })
    );
}

#[test]
fn log_replay_parses_optional_file_and_index() {
    assert_eq!(
        parse_args(&argv(&["log", "replay"])).unwrap(),
        Some(Command::LogReplay {
            file: None,
            index: None
        })
    );
    assert_eq!(
        parse_args(&argv(&[
            "log",
            "replay",
            "--index",
            "3",
            "--file",
            "/tmp/x.jsonl"
        ]))
        .unwrap(),
        Some(Command::LogReplay {
            file: Some("/tmp/x.jsonl".to_string()),
            index: Some(3),
        })
    );
}

#[test]
fn log_replay_rejects_zero_index() {
    assert!(matches!(
        parse_args(&argv(&["log", "replay", "--index", "0"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn log_show_rejects_index_flag() {
    assert!(matches!(
        parse_args(&argv(&["log", "show", "--index", "1"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn log_without_subcommand_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["log"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn log_unknown_subcommand_is_usage_error() {
    assert!(matches!(
        parse_args(&argv(&["log", "bogus"])).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn execute_ask_propagates_config_error_without_calling_the_provider() {
    // SAFETY: this crate's tests run single-threaded w.r.t. process env
    // mutation is avoided entirely here — no ANTHROPIC_* var is read
    // through `execute_ask`'s `LegConfig::from_env`, which fails closed
    // when unset in this test process's environment. If some other test
    // or the outer environment happens to export a credential, this test
    // is skipped rather than flaking on shared state.
    if std::env::var("ANTHROPIC_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok()
        || std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok()
    {
        return;
    }
    let mut buf = Vec::new();
    let err = execute_ask("hello", None, &mut buf).unwrap_err();
    assert!(matches!(err, LegError::Config(_)));
    assert!(buf.is_empty());
}

#[test]
fn open_append_sink_on_an_unopenable_path_falls_back_to_noop_without_erroring() {
    // A directory can never be opened as a file for appending — a stand-in
    // for any real-world open failure (bad permissions, missing parent).
    // Recording is additive: this must not propagate an error, only warn.
    let sink = open_append_sink(std::env::temp_dir().to_str().expect("utf8 path"));
    let mut sink = sink;
    let event = ExchangeEvent::request(1, &meta(), "hello");
    sink.record(&event).expect("NoopSink fallback never fails");
}

/// A network-free [`crate::transport::http::HttpClient`] fake that
/// captures the JSON body of the last request it served, via a shared
/// handle a test retains after the fake is moved into a [`ClaudeClient`].
struct RecordingHttp {
    captured_body: std::rc::Rc<std::cell::RefCell<Option<String>>>,
}

impl crate::transport::http::HttpClient for RecordingHttp {
    fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        body: &str,
    ) -> Result<crate::transport::http::HttpResponse> {
        *self.captured_body.borrow_mut() = Some(body.to_string());
        Ok(crate::transport::http::HttpResponse {
            status: 200,
            body: r#"{"content":[{"type":"text","text":"hi"}]}"#.to_string(),
        })
    }
}

/// End-to-end (network-free): `--model` reaches `execute_ask`'s config
/// override, which is stamped onto the outgoing Claude Messages request.
#[test]
fn model_override_reaches_the_outgoing_claude_request() {
    let mut config =
        LegConfig::from_lookup(|key| (key == "ANTHROPIC_API_KEY").then(|| "secret".to_string()))
            .expect("config loads");
    apply_model_override(&mut config, Some("claude-opus-4-8".to_string()));
    let meta = ExchangeMeta {
        model: config.model.clone(),
        base_url: config.base_url.clone(),
    };

    let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
    let http = RecordingHttp {
        captured_body: std::rc::Rc::clone(&captured),
    };
    let client = ClaudeClient::with_http(config, http);
    let participant = LocalParticipant::new(client, meta.clone());

    let mut buf = Vec::new();
    let mut sink = NoopSink;
    run_ask(&participant, &meta, "hello", &mut buf, &mut sink)
        .expect("infallible per Participant contract");

    let sent = captured.borrow().clone().expect("request body captured");
    let value: serde_json::Value = serde_json::from_str(&sent).expect("valid json");
    assert_eq!(value["model"], "claude-opus-4-8");
}

#[test]
fn configured_bash_timeout_flows_through_registry_to_spec_and_handler() {
    if !bash_available() {
        return;
    }

    let default_timeout_secs = if cfg!(windows) {
        let probe_registry = build_tool_registry(&config_with_bash_timeout(1));
        let probe_result = probe_registry.dispatch(
            "toolu_probe",
            "bash",
            &serde_json::json!({"command": "true", "timeout": 60}),
        );
        let ContentBlock::ToolResult {
            content,
            is_error: None,
            ..
        } = probe_result
        else {
            panic!("bash startup probe failed");
        };
        let probe: serde_json::Value = serde_json::from_str(&content).expect("probe returns JSON");
        assert_eq!(probe["status"], "exited");
        probe["wall_time_seconds"].as_f64().unwrap().ceil() as u64 + 3
    } else {
        1
    };
    let config = config_with_bash_timeout(default_timeout_secs);
    let registry = build_tool_registry(&config);
    let bash_spec = registry
        .specs()
        .into_iter()
        .find(|spec| spec.name == "bash")
        .expect("bash spec is registered");

    assert_eq!(
        bash_spec.input_schema["properties"]["timeout"]["default"],
        default_timeout_secs
    );
    let timeout_unit = if default_timeout_secs == 1 {
        "second"
    } else {
        "seconds"
    };
    assert!(
        bash_spec
            .description
            .contains(&format!("{default_timeout_secs} {timeout_unit} by default"))
    );
    assert_eq!(
        bash_spec.input_schema["properties"]["timeout"]["description"],
        format!("Maximum run time in seconds (default {default_timeout_secs})")
    );

    let command = format!("sleep {}", default_timeout_secs + 1);
    let result = registry.dispatch(
        "toolu_bash_timeout",
        "bash",
        &serde_json::json!({"command": command}),
    );
    let ContentBlock::ToolResult {
        content,
        is_error: None,
        ..
    } = result
    else {
        panic!("bash timeout call failed");
    };
    let output: serde_json::Value = serde_json::from_str(&content).expect("bash returns JSON");
    assert_eq!(output["status"], "timed_out");
    assert_eq!(output["exit_code"], 124);
}

#[test]
fn session_repl_accumulates_history_and_prints_replies() {
    let transport = FakeTransport(Ok(AssistantReply::new("reply-1")));
    let mut output = Vec::new();
    let mut warning = Vec::new();
    let mut sink = NoopSink;
    run_session_repl_with_warning(
        &looped(&transport),
        &mut sink,
        &meta(),
        std::io::Cursor::new(b"hello\n".to_vec()),
        &mut output,
        "sess-1".to_string(),
        Conversation::new(),
        0,
        &mut warning,
    )
    .expect("session loop completes");
    assert_eq!(String::from_utf8(output).unwrap(), "reply-1\n");
}

#[test]
fn session_repl_ignores_blank_lines_and_exits_on_exit_command() {
    let transport = FakeTransport(Ok(AssistantReply::new("reply-1")));
    let mut output = Vec::new();
    let mut warning = Vec::new();
    let mut sink = NoopSink;
    run_session_repl_with_warning(
        &looped(&transport),
        &mut sink,
        &meta(),
        std::io::Cursor::new(b"\nhello\n\n/exit\nnever sent\n".to_vec()),
        &mut output,
        "sess-1".to_string(),
        Conversation::new(),
        0,
        &mut warning,
    )
    .expect("session loop completes");
    assert_eq!(String::from_utf8(output).unwrap(), "reply-1\n");
}

#[test]
fn session_repl_rolls_back_failed_turn_and_continues() {
    let transport = FakeTransport(Err(LegError::Auth("bad credentials".to_string())));
    let mut output = Vec::new();
    let mut warning = Vec::new();
    let mut sink = NoopSink;
    run_session_repl_with_warning(
        &looped(&transport),
        &mut sink,
        &meta(),
        std::io::Cursor::new(b"hello\n".to_vec()),
        &mut output,
        "sess-1".to_string(),
        Conversation::new(),
        0,
        &mut warning,
    )
    .expect("session loop reports the error and continues");
    assert!(String::from_utf8(output).unwrap().is_empty());
}

#[test]
fn session_repl_emits_session_start_turn_and_end_events() {
    let transport = FakeTransport(Ok(AssistantReply::new("reply-1")));
    let mut output = Vec::new();
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        execute_session(
            &looped(&transport),
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"hello\n".to_vec()),
            &mut output,
            "sess-1".to_string(),
        )
        .expect("session completes");
    }
    let text = String::from_utf8(trail).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    let events: Vec<serde_json::Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events[0]["event"], "session_start");
    assert_eq!(events[1]["event"], "request");
    assert_eq!(events[1]["session_id"], "sess-1");
    assert_eq!(events[1]["turn_index"], 0);
    assert_eq!(events[2]["event"], "response_ok");
    assert_eq!(events[3]["event"], "session_end");
    assert_eq!(events[3]["turns"], 1);
}

#[test]
fn session_repl_sends_full_prior_history_on_a_later_turn() {
    let transport = CapturingTransport::new(vec![
        AssistantReply::new("reply-1"),
        AssistantReply::new("reply-2"),
    ]);
    let mut output = Vec::new();
    let mut warning = Vec::new();
    let mut sink = NoopSink;
    run_session_repl_with_warning(
        &looped(&transport),
        &mut sink,
        &meta(),
        std::io::Cursor::new(b"turn one\nturn two\n".to_vec()),
        &mut output,
        "sess-1".to_string(),
        Conversation::new(),
        0,
        &mut warning,
    )
    .expect("session loop completes");

    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], vec![crate::model::Message::user("turn one")]);
    assert_eq!(
        calls[1],
        vec![
            crate::model::Message::user("turn one"),
            crate::model::Message::assistant("reply-1"),
            crate::model::Message::user("turn two"),
        ],
        "the second call must carry the full prior history, not just the new turn"
    );
    assert_eq!(String::from_utf8(output).unwrap(), "reply-1\nreply-2\n");
}

/// End-to-end resume cycle: a first session run writes a real
/// [`WriterSink`] trail; that exact trail is re-parsed with
/// [`crate::log::parse_sessions`] and rehydrated with
/// [`select_and_rehydrate`] (no manually constructed [`SessionRecord`]);
/// the resumed run's next provider call must then carry the rehydrated
/// history plus the new turn, and its own trail must continue the same
/// `session_id` at the next `turn_index` with no fresh `session_start`.
#[test]
fn session_resume_round_trips_history_and_turn_index_through_a_real_trail() {
    let first_transport = CapturingTransport::new(vec![AssistantReply::new("reply-1")]);
    let mut first_output = Vec::new();
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        execute_session(
            &looped(&first_transport),
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"hello\n".to_vec()),
            &mut first_output,
            "sess-1".to_string(),
        )
        .expect("first session run completes");
    }

    let report = crate::log::parse_sessions(std::io::Cursor::new(trail.clone())).expect("parses");
    let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates");
    assert_eq!(resumed.session_id, "sess-1");
    assert_eq!(resumed.next_turn_index, 1);
    assert_eq!(
        resumed.conversation.messages(),
        &[
            crate::model::Message::user("hello"),
            crate::model::Message::assistant("reply-1"),
        ]
    );

    let second_transport = CapturingTransport::new(vec![AssistantReply::new("reply-2")]);
    let mut second_output = Vec::new();
    let mut resumed_trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut resumed_trail);
        execute_session_resumed(
            &looped(&second_transport),
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"again\n".to_vec()),
            &mut second_output,
            resumed,
        )
        .expect("resumed session run completes");
    }

    let calls = second_transport.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        vec![
            crate::model::Message::user("hello"),
            crate::model::Message::assistant("reply-1"),
            crate::model::Message::user("again"),
        ],
        "the resumed call must carry the rehydrated history plus the new turn"
    );
    assert_eq!(String::from_utf8(second_output).unwrap(), "reply-2\n");

    let resumed_text = String::from_utf8(resumed_trail).unwrap();
    let resumed_lines: Vec<&str> = resumed_text.lines().collect();
    assert_eq!(
        resumed_lines.len(),
        3,
        "resuming emits no fresh session_start: {resumed_text}"
    );
    let request_event: serde_json::Value = serde_json::from_str(resumed_lines[0]).unwrap();
    assert_eq!(request_event["event"], "request");
    assert_eq!(request_event["session_id"], "sess-1");
    assert_eq!(request_event["turn_index"], 1);
    let end_event: serde_json::Value = serde_json::from_str(resumed_lines[2]).unwrap();
    assert_eq!(end_event["event"], "session_end");
    assert_eq!(end_event["session_id"], "sess-1");
    assert_eq!(end_event["turns"], 2);
}

fn tool_use_block() -> ContentBlock {
    ContentBlock::ToolUse {
        id: "toolu_1".to_string(),
        name: "read".to_string(),
        input: serde_json::json!({"path": "a.txt"}),
    }
}

fn tool_result_block() -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: "toolu_1".to_string(),
        content: "hello".to_string(),
        is_error: None,
    }
}

#[test]
fn rehydrate_denied_tool_result_as_an_error() {
    let error = "denied by pre-tool hook: role policy".to_string();
    let turn = crate::log::SessionTurn {
        request: crate::events::RequestRecord {
            ts_ms: 0,
            model: String::new(),
            base_url: String::new(),
            prompt: String::new(),
            content: None,
            session_id: Some("sess-1".to_string()),
            turn_index: Some(0),
        },
        rounds: Vec::new(),
        tools: vec![crate::log::ToolPair {
            call: crate::events::ToolCallRecord {
                ts_ms: 1,
                tool_use_id: "toolu_1".to_string(),
                tool_name: "bash".to_string(),
                input: serde_json::json!({}),
            },
            result: Some(crate::events::ToolResultRecord {
                ts_ms: 2,
                tool_use_id: "toolu_1".to_string(),
                tool_name: "bash".to_string(),
                status: ToolStatus::Denied,
                result: None,
                error: Some(error.clone()),
            }),
        }],
        outcome: None,
    };

    assert_eq!(
        rehydrate_tool_result(&turn, "toolu_1"),
        Some(ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            content: error,
            is_error: Some(true),
        })
    );
}

/// Runs one REPL turn per line of `input` against `transport`, returning
/// the trail and the warning stream.
fn run_repl(transport: &impl Transport, input: &[u8]) -> (String, String) {
    let (trail, warning, _) = run_repl_with(&looped(transport), input);
    (trail, warning)
}

/// Runs one REPL turn per line of `input` through `tool_loop`, returning
/// the trail, the warning stream, and stdout.
fn run_repl_with(tool_loop: &ToolLoop<impl Transport>, input: &[u8]) -> (String, String, String) {
    let mut trail = Vec::new();
    let mut warning = Vec::new();
    let mut output = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        run_session_repl_with_warning(
            tool_loop,
            &mut sink,
            &meta(),
            std::io::Cursor::new(input.to_vec()),
            &mut output,
            "sess-1".to_string(),
            Conversation::new(),
            0,
            &mut warning,
        )
        .expect("session loop completes");
    }
    (
        String::from_utf8(trail).unwrap(),
        String::from_utf8(warning).unwrap(),
        String::from_utf8(output).unwrap(),
    )
}

/// A stub `echo` tool loop over `transport`, counting handler calls.
fn echo_looped<T: Transport>(transport: T) -> (ToolLoop<T>, std::rc::Rc<std::cell::Cell<usize>>) {
    let count = std::rc::Rc::new(std::cell::Cell::new(0));
    let registry = crate::tools::tests::echo_registry(count.clone());
    (
        ToolLoop::new(transport, registry, Some(TEST_TOOL_ROUND_LIMIT)),
        count,
    )
}

const TEST_TOOL_ROUND_LIMIT: usize = 3;

fn echo_result(id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: "echo: hi".to_string(),
        is_error: None,
    }
}

#[test]
fn run_ask_iterates_tool_use_and_prints_only_the_final_text() {
    let transport = CapturingTransport::new(vec![
        crate::tools::tests::tool_use_reply("toolu_1", "echo"),
        AssistantReply::new("final answer"),
    ]);
    let (tool_loop, count) = echo_looped(&transport);
    let participant = LocalParticipant::new(tool_loop, meta());
    let mut buf = Vec::new();
    run_ask(&participant, &meta(), "hello", &mut buf, &mut NoopSink).expect("infallible");

    assert_eq!(String::from_utf8(buf).unwrap(), "final answer\n");
    assert_eq!(count.get(), 1);
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1][2],
        Message::new(Role::User, vec![echo_result("toolu_1")]),
        "the stub handler's output is fed back as tool_result"
    );
}

/// Records every event it is given, for trail-order assertions.
struct CollectingSink(std::rc::Rc<std::cell::RefCell<Vec<serde_json::Value>>>);

impl EventSink for CollectingSink {
    fn record(&mut self, event: &ExchangeEvent) -> std::io::Result<()> {
        self.0
            .borrow_mut()
            .push(serde_json::to_value(event).unwrap());
        Ok(())
    }
}

#[test]
fn run_ask_records_tool_events_between_request_and_outcome() {
    let transport = CapturingTransport::new(vec![
        crate::tools::tests::tool_use_reply("toolu_1", "echo"),
        AssistantReply::new("final answer"),
    ]);
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink: Box<dyn EventSink> = Box::new(CollectingSink(events.clone()));
    let mut sink = Rc::new(RefCell::new(sink));
    let (tool_loop, _) = echo_looped(&transport);
    let participant = LocalParticipant::new(
        tool_loop.with_observer(tool_trail_observer(sink.clone())),
        meta(),
    );
    let mut buf = Vec::new();
    run_ask(&participant, &meta(), "hello", &mut buf, &mut sink).expect("infallible");

    let events = events.borrow();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "request",
            "tool_round",
            "tool_call",
            "tool_result",
            "response_ok"
        ]
    );
    assert_eq!(
        events[1]["content"],
        serde_json::to_value(crate::tools::tests::tool_use_reply("toolu_1", "echo").content)
            .unwrap()
    );
    assert_eq!(events[2]["tool_use_id"], "toolu_1");
    assert_eq!(events[2]["tool_name"], "echo");
    assert_eq!(events[2]["input"], serde_json::json!({"text": "hi"}));
    assert_eq!(events[3]["tool_use_id"], "toolu_1");
    assert_eq!(events[3]["status"], "completed");
    assert_eq!(events[3]["result"], "echo: hi");
    assert!(events[1].get("session_id").is_none());
    assert!(events[2].get("session_id").is_none());
}

#[test]
fn session_repl_records_framed_tool_events_within_the_turn() {
    let transport = CapturingTransport::new(vec![
        crate::tools::tests::tool_use_reply("toolu_1", "missing"),
        AssistantReply::new("final answer"),
    ]);
    let (tool_loop, _) = echo_looped(&transport);
    let (trail, _, _) = run_repl_with(&tool_loop, b"go\n");

    let events: Vec<serde_json::Value> = trail
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "request",
            "tool_round",
            "tool_call",
            "tool_result",
            "response_ok",
            "session_end"
        ]
    );
    assert_eq!(events[1]["session_id"], "sess-1");
    assert_eq!(events[1]["turn_index"], 0);
    for tool_event in &events[2..4] {
        assert_eq!(tool_event["tool_use_id"], "toolu_1");
        assert_eq!(tool_event["tool_name"], "missing");
        assert_eq!(tool_event["session_id"], "sess-1");
        assert_eq!(tool_event["turn_index"], 0);
    }
    assert_eq!(events[3]["status"], "failed");
    assert_eq!(events[3]["error"], "unknown tool: missing");
    assert!(events[3].get("result").is_none());
}

#[test]
fn execute_exchange_core_runs_the_loop_inside_one_envelope() {
    let transport = CapturingTransport::new(vec![
        crate::tools::tests::tool_use_reply("toolu_1", "echo"),
        AssistantReply::new("final answer"),
    ]);
    let (tool_loop, count) = echo_looped(&transport);
    let participant = LocalParticipant::new(tool_loop, meta());
    let envelope = MessageEnvelope::new(
        "m-1",
        "c-1",
        "user",
        "assistant",
        MessageKind::Request,
        "hello",
        1_700_000_000_000,
    );
    let raw = serde_json::to_string(&envelope).unwrap();
    let mut buf = Vec::new();
    execute_exchange_core(&participant, &meta(), &raw, &mut buf, &mut NoopSink)
        .expect("infallible");

    let printed = String::from_utf8(buf).unwrap();
    assert_eq!(
        printed.lines().count(),
        1,
        "exactly one envelope: {printed}"
    );
    let value: serde_json::Value = serde_json::from_str(printed.trim()).unwrap();
    assert_eq!(value["kind"], "response");
    assert_eq!(value["body"], "final answer");
    assert_eq!(count.get(), 1);
    assert_eq!(transport.calls.borrow().len(), 2);
}

#[test]
fn session_repl_runs_the_tool_loop_and_keeps_its_rounds_in_history() {
    let tool_reply = AssistantReply::from_blocks(
        vec![ContentBlock::text("checking"), tool_use_block()],
        crate::model::TokenUsage::default(),
        Some(StopReason::ToolUse),
    );
    let transport = CapturingTransport::new(vec![
        tool_reply,
        AssistantReply::new("done"),
        AssistantReply::new("again"),
    ]);
    let (tool_loop, _) = echo_looped(&transport);
    let (trail, warning, output) = run_repl_with(&tool_loop, b"one\ntwo\n");

    assert_eq!(
        output, "done\nagain\n",
        "only each turn's final text prints"
    );
    assert_eq!(warning, "");
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 3);
    let unknown_result = ContentBlock::ToolResult {
        tool_use_id: "toolu_1".to_string(),
        content: "unknown tool: read".to_string(),
        is_error: Some(true),
    };
    assert_eq!(
        calls[2],
        vec![
            Message::user("one"),
            Message::new(
                Role::Assistant,
                vec![ContentBlock::text("checking"), tool_use_block()]
            ),
            Message::new(Role::User, vec![unknown_result]),
            Message::assistant("done"),
            Message::user("two"),
        ],
        "the next turn resends the prior turn's tool rounds"
    );

    // request, tool_round, tool_call, tool_result, then the turn's outcome.
    let response: serde_json::Value = serde_json::from_str(trail.lines().nth(4).unwrap()).unwrap();
    assert_eq!(response["event"], "response_ok");
    assert_eq!(response["reply"], "done");
    assert!(
        response.get("content").is_none(),
        "final reply is text-only"
    );
}

#[test]
fn session_repl_warns_and_drops_dangling_tool_use_when_capped() {
    let mut replies: Vec<AssistantReply> = (0..=TEST_TOOL_ROUND_LIMIT)
        .map(|i| crate::tools::tests::tool_use_reply(&format!("toolu_{i}"), "echo"))
        .collect();
    replies.push(AssistantReply::new("next"));
    let transport = CapturingTransport::new(replies);
    let (tool_loop, count) = echo_looped(&transport);
    let (_, warning, _) = run_repl_with(&tool_loop, b"go\nnext\n");

    assert_eq!(count.get(), TEST_TOOL_ROUND_LIMIT);
    assert_eq!(
        warning,
        format!("{}\n", tool_round_limit_warning(TEST_TOOL_ROUND_LIMIT))
    );
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), TEST_TOOL_ROUND_LIMIT + 2);
    let last = calls.last().unwrap();
    assert_eq!(
        last[last.len() - 2],
        Message::assistant("calling"),
        "the capped reply's unanswered tool_use is stripped from history"
    );
}

#[test]
fn session_reply_message_uses_placeholder_for_a_capped_tool_only_reply() {
    assert_eq!(
        session_reply_message(vec![tool_use_block()], true),
        Message::assistant(TOOL_ROUND_LIMIT_PLACEHOLDER)
    );
    assert_eq!(
        session_reply_message(vec![tool_use_block()], false),
        Message::new(Role::Assistant, vec![tool_use_block()])
    );
}

/// Runs `input` as REPL turns of session `session_id` through `tool_loop`,
/// appending the run's trail lines to `trail`.
fn append_session_run(
    tool_loop: &ToolLoop<impl Transport>,
    trail: &mut Vec<u8>,
    session_id: &str,
    input: &[u8],
) {
    let mut sink = WriterSink::new(trail);
    run_session_repl_with_warning(
        tool_loop,
        &mut sink,
        &meta(),
        std::io::Cursor::new(input.to_vec()),
        Vec::new(),
        session_id.to_string(),
        Conversation::new(),
        0,
        &mut Vec::new(),
    )
    .expect("session loop completes");
}

fn echo_use(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({"text": "hi"}),
    }
}

fn tool_reply(content: Vec<ContentBlock>) -> AssistantReply {
    AssistantReply::from_blocks(
        content,
        crate::model::TokenUsage::default(),
        Some(StopReason::ToolUse),
    )
}

/// A two-round tool turn (text beside two calls, one failing; then one
/// more call) followed by a text turn, all through the live REPL.
fn tool_session_replies() -> Vec<AssistantReply> {
    vec![
        tool_reply(vec![
            ContentBlock::text("checking"),
            echo_use("toolu_a", "echo"),
            echo_use("toolu_b", "missing"),
        ]),
        tool_reply(vec![echo_use("toolu_c", "echo")]),
        AssistantReply::new("done"),
        AssistantReply::new("ok"),
    ]
}

/// Resume rebuilds a tool turn exactly as the live REPL held it — each
/// round's reply, then one user turn with every result in `tool_use`
/// order — and the next turn continues at the next `turn_index`.
#[test]
fn session_resume_rehydrates_tool_rounds_exactly_as_the_live_history() {
    let live = CapturingTransport::new(tool_session_replies());
    let (tool_loop, _) = echo_looped(&live);
    let mut trail = Vec::new();
    append_session_run(&tool_loop, &mut trail, "sess-1", b"one\ntwo\n");

    let report = crate::log::parse_sessions(std::io::Cursor::new(trail)).expect("parses");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates");

    // The live turn-two request is the whole prior history plus "two".
    let mut live_history = live.calls.borrow()[3].clone();
    assert_eq!(
        live_history[..4],
        [
            Message::user("one"),
            Message::new(
                Role::Assistant,
                vec![
                    ContentBlock::text("checking"),
                    echo_use("toolu_a", "echo"),
                    echo_use("toolu_b", "missing"),
                ]
            ),
            Message::new(
                Role::User,
                vec![
                    echo_result("toolu_a"),
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_b".to_string(),
                        content: "unknown tool: missing".to_string(),
                        is_error: Some(true),
                    },
                ]
            ),
            Message::new(Role::Assistant, vec![echo_use("toolu_c", "echo")]),
        ]
    );
    live_history.push(Message::assistant("ok"));
    assert_eq!(resumed.conversation.messages(), &live_history[..]);
    assert_eq!(resumed.prior_turns, 2);
    assert_eq!(resumed.next_turn_index, 2);

    let next = CapturingTransport::new(vec![AssistantReply::new("three-reply")]);
    let mut resumed_trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut resumed_trail);
        execute_session_resumed(
            &echo_looped(&next).0,
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"three\n".to_vec()),
            Vec::new(),
            resumed,
        )
        .expect("resumed run completes");
    }
    live_history.push(Message::user("three"));
    assert_eq!(next.calls.borrow()[0], live_history);
    let request: serde_json::Value = serde_json::from_str(
        String::from_utf8(resumed_trail)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["turn_index"], 2);
}

#[test]
fn session_resume_echoes_thinking_signature_on_the_next_tool_continuation() {
    let first_thinking = ContentBlock::Thinking {
        thinking: "first private reasoning\npreserved exactly".to_string(),
        signature: "first-signature+/=".to_string(),
    };
    let first_tool_use = echo_use("toolu_before_resume", "echo");
    let first_transport = CapturingTransport::new(vec![
        tool_reply(vec![first_thinking.clone(), first_tool_use.clone()]),
        AssistantReply::new("first turn complete"),
    ]);
    let mut trail = Vec::new();
    append_session_run(
        &echo_looped(&first_transport).0,
        &mut trail,
        "sess-thinking",
        b"first turn\n",
    );

    let report = crate::log::parse_sessions(std::io::Cursor::new(trail)).expect("parses trail");
    let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates session");
    assert_eq!(
        resumed.conversation.messages()[1],
        Message::new(
            Role::Assistant,
            vec![first_thinking.clone(), first_tool_use.clone()]
        )
    );
    let prior_history = resumed.conversation.messages().to_vec();

    let second_thinking = ContentBlock::Thinking {
        thinking: "second round reasoning".to_string(),
        signature: "second-signature:=+/".to_string(),
    };
    let second_tool_use = echo_use("toolu_after_resume", "echo");
    let continued_transport = CapturingTransport::new(vec![
        tool_reply(vec![second_thinking.clone(), second_tool_use.clone()]),
        AssistantReply::new("second turn complete"),
    ]);
    let mut resumed_trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut resumed_trail);
        execute_session_resumed(
            &echo_looped(&continued_transport).0,
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"second turn\n".to_vec()),
            Vec::new(),
            resumed,
        )
        .expect("resumed session completes");
    }

    let calls = continued_transport.calls.borrow();
    let mut expected_first_call = prior_history;
    expected_first_call.push(Message::user("second turn"));
    assert_eq!(calls[0], expected_first_call);

    let assistant_messages = calls[1]
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 3);
    assert_eq!(
        assistant_messages[0].content,
        vec![first_thinking, first_tool_use]
    );
    assert_eq!(
        assistant_messages[1].content,
        vec![ContentBlock::text("first turn complete")]
    );
    assert_eq!(
        assistant_messages[2].content,
        vec![second_thinking, second_tool_use]
    );
}

/// A turn capped at the round limit resumes like the live history: every
/// round, then the capped reply without its unanswered `tool_use`.
#[test]
fn session_resume_strips_the_capped_replys_unanswered_tool_use() {
    let replies = (0..=TEST_TOOL_ROUND_LIMIT)
        .map(|i| crate::tools::tests::tool_use_reply(&format!("toolu_{i}"), "echo"))
        .collect();
    let (tool_loop, _) = echo_looped(CapturingTransport::new(replies));
    let mut trail = Vec::new();
    append_session_run(&tool_loop, &mut trail, "sess-1", b"go\n");

    let report = crate::log::parse_sessions(std::io::Cursor::new(trail)).expect("parses");
    let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates");
    let messages = resumed.conversation.messages();
    assert_eq!(messages.len(), 2 + 2 * TEST_TOOL_ROUND_LIMIT);
    assert_eq!(messages.last().unwrap(), &Message::assistant("calling"));
}

/// With several tool-bearing sessions on one trail, `--session` resumes
/// only the selected one's history.
#[test]
fn session_resume_selects_the_named_session_among_tool_sessions() {
    let mut trail = Vec::new();
    let first = CapturingTransport::new(tool_session_replies());
    append_session_run(&echo_looped(&first).0, &mut trail, "sess-1", b"one\ntwo\n");
    let second = CapturingTransport::new(vec![
        tool_reply(vec![echo_use("toolu_z", "echo")]),
        AssistantReply::new("z-done"),
    ]);
    append_session_run(&echo_looped(&second).0, &mut trail, "sess-2", b"zed\n");

    let report = crate::log::parse_sessions(std::io::Cursor::new(trail)).expect("parses");
    let resumed = select_and_rehydrate(report.sessions, Some("sess-2")).expect("rehydrates");
    assert_eq!(resumed.session_id, "sess-2");
    assert_eq!(resumed.next_turn_index, 1);
    assert_eq!(
        resumed.conversation.messages(),
        &[
            Message::user("zed"),
            Message::new(Role::Assistant, vec![echo_use("toolu_z", "echo")]),
            Message::new(Role::User, vec![echo_result("toolu_z")]),
            Message::assistant("z-done"),
        ]
    );
}

/// A tool-bearing trail written before `tool_round` existed carries no
/// round boundaries; it resumes as before — prompt and final reply only.
#[test]
fn session_resume_of_a_trail_without_tool_rounds_keeps_text_turns() {
    let live = CapturingTransport::new(tool_session_replies());
    let mut trail = Vec::new();
    append_session_run(&echo_looped(&live).0, &mut trail, "sess-1", b"one\ntwo\n");
    let legacy: String = String::from_utf8(trail)
        .unwrap()
        .lines()
        .filter(|line| !line.contains(r#""event":"tool_round""#))
        .map(|line| format!("{line}\n"))
        .collect();

    let report =
        crate::log::parse_sessions(std::io::Cursor::new(legacy.into_bytes())).expect("parses");
    let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates");
    assert_eq!(
        resumed.conversation.messages(),
        &[
            Message::user("one"),
            Message::assistant("done"),
            Message::user("two"),
            Message::assistant("ok"),
        ]
    );
    assert_eq!(resumed.next_turn_index, 2);
}

/// `log replay --index` selects a tool-bearing exchange by its global
/// trail index and reruns its request content through the current tool loop,
/// appending fresh request/tool/outcome events — the stored tool results
/// are never fed back.
#[test]
fn log_replay_reruns_a_tool_bearing_prompt_through_the_loop() {
    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let record_ask = |replies: Vec<AssistantReply>, prompt: &str| {
        let sink: Box<dyn EventSink> = Box::new(CollectingSink(recorded.clone()));
        let mut sink = Rc::new(RefCell::new(sink));
        let (tool_loop, _) = echo_looped(CapturingTransport::new(replies));
        let participant = LocalParticipant::new(
            tool_loop.with_observer(tool_trail_observer(sink.clone())),
            meta(),
        );
        run_ask(&participant, &meta(), prompt, Vec::new(), &mut sink).expect("infallible");
    };
    let trail_text = |events: &[serde_json::Value]| -> Vec<u8> {
        events
            .iter()
            .map(|e| format!("{e}\n"))
            .collect::<String>()
            .into_bytes()
    };

    record_ask(
        vec![
            crate::tools::tests::tool_use_reply("toolu_1", "echo"),
            AssistantReply::new("first"),
        ],
        "use a tool",
    );
    let mut trail = trail_text(&recorded.borrow());
    let mut session = Vec::new();
    append_session_run(
        &looped(CapturingTransport::new(vec![AssistantReply::new("hi")])),
        &mut session,
        "sess-1",
        b"later\n",
    );
    trail.extend(session);

    let report = crate::log::parse_jsonl(std::io::Cursor::new(trail.clone())).expect("parses");
    assert_eq!(report.exchanges.len(), 2, "global indexing spans sessions");
    let (config, prompt, content) = replay_target(&report, Some(1), || {
        LegConfig::from_lookup(|key| (key == "ANTHROPIC_API_KEY").then(|| "secret".to_string()))
    })
    .expect("selects");
    assert_eq!(prompt, "use a tool");
    assert_eq!(content, vec![ContentBlock::text("use a tool")]);
    assert_eq!(config.model, "claude-test-model");
    assert_eq!(config.base_url, "https://api.anthropic.com");

    recorded.borrow_mut().clear();
    let rerun = CapturingTransport::new(vec![
        crate::tools::tests::tool_use_reply("toolu_9", "echo"),
        AssistantReply::new("rerun"),
    ]);
    {
        let sink: Box<dyn EventSink> = Box::new(CollectingSink(recorded.clone()));
        let mut sink = Rc::new(RefCell::new(sink));
        let (tool_loop, count) = echo_looped(&rerun);
        let participant = LocalParticipant::new(
            tool_loop.with_observer(tool_trail_observer(sink.clone())),
            meta(),
        );
        run_ask(&participant, &meta(), &prompt, Vec::new(), &mut sink).expect("infallible");
        assert_eq!(count.get(), 1, "the tool executes afresh");
    }
    assert_eq!(rerun.calls.borrow()[0], vec![Message::user("use a tool")]);

    trail.extend(trail_text(&recorded.borrow()));
    let report = crate::log::parse_jsonl(std::io::Cursor::new(trail)).expect("parses");
    assert_eq!(report.exchanges.len(), 3);
    assert_eq!(report.exchanges[2].request.prompt, "use a tool");
    assert_eq!(report.tools[2].len(), 1);
    assert_eq!(report.tools[2][0].call.tool_use_id, "toolu_9");
    assert!(report.tools[2][0].result.is_some());
}

#[test]
fn log_replay_preserves_image_blocks_in_the_provider_request_and_trail() {
    let image = ContentBlock::Image {
        source: crate::model::ImageSource::Base64 {
            media_type: "image/jpeg".to_string(),
            data: "AQID".to_string(),
        },
    };
    let content = vec![image, ContentBlock::text("describe this")];
    let report = crate::log::ParseReport {
        exchanges: vec![Exchange {
            request: crate::events::RequestRecord {
                ts_ms: 1,
                model: "recorded-model".to_string(),
                base_url: "https://recorded.example".to_string(),
                prompt: "describe this".to_string(),
                content: Some(content.clone()),
                session_id: None,
                turn_index: None,
            },
            outcome: Outcome::Ok {
                ts_ms: 2,
                duration_ms: 1,
                reply: "old answer".to_string(),
                content: None,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                session_id: None,
                turn_index: None,
            },
        }],
        tools: vec![Vec::new()],
        warnings: Vec::new(),
    };
    let (config, prompt, replay_content) = replay_target(&report, None, || {
        LegConfig::from_lookup(|key| (key == "ANTHROPIC_API_KEY").then(|| "secret".to_string()))
    })
    .expect("selects replay target");
    assert_eq!(replay_content, content);
    assert_eq!(config.model, "recorded-model");
    assert_eq!(config.base_url, "https://recorded.example");

    let transport = CapturingTransport::new(vec![AssistantReply::new("new answer")]);
    let meta = exchange_meta(&config);
    let participant = LocalParticipant::new(looped(&transport), meta.clone());
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        run_ask_with_content(
            &participant,
            &meta,
            &prompt,
            &replay_content,
            Vec::new(),
            &mut sink,
        )
        .expect("replays");
    }

    assert_eq!(
        transport.calls.borrow()[0],
        vec![Message::new(Role::User, content.clone())]
    );
    let replayed =
        crate::log::parse_jsonl(std::io::Cursor::new(trail)).expect("parses replay trail");
    assert_eq!(replayed.exchanges[0].request.content, Some(content));
}

/// A bad `--index` is reported as a usage error before the environment's
/// config is loaded, so a missing credential cannot mask it.
#[test]
fn replay_target_reports_a_bad_index_before_loading_config() {
    let report = crate::log::ParseReport::default();
    let err = replay_target(&report, Some(3), || {
        Err(LegError::Config("no credential".to_string()))
    })
    .unwrap_err();
    assert!(matches!(err, LegError::Usage(_)), "{err:?}");
}

#[test]
fn session_repl_warns_only_on_max_tokens() {
    let reply = |stop: StopReason| {
        AssistantReply::from_blocks(
            vec![ContentBlock::text("r")],
            crate::model::TokenUsage::default(),
            Some(stop),
        )
    };

    let transport = CapturingTransport::new(vec![reply(StopReason::EndTurn)]);
    let (_, warning) = run_repl(&transport, b"hi\n");
    assert_eq!(warning, "", "end_turn must not warn");

    let transport = CapturingTransport::new(vec![reply(StopReason::MaxTokens)]);
    let (_, warning) = run_repl(&transport, b"hi\n");
    assert_eq!(
        warning,
        "warning: reply truncated (stop_reason: max_tokens)\n"
    );
}

/// All three block kinds written to a real trail — a `tool_use` reply and a
/// `tool_result` follow-up request — are rehydrated verbatim by `--resume`.
#[test]
fn select_and_rehydrate_restores_tool_blocks_from_a_real_trail() {
    let mut trail = Vec::new();
    {
        let mut sink = WriterSink::new(&mut trail);
        let events = [
            ExchangeEvent::session_start(1, "sess-1"),
            ExchangeEvent::session_request(2, &meta(), "read a.txt", "sess-1", 0),
            ExchangeEvent::session_response_ok(3, 1, "", None, None, Some("tool_use"), "sess-1", 0)
                .with_content(&[tool_use_block()]),
            ExchangeEvent::session_request(4, &meta(), "", "sess-1", 1)
                .with_content(&[tool_result_block()]),
            ExchangeEvent::session_response_ok(
                5,
                1,
                "it says hello",
                None,
                None,
                None,
                "sess-1",
                1,
            )
            .with_content(&[ContentBlock::text("it says hello")]),
        ];
        for event in &events {
            sink.record(event).unwrap();
        }
    }

    let report = crate::log::parse_sessions(std::io::Cursor::new(trail)).expect("parses");
    let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates");
    assert_eq!(
        resumed.conversation.messages(),
        &[
            Message::user("read a.txt"),
            Message::new(Role::Assistant, vec![tool_use_block()]),
            Message::new(Role::User, vec![tool_result_block()]),
            Message::assistant("it says hello"),
        ]
    );
    assert_eq!(resumed.next_turn_index, 2);
}

#[test]
fn select_and_rehydrate_restores_conversation_and_next_turn_index() {
    let sessions = vec![crate::log::SessionRecord {
        session_id: "sess-1".to_string(),
        started: true,
        ended: true,
        declared_turns: Some(2),
        turns: vec![
            crate::log::SessionTurn {
                request: crate::events::RequestRecord {
                    ts_ms: 1,
                    model: "m".to_string(),
                    base_url: "u".to_string(),
                    prompt: "hi".to_string(),
                    content: None,
                    session_id: Some("sess-1".to_string()),
                    turn_index: Some(0),
                },
                rounds: vec![],
                tools: vec![],
                outcome: Some(Outcome::Ok {
                    ts_ms: 2,
                    duration_ms: 1,
                    reply: "hello".to_string(),
                    content: None,
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    session_id: Some("sess-1".to_string()),
                    turn_index: Some(0),
                }),
            },
            crate::log::SessionTurn {
                request: crate::events::RequestRecord {
                    ts_ms: 3,
                    model: "m".to_string(),
                    base_url: "u".to_string(),
                    prompt: "failed".to_string(),
                    content: None,
                    session_id: Some("sess-1".to_string()),
                    turn_index: Some(1),
                },
                rounds: vec![],
                tools: vec![],
                outcome: None,
            },
        ],
    }];

    let resumed = select_and_rehydrate(sessions, None).expect("rehydrates");
    assert_eq!(resumed.session_id, "sess-1");
    assert_eq!(resumed.next_turn_index, 2);
    assert_eq!(resumed.conversation.len(), 2);
    assert_eq!(resumed.prior_turns, 1);
}

#[test]
fn select_and_rehydrate_requires_session_id_when_ambiguous() {
    let sessions = vec![
        crate::log::SessionRecord {
            session_id: "sess-1".to_string(),
            started: true,
            ended: true,
            declared_turns: Some(0),
            turns: vec![],
        },
        crate::log::SessionRecord {
            session_id: "sess-2".to_string(),
            started: true,
            ended: true,
            declared_turns: Some(0),
            turns: vec![],
        },
    ];
    assert!(matches!(
        select_and_rehydrate(sessions, None).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn select_and_rehydrate_errors_on_empty_trail() {
    assert!(matches!(
        select_and_rehydrate(vec![], None).unwrap_err(),
        LegError::Usage(_)
    ));
}

#[test]
fn execute_log_show_writes_a_block_per_exchange() {
    let exchanges = vec![Exchange {
        request: crate::events::RequestRecord {
            ts_ms: 1_700_000_000_000,
            model: "m".to_string(),
            base_url: "u".to_string(),
            prompt: "hello".to_string(),
            content: None,
            session_id: None,
            turn_index: None,
        },
        outcome: Outcome::Ok {
            ts_ms: 1_700_000_000_420,
            duration_ms: 418,
            reply: "hi there".to_string(),
            content: None,
            input_tokens: None,
            output_tokens: None,
            stop_reason: None,
            session_id: None,
            turn_index: None,
        },
    }];
    let report = crate::log::ParseReport {
        tools: vec![Vec::new()],
        exchanges,
        warnings: Vec::new(),
    };
    let mut buf = Vec::new();
    execute_log_show(&report, &mut buf).expect("writes");
    let text = String::from_utf8(buf).unwrap();
    assert!(text.contains("#1"));
    assert!(text.contains("hello"));
}

#[test]
fn execute_log_show_on_empty_log_writes_nothing() {
    let mut buf = Vec::new();
    execute_log_show(&crate::log::ParseReport::default(), &mut buf).expect("writes");
    assert!(buf.is_empty());
}

#[test]
fn select_exchange_defaults_to_last_and_validates_range() {
    let exchanges = vec![
        Exchange {
            request: crate::events::RequestRecord {
                ts_ms: 1,
                model: "m".to_string(),
                base_url: "u".to_string(),
                prompt: "a".to_string(),
                content: None,
                session_id: None,
                turn_index: None,
            },
            outcome: Outcome::Ok {
                ts_ms: 2,
                duration_ms: 1,
                reply: "ra".to_string(),
                content: None,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                session_id: None,
                turn_index: None,
            },
        },
        Exchange {
            request: crate::events::RequestRecord {
                ts_ms: 3,
                model: "m".to_string(),
                base_url: "u".to_string(),
                prompt: "b".to_string(),
                content: None,
                session_id: None,
                turn_index: None,
            },
            outcome: Outcome::Ok {
                ts_ms: 4,
                duration_ms: 1,
                reply: "rb".to_string(),
                content: None,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                session_id: None,
                turn_index: None,
            },
        },
    ];
    assert_eq!(
        select_exchange(&exchanges, None).unwrap().request.prompt,
        "b"
    );
    assert_eq!(
        select_exchange(&exchanges, Some(1)).unwrap().request.prompt,
        "a"
    );
    assert!(select_exchange(&exchanges, Some(3)).is_err());
    assert!(select_exchange(&[], None).is_err());
}

// -- `leg exchange` --------------------------------------------------

#[test]
fn parse_args_exchange_bare_defaults_both_paths_to_none() {
    assert_eq!(
        parse_args(&argv(&["exchange"])).unwrap(),
        Some(Command::Exchange {
            in_path: None,
            out_path: None,
            session: None,
            session_id_out: None,
        })
    );
}

#[test]
fn parse_args_exchange_accepts_in_and_out() {
    assert_eq!(
        parse_args(&argv(&["exchange", "--in", "/tmp/a", "--out", "/tmp/b"])).unwrap(),
        Some(Command::Exchange {
            in_path: Some("/tmp/a".to_string()),
            out_path: Some("/tmp/b".to_string()),
            session: None,
            session_id_out: None,
        })
    );
}

#[test]
fn parse_args_exchange_accepts_session_flags_in_either_order() {
    assert_eq!(
        parse_args(&argv(&[
            "exchange",
            "--session-id-out",
            "/tmp/id",
            "--session",
            "sess-1",
        ]))
        .unwrap(),
        Some(Command::Exchange {
            in_path: None,
            out_path: None,
            session: Some(ExchangeSession::Existing("sess-1".to_string())),
            session_id_out: Some("/tmp/id".to_string()),
        })
    );
    assert_eq!(
        parse_args(&argv(&[
            "exchange",
            "--new-session",
            "--session-id-out",
            "/tmp/id",
        ]))
        .unwrap(),
        Some(Command::Exchange {
            in_path: None,
            out_path: None,
            session: Some(ExchangeSession::New),
            session_id_out: Some("/tmp/id".to_string()),
        })
    );
}

#[test]
fn parse_args_exchange_rejects_conflicting_or_incomplete_session_flags() {
    assert!(
        parse_args(&argv(
            &["exchange", "--session", "sess-1", "--new-session",]
        ))
        .is_err()
    );
    assert!(
        parse_args(&argv(
            &["exchange", "--new-session", "--session", "sess-1",]
        ))
        .is_err()
    );
    assert!(
        parse_args(&argv(&["exchange", "--session", "--new-session"])).is_err(),
        "--new-session must not be consumed as the --session id"
    );
    assert!(parse_args(&argv(&["exchange", "--session-id-out", "/tmp/id"])).is_err());
    assert!(parse_args(&argv(&["exchange", "--session"])).is_err());
    assert!(parse_args(&argv(&["exchange", "--session-id-out"])).is_err());
}

#[test]
fn parse_args_exchange_missing_in_value_is_usage_error() {
    assert!(parse_args(&argv(&["exchange", "--in"])).is_err());
}

#[test]
fn parse_args_exchange_unexpected_argument_is_usage_error() {
    assert!(parse_args(&argv(&["exchange", "--who"])).is_err());
}

#[test]
fn parse_exchange_request_valid_envelope_passes_through_unchanged() {
    let envelope = MessageEnvelope::new(
        "m-1",
        "c-1",
        "user",
        "assistant",
        MessageKind::Request,
        "hello",
        1_700_000_000_000,
    );
    let raw = serde_json::to_string(&envelope).unwrap();
    let (request, mode) = parse_exchange_request(&raw);
    assert_eq!(mode, ExchangeMode::Envelope);
    assert_eq!(request, envelope);
}

#[test]
fn parse_exchange_request_plain_text_synthesizes_a_request() {
    let (request, mode) = parse_exchange_request("hello there\n");
    assert_eq!(mode, ExchangeMode::PlainText);
    assert_eq!(request.body, "hello there");
    assert_eq!(request.kind, MessageKind::Request);
}

#[test]
fn parse_exchange_request_trims_trailing_newline_only_preserves_trailing_spaces() {
    let (request, _) = parse_exchange_request("hello   \r\n");
    assert_eq!(request.body, "hello   ");
}

#[test]
fn execute_exchange_core_envelope_mode_success_writes_full_response_envelope() {
    let participant =
        LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
    let envelope = MessageEnvelope::new(
        "m-1",
        "c-1",
        "user",
        "assistant",
        MessageKind::Request,
        "hello",
        1_700_000_000_000,
    );
    let raw = serde_json::to_string(&envelope).unwrap();
    let mut buf = Vec::new();
    execute_exchange_core(&participant, &meta(), &raw, &mut buf, &mut NoopSink)
        .expect("infallible");
    let printed = String::from_utf8(buf).unwrap();
    let value: serde_json::Value = serde_json::from_str(printed.trim()).expect("valid json");
    assert_eq!(value["kind"], "response");
    assert_eq!(value["body"], "hi there");
    assert_eq!(value["exchange"]["schema"], crate::events::SCHEMA);
}

#[test]
fn execute_exchange_core_envelope_mode_error_still_writes_full_error_envelope() {
    let participant = LocalParticipant::new(
        FakeTransport(Err(LegError::Auth("bad credentials".to_string()))),
        meta(),
    );
    let envelope = MessageEnvelope::new(
        "m-1",
        "c-1",
        "user",
        "assistant",
        MessageKind::Request,
        "hello",
        1_700_000_000_000,
    );
    let raw = serde_json::to_string(&envelope).unwrap();
    let mut buf = Vec::new();
    let err = execute_exchange_core(&participant, &meta(), &raw, &mut buf, &mut NoopSink)
        .expect_err("delivered errors must fail the command");
    assert_eq!(err.kind(), "turn_failure");
    let printed = String::from_utf8(buf).unwrap();
    let value: serde_json::Value = serde_json::from_str(printed.trim()).expect("valid json");
    assert_eq!(value["kind"], "error");
    assert_eq!(value["body"], "authentication error: bad credentials");
}

#[test]
fn execute_exchange_core_plain_text_success_writes_only_the_reply_body() {
    let participant =
        LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
    let mut buf = Vec::new();
    execute_exchange_core(&participant, &meta(), "hello", &mut buf, &mut NoopSink)
        .expect("infallible");
    assert_eq!(String::from_utf8(buf).unwrap(), "hi there\n");
}

#[test]
fn execute_exchange_core_plain_text_failure_writes_nothing_and_returns_error() {
    let participant = LocalParticipant::new(
        FakeTransport(Err(LegError::Auth("bad credentials".to_string()))),
        meta(),
    );
    let mut buf = Vec::new();
    let err = execute_exchange_core(&participant, &meta(), "hello", &mut buf, &mut NoopSink)
        .expect_err("delivered errors must fail the command");
    assert_eq!(err.kind(), "turn_failure");
    assert!(buf.is_empty(), "plain-text failure must leave stdout empty");
}
