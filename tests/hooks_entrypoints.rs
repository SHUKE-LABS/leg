use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("leg-hooks-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct ProviderReply {
    status: u16,
    content_type: &'static str,
    body: String,
    retry_after: Option<&'static str>,
}

impl ProviderReply {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.into(),
            retry_after: (status == 503).then_some("0"),
        }
    }

    fn stream(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body,
            retry_after: None,
        }
    }
}

fn spawn_provider(replies: Vec<ProviderReply>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    listener
        .set_nonblocking(true)
        .expect("set fake provider nonblocking");
    let address = listener.local_addr().expect("fake provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    thread::spawn(move || {
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(90);
            let (mut stream, body) = loop {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                stream
                    .set_nonblocking(false)
                    .expect("set accepted fake provider stream blocking");
                if let Some(body) = read_request_body(&mut stream) {
                    break (stream, body);
                }
            };
            recorded.lock().expect("provider request log").push(body);
            let reason = match reply.status {
                200 => "OK",
                503 => "Service Unavailable",
                _ => "Test response",
            };
            let retry_after = reply
                .retry_after
                .map(|value| format!("Retry-After: {value}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.status,
                reason,
                reply.content_type,
                retry_after,
                reply.body.len(),
                reply.body,
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
        }
    });

    (format!("http://{address}"), requests)
}

fn read_request_body(stream: &mut TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut received = Vec::new();
    let header_end = loop {
        if let Some(index) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0_u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return None,
            Ok(count) => received.extend_from_slice(&buffer[..count]),
        }
    };
    let headers = String::from_utf8_lossy(&received[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let target = header_end + content_length;
    while received.len() < target {
        let mut buffer = [0_u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return None,
            Ok(count) => received.extend_from_slice(&buffer[..count]),
        }
    }
    Some(String::from_utf8_lossy(&received[header_end..target]).into_owned())
}

fn text_reply(text: &str) -> String {
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn",
    })
    .to_string()
}

fn tool_reply(id: &str, input: Value) -> String {
    serde_json::json!({
        "content": [{ "type": "tool_use", "id": id, "name": "read", "input": input }],
        "stop_reason": "tool_use",
    })
    .to_string()
}

fn sse_event(name: &str, data: Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&data).expect("serialize SSE event")
    )
}

fn sse_text_reply(text: &str) -> String {
    let mut body = String::new();
    body.push_str(&sse_event(
        "message_start",
        serde_json::json!({
            "type": "message_start",
            "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } }
        }),
    ));
    body.push_str(&sse_event(
        "content_block_start",
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
    ));
    body.push_str(&sse_event(
        "content_block_delta",
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text }
        }),
    ));
    body.push_str(&sse_event(
        "content_block_stop",
        serde_json::json!({ "type": "content_block_stop", "index": 0 }),
    ));
    body.push_str(&sse_event(
        "message_delta",
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 1 }
        }),
    ));
    body.push_str(&sse_event(
        "message_stop",
        serde_json::json!({ "type": "message_stop" }),
    ));
    body
}

fn compile_hook_runner(directory: &Path) -> PathBuf {
    let source = directory.join("hook-runner.rs");
    let executable = directory.join(if cfg!(windows) {
        "hook-runner.exe"
    } else {
        "hook-runner"
    });
    fs::write(&source, include_str!("fixtures/hook_runner.rs")).expect("write hook runner source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2021")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("compile native hook runner");
    assert!(
        output.status.success(),
        "rustc failed to build hook runner: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

fn copy_hook_runner(runner: &Path, directory: &Path, label: &str) -> PathBuf {
    let executable = directory.join(format!(
        "{label}{}",
        if cfg!(windows) { ".exe" } else { "" }
    ));
    fs::copy(runner, &executable).expect("copy labeled hook runner");
    executable
}

fn hook_entry(id: &str, event: &str, matcher: Option<&str>, command: &Path) -> Value {
    let mut entry = serde_json::json!({ "id": id, "event": event, "command": command });
    if let Some(matcher) = matcher {
        entry["matcher"] = Value::String(matcher.to_string());
    }
    entry
}

fn write_hooks(path: &Path, entries: Vec<Value>) {
    fs::write(
        path,
        serde_json::json!({ "version": 1, "hooks": entries }).to_string(),
    )
    .expect("write hook config");
}

fn read_hook_records(path: &Path) -> Vec<(String, Value)> {
    fs::read_to_string(path)
        .expect("read hook log")
        .lines()
        .map(|line| {
            let (label, payload) = line.split_once('\t').expect("labeled hook record");
            (
                label.to_string(),
                serde_json::from_str(payload).expect("hook payload JSON"),
            )
        })
        .collect()
}

fn read_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read event trail")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event record JSON"))
        .collect()
}

