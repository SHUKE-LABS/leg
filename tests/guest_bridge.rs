//! The guest integration test for the capability bridge.
//!
//! The bridge is driven the way a mobile Skill drives it: through the
//! existing `bash` tool, with one request on standard input. The host side of
//! the transport is played by the test, which listens on the Unix socket the
//! bridge connects to.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(unix)]
use std::time::Instant;

use leg::tools::{BashTool, ToolHandler};
use serde_json::{Value, json};

/// The bridge binary the guest calls.
const BRIDGE: &str = env!("CARGO_BIN_EXE_leg-bridge");

/// A scratch directory removed when the test ends.
struct Scratch {
    /// The scratch directory itself.
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Creates a scratch directory named after the test.
fn scratch(name: &str) -> Scratch {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "leg-bridge-guest-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch directory");
    Scratch { path }
}

/// Whether the test host can run the `bash` tool.
fn bash_available() -> bool {
    let mut probe = Command::new("bash");
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH") {
        probe.env("PATH", path);
    }
    probe
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The path a shell can use to reach `path`.
///
/// Git Bash needs the MSYS form of a Windows path to run a binary; the
/// environment variables keep the native form, because the bridge reads them
/// as the operating system paths they are.
fn shell_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    {
        let normalized = value.replace('\\', "/");
        if let Some((drive, rest)) = normalized.split_once(":/") {
            return format!("/{}/{}", drive.to_ascii_lowercase(), rest);
        }
        normalized
    }
    #[cfg(not(windows))]
    {
        value.into_owned()
    }
}

/// Quotes a value for the shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Builds the command a guest Skill runs to reach the bridge.
///
/// The host socket is named by the environment, never by the request.
fn bridge_command(socket: Option<&Path>, state_dir: &Path, request: &str) -> String {
    let transport = match socket {
        Some(socket) => format!(
            "LEG_BRIDGE_HOST_SOCKET={} ",
            shell_quote(&socket.to_string_lossy())
        ),
        None => String::new(),
    };
    format!(
        "{transport}LEG_BRIDGE_STATE_DIR={} {} <<'BRIDGE_REQUEST'\n{}\nBRIDGE_REQUEST\n",
        shell_quote(&state_dir.to_string_lossy()),
        shell_quote(&shell_path(Path::new(BRIDGE))),
        request.trim_end(),
    )
}

/// Runs one request through the `bash` tool and returns the bridge's answer.
fn ask(socket: Option<&Path>, state_dir: &Path, request: &str) -> Value {
    run_command(&bridge_command(socket, state_dir, request))
}

/// Runs one command through the `bash` tool and returns the bridge's answer.
fn run_command(command: &str) -> Value {
    let tool = BashTool::new();
    let output = tool
        .call(&json!({
            "command": command,
            "description": "call the capability bridge",
            "timeout": 60,
        }))
        .expect("the bash tool ran the bridge");
    let output: Value = serde_json::from_str(&output).expect("the tool result is JSON");
    assert_eq!(
        output["exit_code"], 0,
        "the bridge failed: {}",
        output["stderr"]
    );
    let stdout = output["stdout"]
        .as_str()
        .expect("the bridge wrote to stdout");
    let line = stdout.trim();
    assert!(!line.is_empty(), "the bridge wrote no answer");
    serde_json::from_str(line).expect("the answer is JSON")
}

/// A request the bridge accepts.
fn request(id: &str, capability: &str, input: Value) -> String {
    json!({
        "version": 1,
        "request_id": id,
        "capability": capability,
        "input": input,
    })
    .to_string()
}

/// The host the fixture plays.
///
/// The host listens on a Unix socket, reads the request the bridge writes,
/// and answers it the way the test asks for.
#[cfg(unix)]
mod host {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    /// What the fixture host does with a request.
    pub enum Answer {
        /// Answer with this status and result.
        Reply {
            /// The status the host answers with.
            status: String,
            /// The result the host answers with.
            result: Value,
        },
        /// Answer with this status and failure.
        Failure {
            /// The status the host answers with.
            status: String,
            /// The failure the host answers with.
            reason: String,
            /// The detail the host answers with.
            message: String,
        },
        /// Close the connection without answering.
        Crash,
        /// Stay silent until the test ends.
        Silent,
        /// Write these bytes as the answer.
        Raw(Vec<u8>),
    }

