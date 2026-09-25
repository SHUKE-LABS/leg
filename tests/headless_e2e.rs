//! Headless end-to-end proof of the tool loop: wire protocol -> loop -> real
//! `read`/`edit`/`bash` tools -> trail, through the compiled `leg ask` and
//! `leg exchange`.
//!
//! A loopback fake `/v1/messages` provider replays a scripted sequence of
//! rounds — three `stop_reason:"tool_use"` replies, then a final text reply —
//! and captures every request body, so each test can assert what leg sent as
//! well as what it did on disk. No live provider is contacted; every run gets
//! its own temp working directory.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// The scripted rounds: read the file, edit it, run a command that observes
/// the edit, then answer. Generous `bash` timeout: a Windows login shell can
/// take seconds to start.
const ROUNDS: [&str; 4] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"notes.txt"}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"tool_use","id":"toolu_2","name":"edit","input":{"path":"notes.txt","oldString":"TODO","newString":"DONE"}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"tool_use","id":"toolu_3","name":"bash","input":{"command":"grep -c DONE notes.txt > ran.txt","timeout":60}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"text","text":"all done"}],"stop_reason":"end_turn"}"#,
];

const ONE_TEXT_REPLY: [&str; 1] =
    [r#"{"content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn"}"#];

/// Starts a sequence mock server on an OS-assigned port, returning its base
/// URL and the request bodies it receives. It answers one connection per
/// scripted round, in order, then stops accepting — so a request beyond the
/// script fails at the client and shows up as an extra-request assertion
/// failure rather than a silent pass. Each request body is drained in full
/// before replying (closing early aborts the client's write on Windows/ureq).
fn spawn_sequence_server(rounds: &'static [&'static str]) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);

    thread::spawn(move || {
        for reply in rounds {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Some(body) = read_request_body(&mut stream) else {
                return;
            };
            captured.lock().unwrap().push(body);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (format!("http://{addr}"), requests)
}

/// Starts a one-shot fake provider that rejects the request with bad credentials.
fn spawn_auth_failure_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");

    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        if read_request_body(&mut stream).is_none() {
            return;
        }
        let body = r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });

    format!("http://{addr}")
}

/// Reads one HTTP/1.1 request and returns its `Content-Length` body.
fn read_request_body(stream: &mut impl Read) -> Option<String> {
    let mut buf = [0u8; 8192];
    let mut received = Vec::new();
    let header_end = loop {
        if let Some(pos) = received.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => received.extend_from_slice(&buf[..n]),
        }
    };

    let headers = String::from_utf8_lossy(&received[..header_end]);
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);

    while received.len() - header_end < content_length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
        }
    }
    Some(String::from_utf8_lossy(&received[header_end..]).into_owned())
}

/// A fresh temp working directory holding `notes.txt` = `TODO\n`.
fn fixture_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("leg-e2e-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    std::fs::write(dir.join("notes.txt"), "TODO\n").expect("write fixture file");
    dir
}

/// A `leg` command running in `cwd` against the fake provider at `base_url`.
fn leg(cwd: &Path, base_url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_leg"));
    cmd.current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("LEG_MODEL", "claude-test-model")
        .env("LEG_TIMEOUT_SECS", "5")
        .env_remove("LEG_EVENT_LOG")
        .env_remove("LEG_SYSTEM_PROMPT");
    cmd
}

/// Runs `cmd` to completion, writing `stdin` (then closing it) when given,
/// and panics if it has not exited within a deadline.
fn run(mut cmd: Command, stdin: Option<&str>) -> Output {
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg");
    if let Some(input) = stdin {
        // Dropping the taken handle closes the pipe, signalling EOF.
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(Duration::from_secs(120))
        .expect("leg did not exit within the deadline")
        .expect("wait for leg")
}

/// Asserts the four requests leg sent — tools advertised on the first, each
/// later one carrying the previous call's `tool_result` — then the on-disk
/// effects. Requests are checked first so a failing tool reports its result.
fn assert_chain_effects(cwd: &Path, requests: &Mutex<Vec<String>>) {
    let requests: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_str(body).expect("request body is JSON"))
        .collect();
    assert_eq!(requests.len(), 4, "one request per scripted round");

    let mut tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .expect("tools advertised")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    tools.sort_unstable();
    assert_eq!(tools, ["bash", "edit", "read", "write"]);

    for (round, (id, expect)) in [
        ("toolu_1", "TODO"),
        ("toolu_2", "Successfully replaced 1 occurrence"),
        ("toolu_3", "\"exit_code\":0"),
    ]
    .into_iter()
    .enumerate()
    {
        let messages = requests[round + 1]["messages"].as_array().unwrap();
        let result = messages.last().unwrap()["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_result")
            .unwrap_or_else(|| panic!("request {} carries no tool_result", round + 2));
        assert_eq!(result["tool_use_id"], id);
        assert_ne!(result["is_error"], true, "{id} failed: {result}");
        assert!(
            result["content"].as_str().unwrap().contains(expect),
            "{id} result lacks {expect:?}: {result}"
        );
    }

    assert_eq!(
        std::fs::read_to_string(cwd.join("notes.txt")).unwrap(),
        "DONE\n",
        "edit must rewrite the file"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("ran.txt")).unwrap().trim(),
        "1",
        "bash must run after the edit and see its result"
    );
}