fn leg(cwd: &Path, base_url: &str, hooks: &Path, hook_log: &Path, trail: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leg"));
    command
        .current_dir(cwd)
        .env("LEG_HOOKS_CONFIG", hooks)
        .env("LEG_HOOK_TEST_LOG", hook_log)
        .env("LEG_HOOK_TEST_MODE", "allow")
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("LEG_MODEL", "claude-hooks-test")
        .env("LEG_CONTEXT_WINDOW_TOKENS", "200000")
        .env("LEG_TIMEOUT_SECS", "30")
        .env("LEG_EVENT_LOG", trail)
        .env_remove("LEG_PRETOOL_HOOK")
        .env_remove("LEG_SYSTEM_PROMPT");
    command
}

fn run(mut command: Command, stdin: Option<&str>) -> Output {
    let mut child = command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg process");
    if let Some(stdin) = stdin {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .expect("write leg stdin");
    }
    child.wait_with_output().expect("wait for leg process")
}

#[test]
fn lifecycle_hooks_are_consistent_across_cli_entrypoints() {
    let scratch = TestDir::new("entrypoints");
    let cwd = scratch.path();
    let runner = compile_hook_runner(cwd);
    let hooks = cwd.join("hooks.json");
    let hook_log = cwd.join("hook-events.jsonl");
    let entries = [
        hook_entry("turn-start", "TurnStart", None, &runner),
        hook_entry("turn-end", "TurnEnd", None, &runner),
        hook_entry("session-start", "SessionStart", None, &runner),
        hook_entry("session-end", "SessionEnd", None, &runner),
    ];
    write_hooks(&hooks, entries.to_vec());

    let (ask_url, ask_requests) =
        spawn_provider(vec![ProviderReply::json(200, text_reply("ask ok"))]);
    let mut ask = leg(
        cwd,
        &ask_url,
        &hooks,
        &hook_log,
        &cwd.join("ask-trail.jsonl"),
    );
    ask.args(["ask", "ask prompt"]);
    let ask_output = run(ask, None);
    assert!(
        ask_output.status.success(),
        "ask failed: {}",
        String::from_utf8_lossy(&ask_output.stderr)
    );
    assert_eq!(ask_requests.lock().unwrap().len(), 1);
    let records = read_hook_records(&hook_log);
    assert_eq!(
        records
            .iter()
            .map(|(_, p)| p["hook_event_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["TurnStart", "TurnEnd"]
    );
    assert!(
        records
            .iter()
            .all(|(_, p)| p.get("session_id").is_none() && p.get("turn_index").is_none())
    );

    fs::write(&hook_log, "").expect("clear hook log before session");
    let (session_url, _) = spawn_provider(vec![ProviderReply::json(200, text_reply("session ok"))]);
    let session_trail = cwd.join("session-trail.jsonl");
    let mut session = leg(cwd, &session_url, &hooks, &hook_log, &session_trail);
    session.arg("session");
    let session_output = run(session, Some("session prompt\n/exit\n"));
    assert!(
        session_output.status.success(),
        "session failed: {}",
        String::from_utf8_lossy(&session_output.stderr)
    );
    let records = read_hook_records(&hook_log);
    assert_eq!(
        records
            .iter()
            .map(|(_, p)| p["hook_event_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["SessionStart", "TurnStart", "TurnEnd", "SessionEnd"]
    );
    let session_id = records[0].1["session_id"].as_str().expect("session id");
    assert!(records.iter().all(|(_, p)| p["session_id"] == session_id));
    assert_eq!(records[1].1["turn_index"], 0);
    assert_eq!(records[2].1["turn_index"], 0);
    assert_eq!(records[3].1["turns"], 1);
    let session_events = read_events(&session_trail);
    assert!(
        session_events
            .iter()
            .any(|event| event["event"] == "session_start")
    );
    assert!(
        session_events
            .iter()
            .any(|event| event["event"] == "session_end")
    );

    fs::write(&hook_log, "").expect("clear hook log before cold exchange");
    let (exchange_url, _) =
        spawn_provider(vec![ProviderReply::json(200, text_reply("exchange ok"))]);
    let exchange_trail = cwd.join("exchange-trail.jsonl");
    let mut exchange = leg(cwd, &exchange_url, &hooks, &hook_log, &exchange_trail);
    exchange.arg("exchange");
    let exchange_output = run(exchange, Some("cold exchange prompt"));
    assert!(
        exchange_output.status.success(),
        "exchange failed: {}",
        String::from_utf8_lossy(&exchange_output.stderr)
    );
    let records = read_hook_records(&hook_log);
    assert_eq!(
        records
            .iter()
            .map(|(_, p)| p["hook_event_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["TurnStart", "TurnEnd"]
    );
    assert!(
        records
            .iter()
            .all(|(_, p)| p.get("session_id").is_none() && p.get("turn_index").is_none())
    );
    assert!(
        read_events(&exchange_trail)
            .iter()
            .any(|event| event["event"] == "response_ok")
    );

    fs::write(&hook_log, "").expect("clear hook log before stream exchange");
    let (stream_url, _) = spawn_provider(vec![ProviderReply::stream(sse_text_reply("stream ok"))]);
    let stream_trail = cwd.join("stream-trail.jsonl");
    let mut stream = leg(cwd, &stream_url, &hooks, &hook_log, &stream_trail);
    stream.args(["exchange", "--stream-json"]);
    let stream_output = run(stream, Some("stream prompt\n"));
    assert!(
        stream_output.status.success(),
        "stream-json failed: {}",
        String::from_utf8_lossy(&stream_output.stderr)
    );
    let records = read_hook_records(&hook_log);
    assert_eq!(
        records
            .iter()
            .map(|(_, p)| p["hook_event_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["TurnStart", "TurnEnd"]
    );
    assert!(String::from_utf8_lossy(&stream_output.stdout).contains("\"event\":\"turn_end\""));
    assert!(
        read_events(&stream_trail)
            .iter()
            .any(|event| event["event"] == "response_ok")
    );
}

#[test]
fn resumed_exchange_reloads_hooks_and_does_not_persist_hook_configuration() {
    let scratch = TestDir::new("resume");
    let cwd = scratch.path();
    let runner = compile_hook_runner(cwd);
    let old_runner = copy_hook_runner(&runner, cwd, "old-hook");
    let new_runner = copy_hook_runner(&runner, cwd, "new-hook");
    let hooks = cwd.join("hooks.json");
    let hook_log = cwd.join("hook-events.jsonl");
    let trail = cwd.join("event-trail.jsonl");
    let store = cwd.join("sessions");
    let session_id = "sess-228-1";
    let secret = "hook-env-secret-228";
    write_hooks(
        &hooks,
        vec![
            hook_entry("old-turn-start", "TurnStart", None, &old_runner),
            hook_entry("old-turn-end", "TurnEnd", None, &old_runner),
        ],
    );
    let (base_url, requests) = spawn_provider(vec![
        ProviderReply::json(200, text_reply("first turn")),
        ProviderReply::json(200, text_reply("resumed turn")),
    ]);

    let mut first = leg(cwd, &base_url, &hooks, &hook_log, &trail);
    first
        .env("LEG_HOOK_TEST_SECRET", secret)
        .env("LEG_SESSION_DIR", &store)
        .args(["exchange", "--new-session-id", session_id]);
    let first_output = run(first, Some("first turn"));
    assert!(
        first_output.status.success(),
        "new session exchange failed: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );

    write_hooks(
        &hooks,
        vec![
            hook_entry("new-turn-start", "TurnStart", None, &new_runner),
            hook_entry("new-turn-end", "TurnEnd", None, &new_runner),
        ],
    );
    let mut resumed = leg(cwd, &base_url, &hooks, &hook_log, &trail);
    resumed
        .env("LEG_HOOK_TEST_SECRET", secret)
        .env("LEG_SESSION_DIR", &store)
        .args(["exchange", "--session", session_id]);
    let resumed_output = run(resumed, Some("resumed turn"));
    assert!(
        resumed_output.status.success(),
        "resumed exchange failed: {}",
        String::from_utf8_lossy(&resumed_output.stderr)
    );
    assert_eq!(requests.lock().unwrap().len(), 2);

    let records = read_hook_records(&hook_log);
    assert_eq!(
        records
            .iter()
            .map(|(label, p)| (label.as_str(), p["hook_event_name"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("old-hook", "TurnStart"),
            ("old-hook", "TurnEnd"),
            ("new-hook", "TurnStart"),
            ("new-hook", "TurnEnd"),
        ]
    );
    for (turn, pair) in [(0, &records[0..2]), (1, &records[2..4])] {
        for (_, payload) in pair {
            assert_eq!(payload["session_id"], session_id);
            assert_eq!(payload["turn_index"], turn);
        }
    }

    let session_trail = store.join(format!("{session_id}.jsonl"));
    let trail_text = fs::read_to_string(session_trail).expect("read session trail");
    assert!(!trail_text.contains(&old_runner.to_string_lossy().to_string()));
    assert!(!trail_text.contains(&new_runner.to_string_lossy().to_string()));
    assert!(!trail_text.contains(secret));
}

#[test]
fn exchange_hooks_keep_identity_through_retry_and_tool_continuation() {
    let scratch = TestDir::new("retry-continuation");
    let cwd = scratch.path();
    fs::write(cwd.join("notes.txt"), "TODO\n").expect("write tool fixture");
    let runner = compile_hook_runner(cwd);
    let turn_start = copy_hook_runner(&runner, cwd, "turn-start");
    let pre_first = copy_hook_runner(&runner, cwd, "pre-first");
    let pre_second = copy_hook_runner(&runner, cwd, "pre-second");
    let post_tool = copy_hook_runner(&runner, cwd, "post-tool");
    let turn_end = copy_hook_runner(&runner, cwd, "turn-end");
    let hooks = cwd.join("hooks.json");
    let hook_log = cwd.join("hook-events.jsonl");
    let trail = cwd.join("event-trail.jsonl");
    let store = cwd.join("sessions");
    let session_id = "sess-228-2";
    write_hooks(
        &hooks,
        vec![
            hook_entry("turn-start", "TurnStart", None, &turn_start),
            hook_entry("pre-first", "PreToolUse", Some("read"), &pre_first),
            hook_entry("pre-second", "PreToolUse", Some("read"), &pre_second),
            hook_entry("post-tool", "PostToolUse", Some("read"), &post_tool),
            hook_entry("turn-end", "TurnEnd", None, &turn_end),
        ],
    );
    let error_body =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"try again"}}"#;
    let (base_url, requests) = spawn_provider(vec![
        ProviderReply::json(503, error_body),
        ProviderReply::json(
            200,
            tool_reply(
                "toolu_hooks_retry",
                serde_json::json!({ "path": "notes.txt" }),
            ),
        ),
        ProviderReply::json(200, text_reply("continued")),
    ]);

    let mut command = leg(cwd, &base_url, &hooks, &hook_log, &trail);
    command
        .env("LEG_SESSION_DIR", &store)
        .env("LEG_MAX_RETRIES", "1")
        .env("LEG_RETRY_BASE_DELAY_MS", "0")
        .args(["exchange", "--new-session-id", session_id]);
    let output = run(command, Some("read notes.txt"));
    assert!(
        output.status.success(),
        "exchange failed after {} provider request(s): {}",
        requests.lock().unwrap().len(),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0], requests[1],
        "retry repeats the same provider turn"
    );
    let continuation: Value = serde_json::from_str(&requests[2]).expect("provider request JSON");
    assert!(continuation["messages"].to_string().contains("TODO"));
    drop(requests);

    let records = read_hook_records(&hook_log);
    assert_eq!(
        records
            .iter()
            .map(|(label, p)| (label.as_str(), p["hook_event_name"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("turn-start", "TurnStart"),
            ("pre-first", "PreToolUse"),
            ("pre-second", "PreToolUse"),
            ("post-tool", "PostToolUse"),
            ("turn-end", "TurnEnd"),
        ]
    );
    for (_, payload) in &records {
        assert_eq!(payload["session_id"], session_id);
        assert_eq!(payload["turn_index"], 0);
    }
    assert_eq!(records[1].1["tool_input"]["path"], "notes.txt");
    assert_eq!(records[3].1["tool_status"], "completed");
    assert_eq!(records[3].1["tool_output"], "TODO\n");

    let events = read_events(&store.join(format!("{session_id}.jsonl")));
    let tool_call = events
        .iter()
        .find(|event| event["event"] == "tool_call")
        .expect("tool call trail event");
    let tool_result = events
        .iter()
        .find(|event| event["event"] == "tool_result")
        .expect("tool result trail event");
    assert_eq!(tool_call["session_id"], session_id);
    assert_eq!(tool_call["turn_index"], 0);
    assert_eq!(tool_result["session_id"], session_id);
    assert_eq!(tool_result["turn_index"], 0);
    assert_eq!(tool_result["status"], "completed");
}

fn assert_pretool_rejection(mode: &str, session_id: &str, expected: &str) {
    let scratch = TestDir::new(mode);
    let cwd = scratch.path();
    fs::write(cwd.join("notes.txt"), "TODO\n").expect("write tool fixture");
    let runner = compile_hook_runner(cwd);
    let hooks = cwd.join("hooks.json");
    let hook_log = cwd.join("hook-events.jsonl");
    let trail = cwd.join("event-trail.jsonl");
    let store = cwd.join("sessions");
    write_hooks(
        &hooks,
        vec![hook_entry(
            "read-policy",
            "PreToolUse",
            Some("read"),
            &runner,
        )],
    );
    let (base_url, requests) = spawn_provider(vec![
        ProviderReply::json(
            200,
            tool_reply("toolu_rejected", serde_json::json!({ "path": "notes.txt" })),
        ),
        ProviderReply::json(200, text_reply("handled")),
    ]);

    let mut command = leg(cwd, &base_url, &hooks, &hook_log, &trail);
    command
        .env("LEG_HOOK_TEST_MODE", mode)
        .env("LEG_SESSION_DIR", &store)
        .args(["exchange", "--new-session-id", session_id]);
    let output = run(command, Some("read notes.txt"));
    assert!(
        output.status.success(),
        "exchange failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let follow_up: Value =
        serde_json::from_str(&requests[1]).expect("follow-up provider request JSON");
    let tool_result = follow_up["messages"]
        .as_array()
        .expect("conversation messages")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "toolu_rejected")
        .expect("denied result is visible to provider");
    assert_eq!(tool_result["is_error"], true);
    assert_eq!(tool_result["content"], expected);
    drop(requests);

    let events = read_events(&store.join(format!("{session_id}.jsonl")));
    let denied = events
        .iter()
        .find(|event| event["event"] == "tool_result")
        .expect("denied trail record");
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["error"], expected);
}

#[test]
fn pretool_denial_reaches_provider_and_trail() {
    assert_pretool_rejection(
        "deny",
        "sess-228-3",
        "denied by pre-tool hook: fixture policy",
    );
}

#[test]
fn pretool_process_failure_fails_closed_in_provider_and_trail() {
    assert_pretool_rejection("fail", "sess-228-4", "denied by pre-tool hook: hook failed");
}

#[test]
fn observational_hook_failure_does_not_change_tool_or_turn_result() {
    let scratch = TestDir::new("observational-failure");
    let cwd = scratch.path();
    fs::write(cwd.join("notes.txt"), "TODO\n").expect("write tool fixture");
    let runner = compile_hook_runner(cwd);
    let hooks = cwd.join("hooks.json");
    let hook_log = cwd.join("hook-events.jsonl");
    let trail = cwd.join("event-trail.jsonl");
    write_hooks(
        &hooks,
        vec![hook_entry(
            "post-failure",
            "PostToolUse",
            Some("read"),
            &runner,
        )],
    );
    let (base_url, _) = spawn_provider(vec![
        ProviderReply::json(
            200,
            tool_reply(
                "toolu_post_failure",
                serde_json::json!({ "path": "notes.txt" }),
            ),
        ),
        ProviderReply::json(200, text_reply("operation completed")),
    ]);

    let mut command = leg(cwd, &base_url, &hooks, &hook_log, &trail);
    command
        .env("LEG_HOOK_TEST_MODE", "fail")
        .arg("ask")
        .arg("read notes.txt");
    let output = run(command, None);
    assert!(
        output.status.success(),
        "observational hook changed operation status: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "operation completed"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("PostToolUse hook"));
    let events = read_events(&trail);
    let result = events
        .iter()
        .find(|event| event["event"] == "tool_result")
        .expect("tool trail result");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], "TODO\n");
}

#[test]
fn observational_hook_timeout_does_not_change_turn_result() {
    let scratch = TestDir::new("timeout");
    let cwd = scratch.path();
    let runner = compile_hook_runner(cwd);
    let hooks = cwd.join("hooks.json");
    let hook_log = cwd.join("hook-events.jsonl");
    let trail = cwd.join("event-trail.jsonl");
    write_hooks(
        &hooks,
        vec![hook_entry("slow-turn-start", "TurnStart", None, &runner)],
    );
    let (base_url, requests) = spawn_provider(vec![ProviderReply::json(
        200,
        text_reply("turn completed despite hook timeout"),
    )]);

    let mut command = leg(cwd, &base_url, &hooks, &hook_log, &trail);
    command
        .env("LEG_HOOK_TEST_MODE", "sleep")
        .args(["ask", "timeout prompt"]);
    let output = run(command, None);

    assert!(
        output.status.success(),
        "observational timeout changed turn status: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "turn completed despite hook timeout"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("TurnStart hook"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    let records = read_hook_records(&hook_log);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1["hook_event_name"], "TurnStart");
    assert!(
        read_events(&trail)
            .iter()
            .any(|event| event["event"] == "response_ok")
    );
}
