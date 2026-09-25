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

#[cfg(unix)]
use std::time::Instant;

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

#[cfg(unix)]
const PRETOOL_DENY_ROUNDS: [&str; 2] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_denied","name":"bash","input":{"command":"touch blocked.txt"}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#,
];

#[cfg(unix)]
const PRETOOL_ALLOW_ROUNDS: [&str; 2] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_allowed","name":"bash","input":{"command":"touch allowed.txt"}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#,
];

const PRETOOL_UNSET_ROUNDS: [&str; 2] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_unset","name":"bash","input":{"command":"touch unguarded.txt"}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#,
];

const SESSION_ROUNDS: [&str; 3] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_session","name":"read","input":{"path":"notes.txt"}}],"stop_reason":"tool_use"}"#,
    r#"{"content":[{"type":"text","text":"first turn remembered"}],"stop_reason":"end_turn"}"#,
    r#"{"content":[{"type":"text","text":"history restored"}],"stop_reason":"end_turn"}"#,
];

#[cfg(unix)]
const SLEEP_TOOL_ROUNDS: [&str; 1] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_sleep","name":"bash","input":{"command":"echo $$ > shell.pid; sleep 60 & echo $! > sleep.pid; wait","timeout":60}}],"stop_reason":"tool_use"}"#,
];

#[cfg(unix)]
const SECOND_SIGNAL_ROUNDS: [&str; 1] = [
    r#"{"content":[{"type":"tool_use","id":"toolu_second_signal","name":"bash","input":{"command":"trap '' TERM; echo $$ > shell.pid; echo $$ > sleep.pid; exec sleep 60","timeout":60}}],"stop_reason":"tool_use"}"#,
];

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

/// Starts a stoppable provider probe. Every received request is captured and
/// answered, allowing tests to prove a command made no network call.
type RequestProbe = (
    String,
    Arc<Mutex<Vec<String>>>,
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
);