    /// The fixture host.
    pub struct Fixture {
        /// The socket the bridge connects to.
        pub path: std::path::PathBuf,
        /// How many requests the host received.
        pub connections: Arc<AtomicUsize>,
        /// The requests the host received.
        pub seen: Arc<Mutex<Vec<Value>>>,
        /// The thread the host runs in.
        handle: Option<JoinHandle<()>>,
        /// The flag that tells the host to stop.
        stop: Arc<AtomicBool>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl Fixture {
        /// Starts a host that answers every request with `answer`.
        pub fn start(
            path: std::path::PathBuf,
            answer: impl Fn(&Value) -> Answer + Send + 'static,
        ) -> Self {
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind the host socket");
            listener
                .set_nonblocking(true)
                .expect("the host socket polls for requests");

            let connections = Arc::new(AtomicUsize::new(0));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));

            let handle = {
                let connections = Arc::clone(&connections);
                let seen = Arc::clone(&seen);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                connections.fetch_add(1, Ordering::Relaxed);
                                serve(stream, &answer, &seen, &stop);
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };

            Self {
                path,
                connections,
                seen,
                handle: Some(handle),
                stop,
            }
        }

        /// How many requests the host received.
        pub fn connection_count(&self) -> usize {
            self.connections.load(Ordering::Relaxed)
        }

        /// The requests the host received.
        pub fn requests(&self) -> Vec<Value> {
            self.seen.lock().expect("the host lock").clone()
        }
    }

    /// Answers one request the way the fixture says.
    fn serve(
        stream: UnixStream,
        answer: &impl Fn(&Value) -> Answer,
        seen: &Arc<Mutex<Vec<Value>>>,
        stop: &Arc<AtomicBool>,
    ) {
        let _ = stream.set_nonblocking(false);
        let mut writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(_) => return,
        };
        let mut reader = BufReader::new(stream);

        let mut line = String::new();
        // The accepted socket may still be in non-blocking mode on some
        // platforms, so the fixture waits for the request line instead of
        // giving up on the first refusal.
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(5)));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) if line.ends_with('\n') => break,
                Ok(_) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
        if line.trim().is_empty() {
            return;
        }
        let request: Value = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(_) => return,
        };
        seen.lock().expect("the host lock").push(request.clone());

        match answer(&request) {
            Answer::Reply { status, result } => {
                let reply = json!({
                    "version": 1,
                    "nonce": request["nonce"],
                    "request_id": request["request_id"],
                    "status": status,
                    "result": result,
                    "error": null,
                });
                let _ = writeln!(writer, "{reply}");
                let _ = writer.flush();
            }
            Answer::Failure {
                status,
                reason,
                message,
            } => {
                let reply = json!({
                    "version": 1,
                    "nonce": request["nonce"],
                    "request_id": request["request_id"],
                    "status": status,
                    "result": null,
                    "error": {"reason": reason, "message": message},
                });
                let _ = writeln!(writer, "{reply}");
                let _ = writer.flush();
            }
            Answer::Crash => {}
            Answer::Raw(bytes) => {
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
            }
            Answer::Silent => {
                // The host says nothing until the test ends.
                let deadline = Instant::now() + Duration::from_secs(60);
                while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn the_guest_reaches_the_capability_through_the_bash_tool() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("success");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Reply {
            status: "ok".to_string(),
            result: json!({"items": ["summer.jpg"]}),
        }
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(
        socket,
        &state_dir,
        &request("r-1", "files.list", json!({"limit": 5})),
    );

    assert_eq!(answer["version"], 1);
    assert_eq!(answer["request_id"], "r-1");
    assert_eq!(answer["status"], "ok");
    assert_eq!(answer["result"]["items"], json!(["summer.jpg"]));
    assert!(answer["error"].is_null());
}

#[test]
fn a_bad_request_never_reaches_the_host() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("bad-request");
    let state_dir = scratch.path.join("state");

    #[cfg(unix)]
    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Reply {
            status: "ok".to_string(),
            result: json!({}),
        }
    });
    #[cfg(unix)]
    let socket = Some(fixture.path.as_path());
    #[cfg(not(unix))]
    let socket: Option<&Path> = None;

    let cases = [
        (
            json!({"version": 2, "request_id": "r-1", "capability": "files.list", "input": {}}),
            "unsupported_version",
        ),
        (
            json!({"version": 1, "request_id": "r-2", "capability": "photos.delete", "input": {}}),
            "unknown_capability",
        ),
        (
            json!({"version": 1, "request_id": "r-3", "capability": "files.read", "input": {"path": "/etc/passwd"}}),
            "invalid_path",
        ),
        (
            json!({"version": 1, "request_id": "r-4", "capability": "files.list", "input": {}, "command": "rm -rf /"}),
            "unknown_field",
        ),
    ];

    for (line, reason) in cases {
        let answer = ask(socket, &state_dir, &line.to_string());
        assert_eq!(answer["status"], "error");
        assert_eq!(answer["error"]["reason"], reason);
    }

    let malformed = ask(socket, &state_dir, "{\"version\":1,");
    assert_eq!(malformed["status"], "error");
    assert_eq!(malformed["error"]["reason"], "malformed_request");

    #[cfg(unix)]
    assert_eq!(
        fixture.connection_count(),
        0,
        "a refused request reached the host"
    );
}

