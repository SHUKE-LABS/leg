//! The guest integration test for the capability bridge.
//!
//! The bridge is driven the way a mobile Skill drives it: through the
//! existing `bash` tool, with one request on standard input. The host side of
//! the transport is played by the test, which answers through the directory
//! the bridge and the host share.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use leg::bridge::adapter::response_file;
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
fn bridge_command(host_dir: &Path, state_dir: &Path, request: &str) -> String {
    format!(
        "LEG_BRIDGE_HOST_DIR={} LEG_BRIDGE_STATE_DIR={} {} <<'BRIDGE_REQUEST'\n{}\nBRIDGE_REQUEST\n",
        shell_quote(&host_dir.to_string_lossy()),
        shell_quote(&state_dir.to_string_lossy()),
        shell_quote(&shell_path(Path::new(BRIDGE))),
        request.trim_end(),
    )
}

/// Runs one request through the `bash` tool and returns the bridge's answer.
fn ask(host_dir: &Path, state_dir: &Path, request: &str) -> Value {
    run_command(&bridge_command(host_dir, state_dir, request))
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

/// Answers the bridge the way a host does.
///
/// The host watches the shared directory for the request the bridge writes and
/// answers it in the file that carries the same request id.
fn spawn_host(
    host_dir: PathBuf,
    answer: impl Fn(&Value) -> Value + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // A request left over from an earlier exchange is not this one's.
        for stale in fs::read_dir(&host_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bridge-request-"))
            })
        {
            let _ = fs::remove_file(stale);
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut request_path = None;
        while Instant::now() < deadline && request_path.is_none() {
            request_path = fs::read_dir(&host_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("bridge-request-"))
                });
            if request_path.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let Some(request_path) = request_path else {
            return;
        };
        let request: Value =
            serde_json::from_str(&fs::read_to_string(&request_path).expect("read the request"))
                .expect("the request is JSON");
        let request_id = request["request_id"]
            .as_str()
            .expect("the request has an id");
        let response_path = host_dir.join(response_file(request_id));
        let reply = answer(&request);
        fs::write(&response_path, reply.to_string()).expect("write the answer");
    })
}

/// The request files the host can see in the shared directory.
fn host_request_files(host_dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(host_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("bridge-request-"))
        })
        .collect()
}

/// Builds the host's answer for one request.
fn reply(request: &Value, status: &str, result: Value) -> Value {
    json!({
        "version": 1,
        "nonce": request["nonce"],
        "request_id": request["request_id"],
        "status": status,
        "result": result,
        "error": null,
    })
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

#[test]
fn the_guest_reaches_the_capability_through_the_bash_tool() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("success");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let host = spawn_host(host_dir.clone(), |request| {
        reply(request, "ok", json!({"items": ["summer.jpg"]}))
    });

    let answer = ask(
        &host_dir,
        &state_dir,
        &request("r-1", "files.list", json!({"limit": 5})),
    );
    host.join().expect("the host thread finished");

    assert_eq!(answer["version"], 1);
    assert_eq!(answer["request_id"], "r-1");
    assert_eq!(answer["status"], "ok");
    assert_eq!(answer["result"]["items"], json!(["summer.jpg"]));
    assert!(answer["error"].is_null());
}

#[test]
fn a_malformed_request_never_reaches_the_host() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("malformed");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let answer = ask(&host_dir, &state_dir, "{\"version\":1,");

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "malformed_request");
    assert!(
        host_request_files(&host_dir).is_empty(),
        "the malformed request reached the host"
    );
}

#[test]
fn unknown_versions_capabilities_and_paths_never_reach_the_host() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("unknown");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

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
        let answer = ask(&host_dir, &state_dir, &line.to_string());
        assert_eq!(answer["status"], "error");
        assert_eq!(answer["error"]["reason"], reason);
    }

    assert!(
        host_request_files(&host_dir).is_empty(),
        "a refused request reached the host"
    );
}

#[test]
fn an_oversized_request_never_reaches_the_host() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("oversized");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    // The payload goes through a file: a command line cannot carry it.
    let payload = scratch.path.join("oversized.ndjson");
    fs::write(&payload, "x".repeat(64 * 1024 + 1)).expect("write the oversized request");
    let command = format!(
        "export LEG_BRIDGE_HOST_DIR={} LEG_BRIDGE_STATE_DIR={}; cat {} | {}",
        shell_quote(&host_dir.to_string_lossy()),
        shell_quote(&state_dir.to_string_lossy()),
        shell_quote(&shell_path(&payload)),
        shell_quote(&shell_path(Path::new(BRIDGE))),
    );
    let answer = run_command(&command);

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "request_too_large");
    assert!(
        host_request_files(&host_dir).is_empty(),
        "the oversized request reached the host"
    );
}