fn assert_exchange_trail(cwd: &Path, base_url: &str, trail: &Path, prompt: &str) {
    let events: Vec<Value> = std::fs::read_to_string(trail)
        .expect("trail written")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trail line is JSON"))
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().expect("event kind"))
        .collect();
    assert_eq!(
        kinds,
        [
            "request",
            "tool_round",
            "tool_call",
            "tool_result",
            "tool_round",
            "tool_call",
            "tool_result",
            "tool_round",
            "tool_call",
            "tool_result",
            "response_ok",
        ]
    );

    let request = &events[0];
    assert_eq!(request["schema"], "baton.exchange/v1");
    assert_eq!(request["model"], "claude-test-model");
    assert_eq!(request["base_url"], base_url);
    assert_eq!(request["prompt"], prompt);
    assert!(request.get("session_id").is_none());
    assert!(request.get("turn_index").is_none());

    let tool_names: Vec<&str> = events
        .iter()
        .filter(|event| event["event"] == "tool_call")
        .map(|event| event["tool_name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(tool_names, ["read", "edit", "bash"]);
    let outcome = events.last().expect("outcome event");
    assert_eq!(outcome["reply"], "all done");

    let mut show = leg(cwd, base_url);
    show.arg("log").arg("show").arg("--file").arg(trail);
    let shown = run(show, None);
    assert!(
        shown.status.success(),
        "leg log show failed; stderr: {}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown = String::from_utf8_lossy(&shown.stdout);
    for (name, id) in [
        ("read", "toolu_1"),
        ("edit", "toolu_2"),
        ("bash", "toolu_3"),
    ] {
        assert!(
            shown.contains(&format!("tool:   {name} [{id}]")),
            "log show lacks the {name} call:\n{shown}"
        );
    }
    assert_eq!(shown.matches("→ completed: ").count(), 3, "{shown}");
    assert!(shown.contains("reply:  all done"), "{shown}");
}

#[test]
fn ask_drives_read_edit_bash_and_records_the_tool_trail() {
    let (base_url, requests) = spawn_sequence_server(&ROUNDS);
    let cwd = fixture_dir("ask");
    let trail = cwd.join("trail.jsonl");

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail)
        .args(["ask", "mark the TODO done and verify it"]);
    let output = run(cmd, None);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "leg ask failed; stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "all done");
    assert_chain_effects(&cwd, &requests);

    let events: Vec<Value> = std::fs::read_to_string(&trail)
        .expect("trail written")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trail line is JSON"))
        .collect();
    let steps: Vec<(&str, &str, &str)> = events
        .iter()
        .filter(|e| e["event"] == "tool_call" || e["event"] == "tool_result")
        .map(|e| {
            let detail = if e["event"] == "tool_call" {
                e["tool_name"].as_str().unwrap()
            } else {
                e["status"].as_str().unwrap()
            };
            (
                e["event"].as_str().unwrap(),
                e["tool_use_id"].as_str().unwrap(),
                detail,
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("tool_call", "toolu_1", "read"),
            ("tool_result", "toolu_1", "completed"),
            ("tool_call", "toolu_2", "edit"),
            ("tool_result", "toolu_2", "completed"),
            ("tool_call", "toolu_3", "bash"),
            ("tool_result", "toolu_3", "completed"),
        ]
    );

    let mut show = leg(&cwd, &base_url);
    show.arg("log").arg("show").arg("--file").arg(&trail);
    let shown = run(show, None);
    assert!(shown.status.success(), "leg log show failed");
    let shown = String::from_utf8_lossy(&shown.stdout);
    for (name, id) in [
        ("read", "toolu_1"),
        ("edit", "toolu_2"),
        ("bash", "toolu_3"),
    ] {
        assert!(
            shown.contains(&format!("tool:   {name} [{id}]")),
            "log show lacks the {name} call:\n{shown}"
        );
    }
    assert_eq!(shown.matches("→ completed: ").count(), 3, "{shown}");
    assert!(shown.contains("reply:  all done"), "{shown}");

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn ask_provider_failure_leaves_stdout_empty_and_exits_nonzero() {
    let base_url = spawn_auth_failure_server();
    let cwd = fixture_dir("ask-failure");

    let mut cmd = leg(&cwd, &base_url);
    cmd.args(["ask", "hello"]);
    let output = run(cmd, None);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "failed ask must exit non-zero");
    assert!(
        output.stdout.is_empty(),
        "failed ask must leave stdout empty"
    );
    assert!(
        stderr.contains("kind: error"),
        "stderr must identify the failed message kind; got {stderr:?}"
    );
    assert!(
        stderr.contains("invalid x-api-key"),
        "stderr must include the provider diagnostic; got {stderr:?}"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn exchange_drives_read_edit_bash_and_emits_one_envelope() {
    let (base_url, requests) = spawn_sequence_server(&ROUNDS);
    let cwd = fixture_dir("exchange");
    let trail = cwd.join("trail.jsonl");

    let request = serde_json::json!({
        "schema": "baton.message/v1",
        "message_id": "m-1",
        "conversation_id": "c-1",
        "from": "external",
        "to": "leg",
        "in_reply_to": null,
        "kind": "request",
        "body": "mark the TODO done and verify it",
        "ts_ms": 1_700_000_000_000_u64,
        "exchange": null
    });
    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail).arg("exchange");
    let output = run(cmd, Some(&request.to_string()));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "leg exchange failed; stderr: {stderr}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one envelope out; got {stdout:?}");
    let response: Value = serde_json::from_str(lines[0]).expect("response envelope is JSON");
    assert_eq!(response["schema"], "baton.message/v1");
    assert_eq!(response["kind"], "response");
    assert_eq!(response["in_reply_to"], "m-1");
    assert_eq!(response["body"], "all done");
    assert_chain_effects(&cwd, &requests);
    assert_exchange_trail(&cwd, &base_url, &trail, "mark the TODO done and verify it");

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn plain_text_exchange_drives_read_edit_bash_and_records_the_trail() {
    let (base_url, requests) = spawn_sequence_server(&ROUNDS);
    let cwd = fixture_dir("exchange-plain-text");
    let trail = cwd.join("trail.jsonl");
    let prompt = "mark the TODO done and verify it";

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail).arg("exchange");
    let output = run(cmd, Some(prompt));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "leg exchange failed; stderr: {stderr}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "all done\n");
    assert_chain_effects(&cwd, &requests);
    assert_exchange_trail(&cwd, &base_url, &trail, prompt);

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn exchange_does_not_create_a_trail_when_event_log_is_unset_or_blank() {
    for (tag, value) in [("unset", None), ("blank", Some("  "))] {
        let (base_url, _) = spawn_sequence_server(&ONE_TEXT_REPLY);
        let cwd = fixture_dir(&format!("exchange-log-{tag}"));
        let trail = cwd.join("trail.jsonl");

        let mut cmd = leg(&cwd, &base_url);
        if let Some(value) = value {
            cmd.env("LEG_EVENT_LOG", value);
        }
        cmd.arg("exchange");
        let output = run(cmd, Some("hello"));

        assert!(
            output.status.success(),
            "leg exchange failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi there\n");
        assert!(!trail.exists(), "disabled logging must not create a trail");

        std::fs::remove_dir_all(&cwd).ok();
    }
}

#[test]
fn exchange_event_log_open_failure_warns_without_changing_the_reply() {
    let (base_url, _) = spawn_sequence_server(&ONE_TEXT_REPLY);
    let cwd = fixture_dir("exchange-log-open-failure");
    let missing_parent = cwd.join("missing").join("trail.jsonl");

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &missing_parent).arg("exchange");
    let output = run(cmd, Some("hello"));

    assert!(
        output.status.success(),
        "log open failure changed the exit status: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hi there\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("warning: failed to open"),
        "log open failure must warn on stderr"
    );
    assert!(!missing_parent.exists());

    std::fs::remove_dir_all(&cwd).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn exchange_event_log_write_failure_warns_without_changing_the_reply() {
    let (base_url, _) = spawn_sequence_server(&ONE_TEXT_REPLY);
    let cwd = fixture_dir("exchange-log-write-failure");

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", "/dev/full").arg("exchange");
    let output = run(cmd, Some("hello"));

    assert!(
        output.status.success(),
        "log write failure changed the exit status: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hi there\n");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("warning: failed to record exchange event"),
        "log write failure must warn on stderr"
    );

    std::fs::remove_dir_all(&cwd).ok();
}