#[test]
fn an_oversized_request_never_reaches_the_host() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("oversized");
    let state_dir = scratch.path.join("state");

    #[cfg(unix)]
    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Reply {
            status: "ok".to_string(),
            result: json!({}),
        }
    });
    #[cfg(unix)]
    let socket = Some(fixture.path.as_path());
    #[cfg(not(unix))]
    let socket: Option<&Path> = None;

    // The payload goes through a file: a command line cannot carry it.
    let payload = scratch.path.join("oversized.ndjson");
    fs::write(&payload, "x".repeat(64 * 1024 + 1)).expect("write the oversized request");
    let transport = match socket {
        Some(socket) => format!(
            "export LEG_BRIDGE_HOST_SOCKET={}; ",
            shell_quote(&socket.to_string_lossy())
        ),
        None => String::new(),
    };
    let command = format!(
        "{transport}export LEG_BRIDGE_STATE_DIR={}; cat {} | {}",
        shell_quote(&state_dir.to_string_lossy()),
        shell_quote(&shell_path(&payload)),
        shell_quote(&shell_path(Path::new(BRIDGE))),
    );
    let answer = run_command(&command);

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "request_too_large");
    #[cfg(unix)]
    assert_eq!(
        fixture.connection_count(),
        0,
        "the oversized request reached the host"
    );
}

#[cfg(unix)]
#[test]
fn a_request_id_is_used_once() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("duplicate");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Reply {
            status: "ok".to_string(),
            result: json!({"items": []}),
        }
    });
    let socket = Some(fixture.path.as_path());

    let first = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));
    assert_eq!(first["status"], "ok", "the bridge answered: {first}");

    let second = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));
    assert_eq!(second["status"], "error", "the bridge answered: {second}");
    assert_eq!(second["error"]["reason"], "duplicate_request_id");
    assert_eq!(
        fixture.connection_count(),
        1,
        "the duplicate request reached the host"
    );
}

#[cfg(unix)]
#[test]
fn an_approval_the_user_refuses_is_answered_denied() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("denied");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |request| {
        if request["approval_token"].is_string() {
            host::Answer::Failure {
                status: "denied".to_string(),
                reason: "user_refused".to_string(),
                message: "the user refused the request".to_string(),
            }
        } else {
            host::Answer::Reply {
                status: "approval_required".to_string(),
                result: json!({
                    "approval_token": "token-0123456789abcdef",
                    "review": {"summary": "reads the photo library"},
                }),
            }
        }
    });
    let socket = Some(fixture.path.as_path());

    let pending = ask(
        socket,
        &state_dir,
        &request("r-1", "photos.list", json!({"limit": 1})),
    );
    assert_eq!(pending["status"], "approval_required");
    let token = pending["result"]["approval_token"]
        .as_str()
        .expect("the answer carries a token")
        .to_string();

    // The follow-up request carries a fresh request id, the same capability
    // and input, and the token.
    let follow_up = json!({
        "version": 1,
        "request_id": "r-2",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": token,
    })
    .to_string();
    let answer = ask(socket, &state_dir, &follow_up);

    assert_eq!(answer["status"], "denied");
    assert_eq!(answer["request_id"], "r-2");

    // The host received both requests, and the follow-up carried the token.
    #[cfg(unix)]
    {
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["request_id"], "r-1");
        assert!(requests[0]["approval_token"].is_null());
        assert_eq!(requests[1]["request_id"], "r-2");
        assert_eq!(requests[1]["approval_token"], token);
    }
}

#[cfg(unix)]
#[test]
fn a_cancelled_request_is_answered_cancelled() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("cancelled");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Failure {
            status: "cancelled".to_string(),
            reason: "cancelled".to_string(),
            message: "the user cancelled the request".to_string(),
        }
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "cancelled");
    assert_eq!(answer["error"]["reason"], "cancelled");
}

#[cfg(unix)]
#[test]
fn a_request_the_host_ignores_times_out() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("timeout");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Silent
    });
    let socket = Some(fixture.path.as_path());

    let line = json!({
        "version": 1,
        "request_id": "r-1",
        "capability": "files.list",
        "input": {},
        "timeout_ms": 500,
    })
    .to_string();

    let started = Instant::now();
    let answer = ask(socket, &state_dir, &line);
    let elapsed = started.elapsed();

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "timeout");
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "the request did not stop at its deadline: {elapsed:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_host_that_crashes_is_answered_error() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("crash");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Crash
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "host_crashed");
    assert_eq!(
        fixture.connection_count(),
        1,
        "the crashed request was dispatched again"
    );
}