#[test]
fn a_request_id_is_used_once() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("duplicate");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let host = spawn_host(host_dir.clone(), |request| {
        reply(request, "ok", json!({"items": []}))
    });
    let first = ask(
        &host_dir,
        &state_dir,
        &request("r-1", "files.list", json!({})),
    );
    host.join().expect("the host thread finished");
    assert_eq!(first["status"], "ok");

    let second = ask(
        &host_dir,
        &state_dir,
        &request("r-1", "files.list", json!({})),
    );
    assert_eq!(second["status"], "error");
    assert_eq!(second["error"]["reason"], "duplicate_request_id");
}

#[test]
fn an_approval_the_user_refuses_is_answered_denied() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("denied");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let host = spawn_host(host_dir.clone(), |request| {
        if request["preview"] == json!(true) {
            reply(
                request,
                "approval_required",
                json!({
                    "approval_token": "token-0123456789abcdef",
                    "review": {"summary": "reads the photo library"},
                }),
            )
        } else {
            reply(request, "denied", json!(null))
        }
    });

    let preview = ask(
        &host_dir,
        &state_dir,
        &request("r-1", "photos.list", json!({"limit": 1})),
    );
    assert_eq!(preview["status"], "approval_required");
    let token = preview["result"]["approval_token"]
        .as_str()
        .expect("the answer carries a token")
        .to_string();
    host.join().expect("the host thread finished");

    let host = spawn_host(host_dir.clone(), |request| {
        reply(request, "denied", json!(null))
    });
    let replay = json!({
        "version": 1,
        "request_id": "r-1",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": token,
    })
    .to_string();
    let answer = ask(&host_dir, &state_dir, &replay);
    host.join().expect("the host thread finished");

    assert_eq!(answer["status"], "denied");
    assert_eq!(answer["request_id"], "r-1");
}

#[test]
fn a_cancelled_request_is_answered_cancelled() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("cancelled");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let host = spawn_host(host_dir.clone(), |request| {
        reply(request, "cancelled", json!(null))
    });

    let answer = ask(
        &host_dir,
        &state_dir,
        &request("r-1", "files.list", json!({})),
    );
    host.join().expect("the host thread finished");

    assert_eq!(answer["status"], "cancelled");
    assert_eq!(answer["error"]["reason"], "cancelled");
}

#[test]
fn a_request_the_host_ignores_times_out() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("timeout");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let line = json!({
        "version": 1,
        "request_id": "r-1",
        "capability": "files.list",
        "input": {},
        "timeout_ms": 500,
    })
    .to_string();

    let started = Instant::now();
    let answer = ask(&host_dir, &state_dir, &line);
    let elapsed = started.elapsed();

    assert_eq!(answer["status"], "error");
    assert_eq!(answer["error"]["reason"], "timeout");
    assert!(
        elapsed < Duration::from_secs(30),
        "the request did not stop at its deadline: {elapsed:?}"
    );
}

#[test]
fn a_guest_without_a_host_is_answered_unavailable() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("unavailable");
    let state_dir = scratch.path.join("state");

    // No host directory is configured, so the bridge has no transport.
    let command = format!(
        "LEG_BRIDGE_STATE_DIR={} {} <<'BRIDGE_REQUEST'\n{}\nBRIDGE_REQUEST\n",
        shell_quote(&state_dir.to_string_lossy()),
        shell_quote(&shell_path(Path::new(BRIDGE))),
        request("r-1", "files.list", json!({})),
    );
    let output = BashTool::new()
        .call(&json!({"command": command, "timeout": 60}))
        .expect("the bash tool ran the bridge");
    let output: Value = serde_json::from_str(&output).expect("the tool result is JSON");
    let answer: Value = serde_json::from_str(output["stdout"].as_str().expect("stdout"))
        .expect("the answer is JSON");

    assert_eq!(answer["status"], "unavailable");
    assert_eq!(answer["error"]["reason"], "no_host_transport");
}

#[test]
fn the_bridge_keeps_the_request_values_out_of_its_diagnostics() {
    if !bash_available() {
        return;
    }
    let scratch = scratch("secrets");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");
    let state_dir = scratch.path.join("state");

    let secret = "s3cr3t-photo-contents";
    let host = spawn_host(host_dir.clone(), move |request| {
        reply(
            request,
            "approval_required",
            json!({
                "approval_token": "token-0123456789abcdef",
                "review": {"summary": format!("reads {secret}")},
            }),
        )
    });

    let answer = ask(
        &host_dir,
        &state_dir,
        &request(
            "r-1",
            "shortcuts.invoke",
            json!({"name": "morning", "input": {"photo": secret}}),
        ),
    );
    host.join().expect("the host thread finished");
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
