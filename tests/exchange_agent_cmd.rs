//! Hermetic proof of the `leg exchange` <-> `ExternalAgentParticipant` contract.
//!
//! Spawns the *compiled* `leg` binary against a hand-rolled HTTP/1.1 mock
//! standing in for the Anthropic endpoint, driving it exactly the way
//! baton's `capture_child_output` does: write the request body to the
//! child's stdin, close stdin, drain stdout to EOF, then check the exit
//! code. No baton checkout is required for this proof — see
//! `scripts/external-agent-proof.sh` for the real cross-repo run.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// Starts a one-shot HTTP/1.1 mock server on an OS-assigned port, returning
/// its base URL. It accepts exactly one connection, reads the request
/// headers, then drains exactly `Content-Length` request-body bytes (the
/// body content itself is irrelevant to this proof) before replying with
/// `status`/`body` — closing the connection before the client has finished
/// writing its request body aborts the write on some platforms (observed on
/// Windows/ureq), so the full body must be consumed first.
fn spawn_mock_server(status: u16, body: &'static str) -> String {
    spawn_mock_server_with_capture(status, body).0
}

/// Starts the same one-shot mock while retaining the provider request body.
fn spawn_mock_server_with_capture(
    status: u16,
    body: &'static str,
) -> (String, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let captured = Arc::new(Mutex::new(None));
    let captured_for_thread = Arc::clone(&captured);

    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 8192];
        let mut received = Vec::new();
        let header_end = loop {
            if let Some(pos) = received
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
            {
                break pos;
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
        };

        let headers = String::from_utf8_lossy(&received[..header_end]);
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or(line.strip_prefix("content-length:"))
            })
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);

        let mut request_body = received[header_end..].to_vec();
        while request_body.len() < content_length {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => request_body.extend_from_slice(&buf[..n]),
            }
        }
        request_body.truncate(content_length);
        *captured_for_thread.lock().expect("capture mutex") =
            Some(String::from_utf8_lossy(&request_body).into_owned());

        let reason = if status == 200 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });

    (format!("http://{addr}"), captured)
}

/// Spawns `leg exchange` against `base_url`, feeds `stdin_body` on stdin
/// exactly as `ExternalAgentParticipant` does (write, then close), and
/// returns `(exit_success, stdout, stderr)`.
fn run_leg_exchange(base_url: &str, stdin_body: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_leg"))
        .arg("exchange")
        .env("LEG_HOOKS_CONFIG", empty_hooks_config_path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("LEG_MODEL", "claude-test-model")
        .env("LEG_TIMEOUT_SECS", "5")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg exchange");

    // The taken `ChildStdin` is a temporary: it drops (closing the pipe,
    // signalling EOF) at the end of this statement, mirroring
    // `capture_child_output`'s write-then-close-stdin sequencing.
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin_body.as_bytes())
        .expect("write stdin");

    let output = child
        .wait_timeout_or_kill(Duration::from_secs(10))
        .expect("child did not hang");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Returns a fresh, process-and-call-unique scratch directory under the OS
/// temp dir (no `tempfile` dependency warranted for two tests).
fn unique_scratch_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "leg-exchange-test-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Spawns `leg exchange --in <in_path> --out <out_path>` against `base_url`,
/// with no stdin/stdout piping (the file paths carry the request/response
/// instead), and returns `(exit_success, stderr)`.
fn run_leg_exchange_files(
    base_url: &str,
    in_path: &std::path::Path,
    out_path: &std::path::Path,
) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_leg"))
        .arg("exchange")
        .arg("--in")
        .arg(in_path)
        .arg("--out")
        .arg(out_path)
        .env("LEG_HOOKS_CONFIG", empty_hooks_config_path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("LEG_MODEL", "claude-test-model")
        .env("LEG_TIMEOUT_SECS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg exchange");

    let output = child
        .wait_timeout_or_kill(Duration::from_secs(10))
        .expect("child did not hang");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs one exchange in a caller-selected workspace and session store.
fn run_leg_exchange_in_dir(
    base_url: &str,
    args: &[&str],
    cwd: &std::path::Path,
    session_dir: &std::path::Path,
    system_prompt: Option<&std::path::Path>,
    stdin_body: &str,
) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leg"));
    command
        .args(args)
        .env("LEG_HOOKS_CONFIG", empty_hooks_config_path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("LEG_MODEL", "claude-test-model")
        .env("LEG_TIMEOUT_SECS", "5")
        .env("LEG_CONTEXT_WINDOW_TOKENS", "200000")
        .env("LEG_SESSION_DIR", session_dir)
        .env_remove("LEG_EVENT_LOG")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match system_prompt {
        Some(path) => {
            command.env("LEG_SYSTEM_PROMPT", path);
        }
        None => {
            command.env_remove("LEG_SYSTEM_PROMPT");
        }
    }
    let mut child = command.spawn().expect("spawn leg exchange");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin_body.as_bytes())
        .expect("write stdin");

    let output = child
        .wait_timeout_or_kill(Duration::from_secs(10))
        .expect("child did not hang");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// An explicit empty config keeps subprocess tests independent of a developer's
/// project or per-user hook files. Hook-specific tests can override this env var.
fn empty_hooks_config_path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "leg-exchange-empty-hooks-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, r#"{"version":1,"hooks":[]}"#)
            .expect("write isolated empty hooks config");
        path
    })
    .clone()
}