#[cfg(unix)]
#[test]
fn a_permission_the_host_withholds_is_answered_unavailable() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("permission");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Failure {
            status: "unavailable".to_string(),
            reason: "permission_denied".to_string(),
            message: "the host does not grant this capability".to_string(),
        }
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "unavailable");
    assert_eq!(answer["error"]["reason"], "permission_denied");
}

#[test]
fn a_request_that_is_not_text_is_refused() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("not-text");
    let state_dir = scratch.path.join("state");

    // Bytes that are not UTF-8 are a malformed request, not a request with
    // replacement characters in it.
    let command = format!(
        "export LEG_BRIDGE_STATE_DIR={}; printf '\\xff\\xfe\\n' | {}",
        shell_quote(&state_dir.to_string_lossy()),
        shell_quote(&shell_path(Path::new(BRIDGE))),
    );
    let answer = run_command(&command);

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "malformed_request");
}

#[test]
fn a_guest_without_a_host_is_answered_unavailable() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("unavailable");
    let state_dir = scratch.path.join("state");

    // No host socket is configured, so the bridge has no transport.
    let answer = ask(None, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "unavailable");
    assert_eq!(answer["error"]["reason"], "no_host_transport");
}

#[cfg(unix)]
#[test]
fn a_host_that_is_not_listening_is_answered_unavailable() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("absent-host");
    let state_dir = scratch.path.join("state");

    // The socket is configured, but nothing listens on it.
    let socket = scratch.path.join("absent.sock");
    let answer = ask(
        Some(&socket),
        &state_dir,
        &request("r-1", "files.list", json!({})),
    );

    assert_eq!(answer["status"], "unavailable");
    assert_eq!(answer["error"]["reason"], "host_unreachable");
}

#[cfg(unix)]
#[test]
fn a_host_that_sends_a_huge_frame_is_refused() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("huge-frame");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        // A frame that is over the limit, ended in one piece.
        let mut bytes = vec![b'x'; 70 * 1024];
        bytes.push(b'\n');
        host::Answer::Raw(bytes)
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "host_protocol_violation");
    assert_eq!(
        fixture.connection_count(),
        1,
        "the oversized frame was dispatched again"
    );
}

#[cfg(unix)]
#[test]
fn a_host_that_sends_a_frame_that_is_not_text_is_refused() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("frame-not-text");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Raw(vec![0xff, 0xfe, b'\n'])
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "host_crashed");
}

#[cfg(unix)]
#[test]
fn a_host_that_asks_for_an_approval_without_a_summary_is_refused() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("no-review");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Reply {
            status: "approval_required".to_string(),
            result: json!({"approval_token": "token-0123456789abcdef"}),
        }
    });
    let socket = Some(fixture.path.as_path());

    let answer = ask(socket, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "host_protocol_violation");
}

#[test]
fn a_guest_with_a_ledger_it_cannot_read_is_refused() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("foreign-ledger");
    let state_dir = scratch.path.join("state");
    fs::create_dir_all(&state_dir).expect("create the state directory");
    fs::write(
        state_dir.join("state.json"),
        r#"{"schema":"somebody.elses.state/v9","requests":{}}"#,
    )
    .expect("write the foreign ledger");

    // The ledger is not the file the bridge wrote, so the bridge refuses it
    // rather than reading it as its own.
    let answer = ask(None, &state_dir, &request("r-1", "files.list", json!({})));

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "state_unavailable");
    // The failure happened before the request was dispatched, but the caller
    // still learns which request failed.
    assert_eq!(answer["request_id"], "r-1");
}

#[cfg(unix)]
#[test]
fn the_bridge_keeps_the_request_values_out_of_its_diagnostics() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("secrets");
    let state_dir = scratch.path.join("state");

    let fixture = host::Fixture::start(scratch.path.join("host.sock"), |_request| {
        host::Answer::Reply {
            status: "approval_required".to_string(),
            result: json!({
                "approval_token": "token-0123456789abcdef",
                "review": {"summary": "reads the photo library"},
            }),
        }
    });
    let socket = Some(fixture.path.as_path());

    let secret = "s3cr3t-photo-contents";
    let answer = ask(
        socket,
        &state_dir,
        &request(
            "r-1",
            "shortcuts.invoke",
            json!({"name": "morning", "input": {"photo": secret}}),
        ),
    );
    assert_eq!(answer["status"], "approval_required");

    let audit = fs::read_to_string(state_dir.join("audit.log")).expect("the audit summary");
    assert!(audit.contains("r-1"));
    assert!(
        !audit.contains(secret),
        "the audit summary carried a request value: {audit}"
    );
    assert!(
        !audit.contains("morning"),
        "the audit summary carried a request value: {audit}"
    );
}