fn spawn_request_probe() -> RequestProbe {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    listener
        .set_nonblocking(true)
        .expect("set mock server nonblocking");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let (stop, stopped) = mpsc::channel();

    let server = thread::spawn(move || {
        loop {
            match stopped.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(body) = read_request_body(&mut stream) else {
                        break;
                    };
                    captured.lock().unwrap().push(body);
                    let reply = ONE_TEXT_REPLY[0];
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });

    (format!("http://{addr}"), requests, stop, server)
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

#[cfg(unix)]
fn write_pretool_hook(cwd: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = cwd.join("pretool-hook");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write pre-tool hook");
    let mut permissions = std::fs::metadata(&path)
        .expect("pre-tool hook metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("make pre-tool hook executable");
    path
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
        .env_remove("LEG_SYSTEM_PROMPT")
        .env_remove("LEG_PRETOOL_HOOK");
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

#[cfg(unix)]
struct SignalTestCleanup {
    leg_pid: libc::pid_t,
    shell_pid_file: PathBuf,
    sleep_pid_file: PathBuf,
    active: bool,
}

#[cfg(unix)]
impl Drop for SignalTestCleanup {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(shell_pid) = read_pid_file(&self.shell_pid_file) {
            unsafe {
                libc::kill(-shell_pid, libc::SIGKILL);
            }
        }
        if let Some(sleep_pid) = read_pid_file(&self.sleep_pid_file) {
            unsafe {
                libc::kill(sleep_pid, libc::SIGKILL);
            }
        }
        unsafe {
            libc::kill(self.leg_pid, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
fn read_pid_file(path: &Path) -> Option<libc::pid_t> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(unix)]
fn run_until_bash_signal(
    mut command: Command,
    cwd: &Path,
    input: Option<&str>,
    keep_stdin_open: bool,
    signal: libc::c_int,
) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg");
    let leg_pid = child.id().try_into().expect("leg pid fits pid_t");
    let shell_pid_file = cwd.join("shell.pid");
    let sleep_pid_file = cwd.join("sleep.pid");
    let mut cleanup = SignalTestCleanup {
        leg_pid,
        shell_pid_file: shell_pid_file.clone(),
        sleep_pid_file: sleep_pid_file.clone(),
        active: true,
    };
    let mut stdin = child.stdin.take();
    if let Some(input) = input {
        stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .expect("write leg input");
    }
    if !keep_stdin_open {
        stdin.take();
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let sleep_pid = loop {
        if let Some(pid) = read_pid_file(&sleep_pid_file) {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "bash did not start the sleep process"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let started = Instant::now();
    assert_eq!(
        unsafe { libc::kill(leg_pid, signal) },
        0,
        "send signal to leg"
    );
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("leg did not stop within five seconds")
        .expect("wait for leg");
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    while process_is_running(sleep_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_is_running(sleep_pid),
        "sleep process {sleep_pid} survived the interrupt"
    );
    assert!(
        started.elapsed() <= Duration::from_secs(5),
        "leg exceeded the interrupt deadline"
    );
    cleanup.active = false;
    output
}

#[cfg(unix)]
fn process_is_running(pid: libc::pid_t) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps to check child process");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

#[cfg(unix)]
struct StalledServer {
    release: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Drop for StalledServer {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(unix)]
fn spawn_stalled_server() -> (String, mpsc::Receiver<()>, StalledServer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let (received, request_received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        if read_request_body(&mut stream).is_none() {
            return;
        }
        let _ = received.send(());
        if released.recv_timeout(Duration::from_secs(10)).is_err() {
            return;
        }
        let reply = ONE_TEXT_REPLY[0];
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    (
        format!("http://{addr}"),
        request_received,
        StalledServer {
            release,
            worker: Some(server),
        },
    )
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

fn read_events(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("trail written")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trail line is JSON"))
        .collect()
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

#[cfg(unix)]
#[test]
fn pretool_hook_deny_skips_bash_and_records_denied_result() {
    let (base_url, requests) = spawn_sequence_server(&PRETOOL_DENY_ROUNDS);
    let cwd = fixture_dir("pretool-deny");
    let trail = cwd.join("trail.jsonl");
    let hook = write_pretool_hook(
        &cwd,
        r#"payload="$(cat)"
printf '%s' "$payload" | grep -Fq '"hook_event_name":"PreToolUse"' || exit 2
printf '%s' "$payload" | grep -Fq '"tool_name":"bash"' || exit 3
printf '%s' "$payload" | grep -Fq '"tool_input":{"command":"touch blocked.txt"}' || exit 4
current_dir="$(pwd)"
printf '%s' "$payload" | grep -Fq "\"cwd\":\"$current_dir\"" || exit 5
printf '{"decision":"deny","reason":"bash is forbidden"}'
"#,
    );

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_PRETOOL_HOOK", &hook)
        .env("LEG_EVENT_LOG", &trail)
        .args(["ask", "try a forbidden command"]);
    let output = run(cmd, None);
    assert!(
        output.status.success(),
        "leg ask failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");
    assert!(
        !cwd.join("blocked.txt").exists(),
        "denied bash must not run"
    );

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "denied result is sent to the provider");
    let second: Value = serde_json::from_str(&requests[1]).expect("request body is JSON");
    let result = second["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find_map(|message| {
            message["content"].as_array()?.iter().find(|block| {
                block["type"] == "tool_result" && block["tool_use_id"] == "toolu_denied"
            })
        })
        .expect("follow-up request carries the denied tool result");
    assert_eq!(result["is_error"], true);
    assert_eq!(
        result["content"],
        "denied by pre-tool hook: bash is forbidden"
    );
    drop(requests);

    let events = read_events(&trail);
    let denied = events
        .iter()
        .find(|event| event["event"] == "tool_result")
        .expect("denial is recorded");
    assert_eq!(denied["status"], "denied");
    assert_eq!(
        denied["error"],
        "denied by pre-tool hook: bash is forbidden"
    );

    let mut show = leg(&cwd, &base_url);
    show.arg("log").arg("show").arg("--file").arg(&trail);
    let shown = run(show, None);
    assert!(shown.status.success(), "log show failed");
    assert!(
        String::from_utf8_lossy(&shown.stdout)
            .contains("→ denied: denied by pre-tool hook: bash is forbidden")
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[cfg(unix)]
#[test]
fn pretool_hook_allow_runs_bash() {
    let (base_url, requests) = spawn_sequence_server(&PRETOOL_ALLOW_ROUNDS);
    let cwd = fixture_dir("pretool-allow");
    let hook = write_pretool_hook(
        &cwd,
        r#"cat >/dev/null
printf '{"decision":"allow"}'
"#,
    );

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_PRETOOL_HOOK", &hook)
        .args(["ask", "run an allowed command"]);
    let output = run(cmd, None);
    assert!(
        output.status.success(),
        "leg ask failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(cwd.join("allowed.txt").is_file());
    assert_eq!(requests.lock().unwrap().len(), 2);

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn unset_pretool_hook_preserves_normal_tool_dispatch() {
    let (base_url, requests) = spawn_sequence_server(&PRETOOL_UNSET_ROUNDS);
    let cwd = fixture_dir("pretool-unset");

    let mut cmd = leg(&cwd, &base_url);
    cmd.env_remove("LEG_PRETOOL_HOOK")
        .args(["ask", "run without a pre-tool hook"]);
    let output = run(cmd, None);
    assert!(
        output.status.success(),
        "leg ask failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(cwd.join("unguarded.txt").is_file());
    assert_eq!(requests.lock().unwrap().len(), 2);

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
fn exchange_named_session_restores_tool_history_and_writes_its_id() {
    let (base_url, requests) = spawn_sequence_server(&SESSION_ROUNDS);
    let cwd = fixture_dir("exchange-named-session");
    let store = cwd.join("sessions");
    let event_log = cwd.join("legacy-events.jsonl");
    let id_out = cwd.join("session-id.txt");
    let continued_id_out = cwd.join("continued-session-id.txt");

    let mut first = leg(&cwd, &base_url);
    first
        .env("LEG_SESSION_DIR", &store)
        .env("LEG_EVENT_LOG", &event_log)
        .args(["exchange", "--new-session", "--session-id-out"])
        .arg(&id_out);
    let request = serde_json::json!({
        "schema": "baton.message/v1",
        "message_id": "m-session-1",
        "conversation_id": "c-session-1",
        "from": "external",
        "to": "leg",
        "in_reply_to": null,
        "kind": "request",
        "body": "read notes.txt",
        "ts_ms": 1_700_000_000_000_u64,
        "exchange": null
    })
    .to_string();
    let first_output = run(first, Some(&request));
    assert!(
        first_output.status.success(),
        "first session exchange failed: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let first_response: Value =
        serde_json::from_slice(&first_output.stdout).expect("response envelope is JSON");
    assert_eq!(first_response["kind"], "response");
    assert_eq!(first_response["body"], "first turn remembered");

    let id_line = std::fs::read_to_string(&id_out).expect("new session id written");
    let session_id = id_line.trim_end_matches('\n');
    assert!(!session_id.is_empty());
    assert_eq!(id_line, format!("{session_id}\n"));
    assert_eq!(
        first_response["exchange"]["exchange"]["request"]["session_id"],
        session_id
    );
    let session_trail = store.join(format!("{session_id}.jsonl"));
    assert!(session_trail.is_file(), "session trail created");

    let mut continued = leg(&cwd, &base_url);
    continued
        .env("LEG_SESSION_DIR", &store)
        .env("LEG_EVENT_LOG", &event_log)
        .args(["exchange", "--session", session_id, "--session-id-out"])
        .arg(&continued_id_out);
    let continued_output = run(continued, Some("what should I do next?"));
    assert!(
        continued_output.status.success(),
        "continued session exchange failed: {}",
        String::from_utf8_lossy(&continued_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&continued_output.stdout),
        "history restored\n"
    );
    assert_eq!(
        std::fs::read_to_string(&continued_id_out).expect("continued id written"),
        format!("{session_id}\n")
    );

    let requests: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_str(body).expect("request body is JSON"))
        .collect();
    assert_eq!(requests.len(), 3, "two tool-loop calls and one later turn");
    let messages = requests[2]["messages"]
        .as_array()
        .expect("conversation history");
    assert_eq!(messages.len(), 5, "prior turn plus new user message");
    assert_eq!(messages[0]["content"], "read notes.txt");
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    assert_eq!(messages[1]["content"][0]["id"], "toolu_session");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_session");
    assert_eq!(messages[3]["content"], "first turn remembered");
    assert_eq!(messages[4]["content"], "what should I do next?");

    let events = read_events(&session_trail);
    let request_events: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "request")
        .collect();
    assert_eq!(request_events.len(), 2);
    assert_eq!(request_events[0]["session_id"], session_id);
    assert_eq!(request_events[0]["turn_index"], 0);
    assert_eq!(request_events[1]["session_id"], session_id);
    assert_eq!(request_events[1]["turn_index"], 1);
    assert!(events.iter().any(|event| {
        event["event"] == "tool_result"
            && event["session_id"] == session_id
            && event["turn_index"] == 0
    }));
    assert_eq!(
        read_events(&event_log),
        events,
        "LEG_EVENT_LOG receives the same session events additively"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn exchange_new_session_resolves_store_directory_precedence() {
    for (tag, explicit_dir, xdg_dir) in [
        ("explicit", true, true),
        ("xdg", false, true),
        ("home", false, false),
    ] {
        let (base_url, _) = spawn_sequence_server(&ONE_TEXT_REPLY);
        let cwd = fixture_dir(&format!("exchange-session-store-{tag}"));
        let explicit_store = cwd.join("explicit-store");
        let xdg_state = cwd.join("xdg-state");
        let home = cwd.join("home");
        let expected_store = if explicit_dir {
            explicit_store.clone()
        } else if xdg_dir {
            xdg_state.join("leg").join("sessions")
        } else {
            home.join(".local")
                .join("state")
                .join("leg")
                .join("sessions")
        };
        let id_out = cwd.join("session-id.txt");

        let mut cmd = leg(&cwd, &base_url);
        cmd.env("HOME", &home)
            .env("XDG_STATE_HOME", &xdg_state)
            .arg("exchange")
            .arg("--new-session")
            .arg("--session-id-out")
            .arg(&id_out);
        if explicit_dir {
            cmd.env("LEG_SESSION_DIR", &explicit_store);
        } else {
            cmd.env_remove("LEG_SESSION_DIR");
        }
        if !xdg_dir {
            cmd.env_remove("XDG_STATE_HOME");
        }

        let output = run(cmd, Some("hello"));
        assert!(
            output.status.success(),
            "new session failed for {tag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let session_id = std::fs::read_to_string(&id_out)
            .expect("session id written")
            .trim_end()
            .to_string();
        let trail = expected_store.join(format!("{session_id}.jsonl"));
        assert!(trail.is_file(), "{tag} precedence chose {trail:?}");
        assert_eq!(
            [
                explicit_store.exists(),
                xdg_state.join("leg").join("sessions").exists(),
                home.join(".local")
                    .join("state")
                    .join("leg")
                    .join("sessions")
                    .exists(),
            ]
            .into_iter()
            .filter(|exists| *exists)
            .count(),
            1,
            "only the selected store directory is created for {tag}"
        );

        std::fs::remove_dir_all(&cwd).ok();
    }
}

#[test]
fn exchange_unknown_session_fails_before_any_provider_request() {
    let (base_url, requests, stop, server) = spawn_request_probe();
    let cwd = fixture_dir("exchange-unknown-session");
    let store = cwd.join("sessions");
    let id_out = cwd.join("should-not-exist.txt");
    let unknown_id = "missing-session";

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_SESSION_DIR", &store)
        .args(["exchange", "--session", unknown_id, "--session-id-out"])
        .arg(&id_out);
    let output = run(cmd, None);
    stop.send(()).expect("stop request probe");
    server.join().expect("join request probe");

    assert!(!output.status.success(), "unknown session must fail");
    assert!(
        output.stdout.is_empty(),
        "unknown session leaves stdout empty"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("leg: no session found: {unknown_id}\n")
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "unknown session must not contact the provider"
    );
    assert!(!id_out.exists(), "unknown session does not write an id");
    assert!(
        !store.exists(),
        "reading an unknown session does not create a store"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn exchange_failed_named_session_turn_is_stored_and_resumable() {
    let cwd = fixture_dir("exchange-session-failure");
    let store = cwd.join("sessions");
    let id_out = cwd.join("session-id.txt");
    let failure_url = spawn_auth_failure_server();

    let mut first = leg(&cwd, &failure_url);
    first
        .env("LEG_SESSION_DIR", &store)
        .args(["exchange", "--new-session", "--session-id-out"])
        .arg(&id_out);
    let first_output = run(first, Some("this turn fails"));
    assert!(!first_output.status.success(), "failed turn exits non-zero");
    assert!(
        first_output.stdout.is_empty(),
        "plain-text failure has no stdout"
    );
    let session_id = std::fs::read_to_string(&id_out)
        .expect("session id written even after failed turn")
        .trim_end()
        .to_string();
    let session_trail = store.join(format!("{session_id}.jsonl"));
    let initial_events = read_events(&session_trail);
    assert_eq!(initial_events.len(), 2);
    assert_eq!(initial_events[0]["event"], "request");
    assert_eq!(initial_events[0]["turn_index"], 0);
    assert_eq!(initial_events[1]["event"], "response_error");
    assert_eq!(initial_events[1]["session_id"], session_id);
    assert_eq!(initial_events[1]["turn_index"], 0);
    assert_eq!(initial_events[1]["kind"], "auth");

    let (base_url, requests) = spawn_sequence_server(&ONE_TEXT_REPLY);
    let mut continued = leg(&cwd, &base_url);
    continued
        .env("LEG_SESSION_DIR", &store)
        .args(["exchange", "--session", &session_id]);
    let continued_output = run(continued, Some("continue after failure"));
    assert!(
        continued_output.status.success(),
        "session was not resumable: {}",
        String::from_utf8_lossy(&continued_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&continued_output.stdout),
        "hi there\n"
    );
    let requests: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_str(body).expect("request body is JSON"))
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        requests[0]["messages"][0]["content"],
        "continue after failure"
    );

    let events = read_events(&session_trail);
    assert_eq!(events[2]["event"], "request");
    assert_eq!(events[2]["session_id"], session_id);
    assert_eq!(events[2]["turn_index"], 1);
    assert_eq!(events[3]["event"], "response_ok");
    assert_eq!(events[3]["turn_index"], 1);

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

#[cfg(unix)]
fn assert_exchange_interrupt(signal: libc::c_int, exit_code: i32, signal_name: &str, tag: &str) {
    let (base_url, requests) = spawn_sequence_server(&SLEEP_TOOL_ROUNDS);
    let cwd = fixture_dir(tag);
    let trail = cwd.join("trail.jsonl");

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail).arg("exchange");
    let request = serde_json::json!({
        "schema": "baton.message/v1",
        "message_id": "m-interrupt",
        "conversation_id": "c-interrupt",
        "from": "external",
        "to": "leg",
        "in_reply_to": null,
        "kind": "request",
        "body": "run the long command",
        "ts_ms": 1_700_000_000_000_u64,
        "exchange": null
    })
    .to_string();
    let output = run_until_bash_signal(cmd, &cwd, Some(&request), false, signal);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(exit_code));
    assert!(
        output.stdout.is_empty(),
        "interrupted exchange wrote stdout"
    );
    assert!(
        stderr.contains(&format!("interrupted by {signal_name}")),
        "missing interruption diagnostic: {stderr}"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);

    let events = read_events(&trail);
    let outcome = events.last().expect("interrupted outcome event");
    assert_eq!(outcome["event"], "response_error");
    assert_eq!(outcome["kind"], "interrupted");
    let tool_result = events
        .iter()
        .find(|event| event["event"] == "tool_result")
        .expect("interrupted bash result");
    assert_eq!(tool_result["status"], "failed");
    assert!(
        tool_result["error"]
            .as_str()
            .expect("tool error")
            .contains(&format!("interrupted by {signal_name}"))
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[cfg(unix)]
#[test]
fn sigterm_stops_bash_and_records_interrupted_outcome() {
    assert_exchange_interrupt(libc::SIGTERM, 143, "SIGTERM", "exchange-sigterm");
}

#[cfg(unix)]
#[test]
fn sigint_stops_bash_and_records_interrupted_outcome() {
    assert_exchange_interrupt(libc::SIGINT, 130, "SIGINT", "exchange-sigint");
}

#[cfg(unix)]
#[test]
fn interrupted_session_trail_can_be_resumed() {
    let (base_url, _) = spawn_sequence_server(&SLEEP_TOOL_ROUNDS);
    let cwd = fixture_dir("session-interrupted");
    let trail = cwd.join("session.jsonl");

    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail).arg("session");
    let output = run_until_bash_signal(
        cmd,
        &cwd,
        Some("run the long command\n"),
        true,
        libc::SIGTERM,
    );
    assert_eq!(output.status.code(), Some(143));
    assert!(output.stdout.is_empty(), "interrupted session wrote stdout");

    let initial_events = read_events(&trail);
    assert_eq!(
        initial_events.last().expect("interrupted outcome")["kind"],
        "interrupted"
    );
    assert!(
        !initial_events
            .iter()
            .any(|event| event["event"] == "session_end"),
        "interrupted session must not be marked cleanly ended"
    );
    let session_id = initial_events[0]["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    let (resume_url, requests) = spawn_sequence_server(&ONE_TEXT_REPLY);
    let mut resume = leg(&cwd, &resume_url);
    resume.args(["session", "--resume"]).arg(&trail);
    let resumed = run(resume, Some("continue after interrupt\n/exit\n"));
    assert!(
        resumed.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&resumed.stdout), "hi there\n");

    let requests: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_str(body).expect("request body is JSON"))
        .collect();
    assert_eq!(requests.len(), 1);
    let messages = requests[0]["messages"].as_array().expect("message history");
    assert_eq!(messages.len(), 1, "interrupted turn must not enter history");
    assert_eq!(messages[0]["content"], "continue after interrupt");

    let events = read_events(&trail);
    assert!(events.iter().any(|event| {
        event["event"] == "response_error"
            && event["kind"] == "interrupted"
            && event["session_id"] == session_id
            && event["turn_index"] == 0
    }));
    assert!(events.iter().any(|event| {
        event["event"] == "request" && event["session_id"] == session_id && event["turn_index"] == 1
    }));
    assert!(
        events
            .iter()
            .any(|event| { event["event"] == "session_end" && event["session_id"] == session_id })
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[cfg(unix)]
#[test]
fn sigterm_exits_session_while_waiting_for_input() {
    let (base_url, requests, stop, server) = spawn_request_probe();
    let cwd = fixture_dir("session-idle-sigterm");
    let trail = cwd.join("session.jsonl");
    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail).arg("session");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg");
    let leg_pid = child.id().try_into().expect("leg pid fits pid_t");
    let mut cleanup = SignalTestCleanup {
        leg_pid,
        shell_pid_file: cwd.join("shell.pid"),
        sleep_pid_file: cwd.join("sleep.pid"),
        active: true,
    };
    let stdin = child.stdin.take().expect("piped stdin");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&trail)
            .is_ok_and(|contents| contents.contains("\"event\":\"session_start\""))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session did not start before the deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(50));

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    assert_eq!(
        unsafe { libc::kill(leg_pid, libc::SIGTERM) },
        0,
        "send SIGTERM to leg"
    );
    let output = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("idle session did not stop within five seconds")
        .expect("wait for leg");
    drop(stdin);
    stop.send(()).expect("stop request probe");
    server.join().expect("join request probe");
    cleanup.active = false;

    assert_eq!(output.status.code(), Some(143));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted by SIGTERM"));
    assert!(requests.lock().unwrap().is_empty());
    let events = read_events(&trail);
    assert!(
        !events.iter().any(|event| event["event"] == "session_end"),
        "interrupted session must not be marked cleanly ended"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

#[cfg(unix)]
#[test]
fn sigterm_abandons_an_in_flight_provider_request() {
    let cwd = fixture_dir("provider-sigterm");
    let trail = cwd.join("trail.jsonl");
    let (base_url, request_received, server) = spawn_stalled_server();
    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail)
        .args(["ask", "wait for the provider"]);
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg");
    let leg_pid = child.id().try_into().expect("leg pid fits pid_t");
    let mut cleanup = SignalTestCleanup {
        leg_pid,
        shell_pid_file: cwd.join("shell.pid"),
        sleep_pid_file: cwd.join("sleep.pid"),
        active: true,
    };
    request_received
        .recv_timeout(Duration::from_secs(5))
        .expect("provider request did not arrive");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    assert_eq!(
        unsafe { libc::kill(leg_pid, libc::SIGTERM) },
        0,
        "send SIGTERM to leg"
    );
    let result = rx.recv_timeout(Duration::from_secs(5));
    drop(server);
    let output = result
        .expect("provider request was not abandoned within five seconds")
        .expect("wait for leg");
    cleanup.active = false;

    assert_eq!(output.status.code(), Some(143));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted by SIGTERM"));
    let events = read_events(&trail);
    assert_eq!(
        events.last().expect("interrupted outcome")["event"],
        "response_error"
    );
    assert_eq!(events.last().unwrap()["kind"], "interrupted");

    std::fs::remove_dir_all(&cwd).ok();
}

#[cfg(unix)]
#[test]
fn second_signal_exits_immediately_and_kills_bash_process_group() {
    let (base_url, requests) = spawn_sequence_server(&SECOND_SIGNAL_ROUNDS);
    let cwd = fixture_dir("exchange-second-signal");
    let trail = cwd.join("trail.jsonl");
    let mut cmd = leg(&cwd, &base_url);
    cmd.env("LEG_EVENT_LOG", &trail).arg("exchange");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg");
    let leg_pid = child.id().try_into().expect("leg pid fits pid_t");
    let shell_pid_file = cwd.join("shell.pid");
    let sleep_pid_file = cwd.join("sleep.pid");
    let mut cleanup = SignalTestCleanup {
        leg_pid,
        shell_pid_file: shell_pid_file.clone(),
        sleep_pid_file: sleep_pid_file.clone(),
        active: true,
    };
    let request = serde_json::json!({
        "schema": "baton.message/v1",
        "message_id": "m-second-signal",
        "conversation_id": "c-second-signal",
        "from": "external",
        "to": "leg",
        "in_reply_to": null,
        "kind": "request",
        "body": "run the long command",
        "ts_ms": 1_700_000_000_000_u64,
        "exchange": null
    })
    .to_string();
    let mut stdin = child.stdin.take();
    stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(request.as_bytes())
        .expect("write leg input");
    stdin.take();

    let deadline = Instant::now() + Duration::from_secs(10);
    let shell_pid = loop {
        if let (Some(shell_pid), Some(sleep_pid)) = (
            read_pid_file(&shell_pid_file),
            read_pid_file(&sleep_pid_file),
        ) && shell_pid == sleep_pid
            && process_is_running(sleep_pid)
        {
            break shell_pid;
        }
        assert!(
            Instant::now() < deadline,
            "bash did not start the SIGTERM-ignoring sleep"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    assert_eq!(
        unsafe { libc::kill(leg_pid, libc::SIGTERM) },
        0,
        "send first signal to leg"
    );
    let second_signal_started = Instant::now();
    assert_eq!(
        unsafe { libc::kill(leg_pid, libc::SIGINT) },
        0,
        "send second signal to leg"
    );
    let output = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second signal did not exit immediately")
        .expect("wait for leg");
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    while process_is_running(shell_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_is_running(shell_pid),
        "SIGTERM-ignoring sleep process {shell_pid} survived the second signal"
    );
    assert!(
        second_signal_started.elapsed() <= Duration::from_secs(1),
        "second signal did not exit immediately"
    );
    assert!(
        [130, 143].contains(&output.status.code().expect("signal exit code")),
        "expected signal-specific exit code, got {:?}",
        output.status.code()
    );
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.is_empty(),
        "immediate exit should bypass diagnostics"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);

    cleanup.active = false;
    std::fs::remove_dir_all(&cwd).ok();
}