/// Minimal `wait-with-timeout`: `std::process::Child` has no built-in
/// deadline, and pulling in a crate for one test is not warranted, so this
/// polls `try_wait` — the child is a short-lived local process, not a hot
/// loop concern.
///
/// stdout/stderr are drained on their own threads *while* the child runs, not
/// after it exits — these fixtures are tiny, but draining only post-exit
/// would deadlock a child that blocks writing to a full pipe before exiting.
trait WaitTimeoutOrKill {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<std::process::Output>;
}

impl WaitTimeoutOrKill for std::process::Child {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<std::process::Output> {
        let stdout_reader = self.stdout.take().map(|mut out| {
            thread::spawn(move || -> Vec<u8> {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                buf
            })
        });
        let stderr_reader = self.stderr.take().map(|mut err| {
            thread::spawn(move || -> Vec<u8> {
                let mut buf = Vec::new();
                let _ = err.read_to_end(&mut buf);
                buf
            })
        });

        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.try_wait()?.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.kill();
                self.wait()?;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        let status = self.wait()?;
        let stdout = stdout_reader
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr = stderr_reader
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }
}

#[test]
fn plain_text_success_writes_only_the_raw_reply_body() {
    let base_url = spawn_mock_server(
        200,
        r#"{"content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn"}"#,
    );

    let (success, stdout, _stderr) = run_leg_exchange(&base_url, "hello");

    assert!(success, "leg exchange should exit 0 on a successful reply");
    assert_eq!(stdout, "hi there\n");
}

#[test]
fn plain_text_provider_failure_leaves_stdout_empty_and_exits_nonzero() {
    let base_url = spawn_mock_server(
        401,
        r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    );

    let (success, stdout, stderr) = run_leg_exchange(&base_url, "hello");

    assert!(
        !success,
        "a provider failure must exit non-zero so callers can detect it"
    );
    assert!(
        stdout.is_empty(),
        "plain-text failure must leave stdout empty; got {stdout:?}"
    );
    assert!(
        stderr.contains("invalid x-api-key"),
        "the diagnostic must still be observable on stderr; got {stderr:?}"
    );
    assert!(
        stderr.contains("authentication_error"),
        "stderr must include the provider error type; got {stderr:?}"
    );
    assert!(
        stderr.contains("kind: error"),
        "stderr must identify the failed message kind; got {stderr:?}"
    );
}

#[test]
fn envelope_mode_provider_failure_preserves_error_envelope_and_exits_nonzero() {
    let base_url = spawn_mock_server(
        401,
        r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    );
    let request = r#"{
        "schema": "baton.message/v1",
        "message_id": "m-1",
        "conversation_id": "c-1",
        "from": "external",
        "to": "leg",
        "in_reply_to": null,
        "kind": "request",
        "body": "hello",
        "ts_ms": 1700000000000,
        "exchange": null
    }"#;

    let (success, stdout, stderr) = run_leg_exchange(&base_url, request);

    assert!(
        !success,
        "an envelope-mode provider failure must exit non-zero; stderr: {stderr}"
    );
    let response: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("error response envelope is JSON");
    assert_eq!(response["kind"], "error");
    assert_eq!(
        response["body"],
        "authentication error: authentication_error: invalid x-api-key"
    );
    assert!(
        stderr.contains("kind: error"),
        "stderr must identify the failed message kind; got {stderr:?}"
    );
}

