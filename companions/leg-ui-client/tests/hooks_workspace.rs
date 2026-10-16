use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use leg_ui_client::{Client, ClientConfig, LegSession, StreamEvent, TurnOutcome, TurnRequest};
use serde_json::{Value, json};

struct EnvGuard(Vec<(OsString, Option<OsString>)>);

impl EnvGuard {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn set(&mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) {
        let key = OsString::from(name);
        if !self.0.iter().any(|(existing, _)| existing == &key) {
            self.0.push((key.clone(), env::var_os(name)));
        }
        unsafe { env::set_var(key, value) };
    }

    fn remove(&mut self, name: &str) {
        let key = OsString::from(name);
        if !self.0.iter().any(|(existing, _)| existing == &key) {
            self.0.push((key.clone(), env::var_os(name)));
        }
        unsafe { env::remove_var(key) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..).rev() {
            if let Some(value) = value {
                unsafe { env::set_var(name, value) };
            } else {
                unsafe { env::remove_var(name) };
            }
        }
    }
}

fn root_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repository root")
        .join("Cargo.toml")
}

fn build_real_leg() -> PathBuf {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest = root_manifest();
    let build = Command::new(&cargo)
        .args(["build", "--locked", "--manifest-path"])
        .arg(&manifest)
        .args(["--bin", "leg"])
        .status()
        .expect("build the native core leg binary");
    assert!(build.success(), "building the native core leg failed");

    let metadata = Command::new(cargo)
        .args([
            "metadata",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .expect("read root Cargo metadata");
    assert!(metadata.status.success(), "root Cargo metadata failed");
    let metadata: Value = serde_json::from_slice(&metadata.stdout).expect("metadata JSON");
    let target_dir = PathBuf::from(metadata["target_directory"].as_str().unwrap());
    let binary = target_dir
        .join("debug")
        .join(if cfg!(windows) { "leg.exe" } else { "leg" });
    assert!(binary.is_file(), "built leg binary missing at {binary:?}");
    binary
}

fn supervisor_binary() -> PathBuf {
    if let Some(path) = env::var_os("CARGO_BIN_EXE_leg-ui-supervisor") {
        return PathBuf::from(path);
    }
    env::current_exe()
        .expect("integration test executable")
        .parent()
        .and_then(Path::parent)
        .expect("Cargo debug directory")
        .join(if cfg!(windows) {
            "leg-ui-supervisor.exe"
        } else {
            "leg-ui-supervisor"
        })
}

fn compile_hook_runner(directory: &Path) -> PathBuf {
    let source = directory.join("hook-runner.rs");
    let executable = directory.join(if cfg!(windows) {
        "hook-runner.exe"
    } else {
        "hook-runner"
    });
    fs::write(
        &source,
        include_str!("../../../tests/fixtures/hook_runner.rs"),
    )
    .expect("write native hook fixture source");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2021")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("compile native hook fixture");
    assert!(
        output.status.success(),
        "failed to compile hook fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

fn text_reply(text: &str) -> String {
    let mut body = String::new();
    body.push_str(&sse_event(
        "message_start",
        json!({ "type": "message_start", "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } } }),
    ));
    body.push_str(&sse_event(
        "content_block_start",
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
    ));
    body.push_str(&sse_event(
        "content_block_delta",
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
    ));
    body.push_str(&sse_event(
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": 0 }),
    ));
    body.push_str(&sse_event(
        "message_delta",
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn", "stop_sequence": null }, "usage": { "output_tokens": 1 } }),
    ));
    body.push_str(&sse_event(
        "message_stop",
        json!({ "type": "message_stop" }),
    ));
    body
}

fn sse_event(name: &str, data: Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&data).expect("serialize provider SSE event")
    )
}

fn spawn_provider(reply: String) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    listener
        .set_nonblocking(true)
        .expect("set fake provider nonblocking");
    let address = listener.local_addr().expect("fake provider address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, request) = loop {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "leg did not call fake provider");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept fake provider request: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("set accepted fake provider stream blocking");
            if let Some(request) = read_request_body(&mut stream) {
                break (stream, request);
            }
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.len(),
            reply
        );
        stream
            .write_all(response.as_bytes())
            .expect("write provider response");
        request
    });
    (format!("http://{address}"), server)
}

fn read_request_body(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut received = Vec::new();
    let header_end = loop {
        if let Some(index) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        received.extend_from_slice(&buffer[..count]);
    };
    let headers = String::from_utf8_lossy(&received[..header_end]);
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    let body_end = header_end.checked_add(content_length)?;
    while received.len() < body_end {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        received.extend_from_slice(&buffer[..count]);
    }
    Some(String::from_utf8_lossy(&received[header_end..body_end]).into_owned())
}

#[test]
fn companion_loads_hooks_from_the_selected_workspace() {
    let scratch = tempfile::tempdir().expect("create scratch directory");
    let workspace = scratch.path().join("selected-workspace");
    let project_config = workspace.join(".leg/hooks.json");
    fs::create_dir_all(project_config.parent().unwrap()).expect("create project hook directory");
    let hook_runner = compile_hook_runner(scratch.path());
    let hook_command = hook_runner.to_string_lossy().into_owned();
    fs::write(
        &project_config,
        json!({
            "version": 1,
            "hooks": [
                { "id": "companion-turn-start", "event": "TurnStart", "command": hook_command.clone() },
                { "id": "companion-turn-end", "event": "TurnEnd", "command": hook_command },
            ]
        })
        .to_string(),
    )
    .expect("write project hooks");

    let hook_log = scratch.path().join("hook-events.jsonl");
    let config_home = scratch.path().join("empty-user-config");
    let app_data = scratch.path().join("empty-appdata");
    let mut environment = EnvGuard::new();
    environment.remove("LEG_HOOKS_CONFIG");
    environment.set("LEG_HOOK_TEST_LOG", &hook_log);
    environment.set("LEG_HOOK_TEST_MODE", "allow");
    environment.set("XDG_CONFIG_HOME", &config_home);
    environment.set("APPDATA", &app_data);
    environment.set("ANTHROPIC_API_KEY", "hooks-workspace-test-key");
    environment.set("ANTHROPIC_BASE_URL", "http://127.0.0.1:1");
    environment.set("LEG_MODEL", "claude-hooks-workspace-test");
    environment.set("LEG_CONTEXT_WINDOW_TOKENS", "200000");
    environment.set("LEG_TIMEOUT_SECS", "10");

    let leg = build_real_leg();
    let supervisor = supervisor_binary();
    assert!(
        supervisor.is_file(),
        "missing supervisor binary: {supervisor:?}"
    );
    let (base_url, provider) = spawn_provider(text_reply("workspace hooks ran"));
    environment.set("ANTHROPIC_BASE_URL", &base_url);
    let session_store = scratch.path().join("sessions");
    let client = Client::new(ClientConfig {
        leg_bin: Some(leg),
        supervisor_bin: Some(supervisor),
        session_store_dir: Some(session_store.clone()),
    });
    let mut turn = client
        .start(TurnRequest::new(
            "run hooks from this workspace",
            &workspace,
            LegSession::New,
        ))
        .expect("start companion turn");
    let mut events = Vec::new();
    while let Some(event) = turn.observe().expect("observe companion event") {
        events.push(event);
    }
    let outcome = turn.wait().expect("wait for companion turn");
    assert!(
        matches!(&outcome, TurnOutcome::Succeeded { .. }),
        "unexpected companion outcome: {outcome:?}"
    );
    let provider_request = provider.join().expect("fake provider thread");
    assert!(provider_request.contains("run hooks from this workspace"));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TextDelta { text, .. } if text == "workspace hooks ran"
    )));

    let hook_records = fs::read_to_string(&hook_log).expect("read hook records");
    let hook_records: Vec<(String, Value)> = hook_records
        .lines()
        .map(|line| {
            let (label, payload) = line.split_once('\t').expect("labeled hook record");
            (
                label.to_string(),
                serde_json::from_str(payload).expect("hook payload JSON"),
            )
        })
        .collect();
    assert_eq!(hook_records.len(), 2);
    assert_eq!(
        hook_records
            .iter()
            .map(|(_, payload)| payload["hook_event_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["TurnStart", "TurnEnd"]
    );
    let selected_workspace = fs::canonicalize(&workspace).expect("canonicalize selected workspace");
    let session_id = hook_records[0].1["session_id"]
        .as_str()
        .expect("hook session ID");
    for (_, payload) in &hook_records {
        let payload_cwd = PathBuf::from(payload["cwd"].as_str().expect("hook cwd"));
        assert_eq!(
            fs::canonicalize(payload_cwd).expect("canonicalize hook cwd"),
            selected_workspace
        );
        assert_eq!(payload["session_id"], session_id);
        assert_eq!(payload["turn_index"], 0);
    }
    let trail = fs::read_to_string(session_store.join(format!("{session_id}.jsonl")))
        .expect("read companion trail");
    assert!(trail.contains("response_ok"));
    assert!(!trail.contains(&hook_command));
}