#[test]
fn envelope_mode_round_trips_through_in_and_out_files() {
    let base_url = spawn_mock_server(
        200,
        r#"{"content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn"}"#,
    );

    let dir = unique_scratch_dir("envelope-files");
    let in_path = dir.join("request.json");
    let out_path = dir.join("response.json");

    let request = r#"{
        "schema": "baton.message/v1",
        "message_id": "m-1",
        "conversation_id": "c-1",
        "from": "external",
        "to": "leg",
        "in_reply_to": null,
        "kind": "request",
        "body": "hello",
        "ts_ms": 1700000000000,
        "exchange": null
    }"#;
    std::fs::write(&in_path, request).expect("write request file");

    let (success, stderr) = run_leg_exchange_files(&base_url, &in_path, &out_path);
    assert!(
        success,
        "leg exchange --in/--out should exit 0; stderr: {stderr}"
    );

    let response_raw = std::fs::read_to_string(&out_path).expect("read response file");
    let response: serde_json::Value =
        serde_json::from_str(&response_raw).expect("response file is valid JSON");

    assert_eq!(response["schema"], "baton.message/v1");
    assert_eq!(response["kind"], "response");
    assert_eq!(response["in_reply_to"], "m-1");
    assert_eq!(response["body"], "hi there");
    assert_eq!(response["exchange"]["schema"], "baton.exchange/v1");
    assert_eq!(
        response["exchange"]["exchange"]["outcome"]["event"],
        "response_ok"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resumed_exchange_rebuilds_current_agents_prompt_without_persisting_it() {
    let dir = unique_scratch_dir("agents-resume");
    let nested = dir.join("nested");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&nested).expect("create nested workspace");
    std::fs::create_dir_all(&sessions).expect("create session store");
    let git_status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .expect("run git init");
    assert!(git_status.success(), "git init failed");

    let system_prompt = dir.join("caller-system.md");
    std::fs::write(&system_prompt, "caller rules").expect("write caller prompt");
    let root_agents = dir.join("AGENTS.md");
    let nested_agents = nested.join("AGENTS.md");
    std::fs::write(&root_agents, "root rules old").expect("write root instructions");
    std::fs::write(&nested_agents, "nested rules old").expect("write nested instructions");

    let first_url = spawn_mock_server_with_capture(
        200,
        r#"{"content":[{"type":"text","text":"first"}],"stop_reason":"end_turn"}"#,
    );
    let (first_url, first_capture) = first_url;
    let (first_success, first_stdout, first_stderr) = run_leg_exchange_in_dir(
        &first_url,
        &["exchange", "--new-session-id", "sess-219-1"],
        &nested,
        &sessions,
        Some(&system_prompt),
        "first turn",
    );
    assert!(first_success, "first exchange failed: {first_stderr}");
    assert_eq!(first_stdout, "first\n");
    let first_body = first_capture
        .lock()
        .expect("first capture mutex")
        .clone()
        .expect("first request body");
    let first_json: serde_json::Value =
        serde_json::from_str(&first_body).expect("first request JSON");
    assert_eq!(
        first_json["system"],
        "caller rules\n\nroot rules old\n\nnested rules old"
    );

    std::fs::write(&root_agents, "root rules new").expect("update root instructions");
    std::fs::write(&nested_agents, "nested rules new").expect("update nested instructions");

    let second_url = spawn_mock_server_with_capture(
        200,
        r#"{"content":[{"type":"text","text":"second"}],"stop_reason":"end_turn"}"#,
    );
    let (second_url, second_capture) = second_url;
    let (second_success, second_stdout, second_stderr) = run_leg_exchange_in_dir(
        &second_url,
        &["exchange", "--session", "sess-219-1"],
        &nested,
        &sessions,
        Some(&system_prompt),
        "second turn",
    );
    assert!(second_success, "resumed exchange failed: {second_stderr}");
    assert_eq!(second_stdout, "second\n");
    let second_body = second_capture
        .lock()
        .expect("second capture mutex")
        .clone()
        .expect("second request body");
    let second_json: serde_json::Value =
        serde_json::from_str(&second_body).expect("second request JSON");
    assert_eq!(
        second_json["system"],
        "caller rules\n\nroot rules new\n\nnested rules new"
    );

    let trail =
        std::fs::read_to_string(sessions.join("sess-219-1.jsonl")).expect("read session trail");
    assert!(!trail.contains("caller rules"));
    assert!(!trail.contains("root rules old"));
    assert!(!trail.contains("nested rules old"));
    assert!(!trail.contains("root rules new"));
    assert!(!trail.contains("nested rules new"));

    let _ = std::fs::remove_dir_all(&dir);
}
