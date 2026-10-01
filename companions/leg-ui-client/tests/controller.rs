#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use leg_ui_client::{
    Client, ClientConfig, LegSession, ResolveError, SessionCatalog, SessionCatalogConfig,
    SessionInterface, StartError, StreamEvent, TurnOutcome, TurnRequest,
};
use serde_json::{Value, json};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Clone)]
struct ReplyGate(Arc<(Mutex<bool>, Condvar)>);

impl ReplyGate {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    fn wait(&self) {
        let (released, ready) = &*self.0;
        let mut released = released.lock().expect("reply gate lock");
        while !*released {
            released = ready.wait(released).expect("reply gate wait");
        }
    }

    fn release(&self) {
        let (released, ready) = &*self.0;
        *released.lock().expect("reply gate lock") = true;
        ready.notify_all();
    }
}

#[derive(Clone)]
struct MockReply {
    status: u16,
    body: String,
    content_type: &'static str,
    delay: Duration,
    gate: Option<ReplyGate>,
}

impl MockReply {
    fn stream(body: String) -> Self {
        Self {
            status: 200,
            body,
            content_type: "text/event-stream",
            delay: Duration::ZERO,
            gate: None,
        }
    }

    fn error(status: u16, body: String) -> Self {
        Self {
            status,
            body,
            content_type: "application/json",
            delay: Duration::ZERO,
            gate: None,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn gated(mut self, gate: ReplyGate) -> Self {
        self.gate = Some(gate);
        self
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    headers: String,
    body: Value,
}

struct MockProvider {
    base_url: String,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
    stopped: Arc<AtomicBool>,
    gates: Vec<ReplyGate>,
    server: Option<JoinHandle<()>>,
}

impl MockProvider {
    fn start(replies: Vec<MockReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let address = listener.local_addr().expect("provider address");
        let records = Arc::new(Mutex::new(Vec::new()));
        let server_records = Arc::clone(&records);
        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = Arc::clone(&stopped);
        let gates = replies
            .iter()
            .filter_map(|reply| reply.gate.clone())
            .collect();
        let server = thread::spawn(move || {
            for reply in replies {
                let mut connection = loop {
                    if server_stopped.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                connection
                    .set_nonblocking(false)
                    .expect("set mock provider connection blocking");
                let _ = connection.set_read_timeout(Some(Duration::from_secs(10)));
                let Some((headers, body)) = read_request(&mut connection) else {
                    return;
                };
                let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                    return;
                };
                server_records
                    .lock()
                    .expect("record lock")
                    .push(RecordedRequest { headers, body });
                if let Some(gate) = reply.gate {
                    gate.wait();
                } else {
                    thread::sleep(reply.delay);
                }
                let reason = if reply.status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reason,
                    reply.content_type,
                    reply.body.len(),
                    reply.body,
                );
                if connection.write_all(response.as_bytes()).is_err() {
                    return;
                }
                let _ = connection.flush();
            }
        });
        Self {
            base_url: format!("http://{address}"),
            records,
            stopped,
            gates,
            server: Some(server),
        }
    }

    fn records(&self) -> Vec<RecordedRequest> {
        self.records.lock().expect("record lock").clone()
    }

    fn wait_for_records(&self, count: usize, timeout: Duration) -> Vec<RecordedRequest> {
        let deadline = Instant::now() + timeout;
        loop {
            let records = self.records();
            if records.len() >= count {
                return records;
            }
            assert!(
                Instant::now() < deadline,
                "provider received {count} requests"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(mut self) -> Vec<RecordedRequest> {
        self.release_gates();
        self.stopped.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            server.join().expect("mock provider server");
        }
        self.records()
    }

    fn release_gates(&self) {
        for gate in &self.gates {
            gate.release();
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.release_gates();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut received = Vec::new();
    let header_end = loop {
        if let Some(position) = received
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break position;
        }
        let mut chunk = [0_u8; 8192];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        received.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&received[..header_end]).into_owned();
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    let body_end = header_end.checked_add(content_length)?;
    while received.len() < body_end {
        let mut chunk = [0_u8; 8192];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        received.extend_from_slice(&chunk[..count]);
    }
    Some((headers, received[header_end..body_end].to_vec()))
}

fn sse_event(name: &str, data: Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&data).unwrap()
    )
}

fn text_reply(text: &str) -> String {
    let mut body = String::new();
    body.push_str(&sse_event(
        "message_start",
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
    ));
    body.push_str(&sse_event(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    ));
    body.push_str(&sse_event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
    ));
    body.push_str(&sse_event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    body.push_str(&sse_event(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}),
    ));
    body.push_str(&sse_event("message_stop", json!({"type":"message_stop"})));
    body
}

fn bash_tool_reply(id: &str, command: &str, text: &str) -> String {
    let mut body = String::new();
    body.push_str(&sse_event(
        "message_start",
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
    ));
    let mut index = 0;
    if !text.is_empty() {
        body.push_str(&sse_event(
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        ));
        body.push_str(&sse_event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
        ));
        body.push_str(&sse_event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        ));
        index += 1;
    }
    let input =
        serde_json::to_string(&json!({"command":command,"description":"controller fixture"}))
            .unwrap();
    body.push_str(&sse_event(
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":"bash","input":{}}}),
    ));
    body.push_str(&sse_event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input}}),
    ));
    body.push_str(&sse_event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":index}),
    ));
    body.push_str(&sse_event(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":1}}),
    ));
    body.push_str(&sse_event("message_stop", json!({"type":"message_stop"})));
    body
}

struct EnvGuard(Vec<(OsString, Option<OsString>)>);

impl EnvGuard {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn set(&mut self, name: &str, value: impl AsRef<OsStr>) {
        let key = OsString::from(name);
        if !self.0.iter().any(|(existing, _)| existing == &key) {
            self.0.push((key.clone(), std::env::var_os(name)));
        }
        // The harness has one environment-mutating test; its child-only entry
        // point reads inherited values without changing the test process.
        unsafe { std::env::set_var(key, value) };
    }

    fn remove(&mut self, name: &str) {
        let key = OsString::from(name);
        if !self.0.iter().any(|(existing, _)| existing == &key) {
            self.0.push((key.clone(), std::env::var_os(name)));
        }
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..).rev() {
            if let Some(value) = value {
                unsafe { std::env::set_var(name, value) };
            } else {
                unsafe { std::env::remove_var(name) };
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
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest = root_manifest();
    let status = Command::new(&cargo)
        .args(["build", "--locked", "--manifest-path"])
        .arg(&manifest)
        .args(["--bin", "leg"])
        .status()
        .expect("run cargo build for the core leg binary");
    assert!(status.success(), "building the real core leg failed");

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
        .expect("read root cargo metadata");
    assert!(metadata.status.success(), "root cargo metadata failed");
    let metadata: Value = serde_json::from_slice(&metadata.stdout).expect("metadata JSON");
    let target_dir = PathBuf::from(metadata["target_directory"].as_str().unwrap());
    let name = if cfg!(windows) { "leg.exe" } else { "leg" };
    let binary = target_dir.join("debug").join(name);
    assert!(binary.is_file(), "built leg binary missing at {binary:?}");
    binary
}

fn supervisor_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_leg-ui-supervisor") {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .expect("current test executable")
        .parent()
        .and_then(Path::parent)
        .expect("Cargo debug directory")
        .join("leg-ui-supervisor")
}

fn client(leg: Option<PathBuf>, supervisor: &Path, store: &Path) -> Client {
    Client::new(ClientConfig {
        leg_bin: leg,
        supervisor_bin: Some(supervisor.to_path_buf()),
        session_store_dir: Some(store.to_path_buf()),
    })
}

fn collect_events(turn: &mut leg_ui_client::TurnHandle) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = turn.observe().expect("observe validated stream") {
        events.push(event);
    }
    events
}

fn collect_catalog_events(turn: &mut leg_ui_client::CatalogTurn) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = turn.observe().expect("observe catalog turn") {
        events.push(event);
    }
    events
}

fn shared_catalog_controller_contract(
    scratch: &Path,
    leg: &Path,
    supervisor: &Path,
    first_cwd: &Path,
    second_cwd: &Path,
    sentinel: &str,
) {
    let first_cwd = fs::canonicalize(first_cwd).expect("canonicalize first catalog workspace");
    let second_cwd = fs::canonicalize(second_cwd).expect("canonicalize second catalog workspace");
    let state_dir = scratch.join("shared-catalog");
    let inherited_store = scratch.join("inherited-session-store");
    let provider = MockProvider::start(vec![
        MockReply::stream(bash_tool_reply("pwd-one", "pwd", "")),
        MockReply::stream(text_reply("first final answer")),
        MockReply::stream(bash_tool_reply("pwd-two", "pwd", "")),
        MockReply::stream(text_reply("second final answer")),
        MockReply::stream(text_reply("third TUI answer")),
        MockReply::error(
            500,
            json!({"error":{"type":"api_error","message":"temporary fixture failure"}}).to_string(),
        ),
        MockReply::stream(text_reply("retried final answer")),
        MockReply::error(
            500,
            json!({"error":{"type":"api_error","message":format!("first turn fixture failure: {sentinel}")}})
                .to_string(),
        ),
    ]);
    let mut environment = EnvGuard::new();
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    environment.set("LEG_SESSION_DIR", &inherited_store);
    let config = SessionCatalogConfig {
        state_dir: Some(state_dir.clone()),
        leg_bin: Some(leg.to_path_buf()),
        supervisor_bin: Some(supervisor.to_path_buf()),
    };

    let first_controller = SessionCatalog::open(config.clone()).expect("open first catalog");
    let draft = first_controller
        .create_draft(
            SessionInterface::Tui,
            Some("shared session".into()),
            Some(first_cwd.as_path()),
        )
        .expect("create UI draft");
    first_controller
        .save_draft(
            &draft.id,
            SessionInterface::Tui,
            "TUI draft survives".into(),
        )
        .expect("save TUI draft");
    first_controller
        .save_display_metadata(&draft.id, "color".into(), json!("blue"))
        .expect("save display metadata");

    let mut first = first_controller
        .start_new(&draft.id, SessionInterface::Tui, "first catalog prompt")
        .expect("start first catalog turn");
    let first_events = collect_catalog_events(&mut first);
    assert_success(first.wait().expect("finish first catalog turn"));
    let session_id = first
        .session_id()
        .expect("new leg id is persisted at turn_start")
        .to_string();
    assert!(first_events.iter().any(|event| matches!(
        event,
        StreamEvent::TurnStart {
            session_id: Some(id),
            ..
        } if id == &session_id
    )));

    let second_controller = SessionCatalog::open(config.clone()).expect("reopen second catalog");
    let after_first = second_controller
        .get(&session_id)
        .expect("browse shared session");
    assert_eq!(after_first.name.as_deref(), Some("shared session"));
    assert_eq!(after_first.cwd.as_deref(), Some(first_cwd.as_path()));
    assert_eq!(after_first.turns.len(), 1);
    assert_eq!(
        after_first.turns[0].outcome,
        leg_ui_client::TrailOutcome::Succeeded
    );
    let tool_output = after_first.turns[0].tools[0]
        .result
        .as_ref()
        .unwrap()
        .result
        .as_deref()
        .unwrap_or("<missing tool output>");
    assert!(
        tool_output.contains(first_cwd.to_string_lossy().as_ref()),
        "tool output {tool_output:?} does not contain cwd {}",
        first_cwd.display()
    );
    second_controller
        .save_draft(
            &session_id,
            SessionInterface::Web,
            "Web draft stays independent".into(),
        )
        .expect("save Web draft");
    second_controller
        .set_workspace(&session_id, second_cwd.as_path())
        .expect("change workspace while idle");
    let after_workspace_change = second_controller
        .get(&session_id)
        .expect("read changed workspace");
    assert_eq!(
        after_workspace_change.drafts[&SessionInterface::Tui],
        "TUI draft survives"
    );
    assert_eq!(
        after_workspace_change.drafts[&SessionInterface::Web],
        "Web draft stays independent"
    );

    let mut second = second_controller
        .start_existing(&session_id, SessionInterface::Web, "second catalog prompt")
        .expect("continue from another controller");
    let second_events = collect_catalog_events(&mut second);
    assert_success(second.wait().expect("finish second catalog turn"));
    assert!(second_events.iter().any(|event| matches!(
        event,
        StreamEvent::TurnStart {
            session_id: Some(id),
            ..
        } if id == &session_id
    )));

    let mut third = second_controller
        .start_existing(&session_id, SessionInterface::Tui, "third TUI prompt")
        .expect("continue the Web session through the TUI interface");
    let _ = collect_catalog_events(&mut third);
    assert_success(third.wait().expect("finish TUI continuation"));
    let after_tui_continuation = second_controller
        .get(&session_id)
        .expect("read the session continued through TUI");
    assert_eq!(after_tui_continuation.turns.len(), 3);
    assert_eq!(
        after_tui_continuation.cwd.as_deref(),
        Some(second_cwd.as_path())
    );

    let failed_prompt = "retry this failed catalog prompt";
    let mut failed = second_controller
        .start_existing(&session_id, SessionInterface::Web, failed_prompt)
        .expect("start a turn that fails");
    let _ = collect_catalog_events(&mut failed);
    assert!(matches!(
        failed.wait().expect("collect provider failure"),
        TurnOutcome::Failed { .. }
    ));
    let retry = second_controller
        .prepare_retry(&session_id)
        .expect("prepare explicit retry");
    assert_eq!(retry.prompt(), failed_prompt);
    assert!(retry.warning().contains("tools again"));
    let mut retried = second_controller
        .confirm_retry(&retry, SessionInterface::Web)
        .expect("confirm explicit retry");
    let _ = collect_catalog_events(&mut retried);
    assert_success(retried.wait().expect("finish retry"));

    let first_failure_draft = second_controller
        .create_draft(
            SessionInterface::Web,
            Some("named first failure".into()),
            Some(first_cwd.as_path()),
        )
        .expect("create first-failure draft");
    let mut first_failure = second_controller
        .start_new(
            &first_failure_draft.id,
            SessionInterface::Web,
            "first provider request fails",
        )
        .expect("start first turn that fails");
    let _ = collect_catalog_events(&mut first_failure);
    assert!(matches!(
        first_failure.wait().expect("collect first-turn failure"),
        TurnOutcome::Failed { .. }
    ));
    let first_failed_id = first_failure
        .session_id()
        .expect("first-turn failure retains the emitted session id")
        .to_string();
    let reopened_failure = second_controller
        .get(&first_failed_id)
        .expect("failed first turn remains browsable");
    assert_eq!(
        reopened_failure.name.as_deref(),
        Some("named first failure")
    );
    assert_eq!(
        reopened_failure.turns[0].outcome,
        leg_ui_client::TrailOutcome::Failed
    );
    let failed_transcript = second_controller
        .export_transcript(&first_failed_id)
        .expect("export failed trail");
    assert!(!failed_transcript.contains(sentinel));

    let records = provider.finish();
    assert_eq!(records.len(), 8);
    for record in &records {
        let request = record.body.to_string();
        assert!(!request.contains("shared session"));
        assert!(!request.contains("TUI draft survives"));
        assert!(!request.contains("Web draft stays independent"));
        assert!(!request.contains("blue"));
    }
    let second_turn_request = records[2].body.to_string();
    assert!(second_turn_request.contains("first final answer"));
    assert!(second_turn_request.contains(first_cwd.to_string_lossy().as_ref()));
    let changed_cwd_observation = records[3].body.to_string();
    assert!(changed_cwd_observation.contains(second_cwd.to_string_lossy().as_ref()));
    let tui_continuation_request = records[4].body.to_string();
    assert!(tui_continuation_request.contains("second final answer"));
    assert!(tui_continuation_request.contains(second_cwd.to_string_lossy().as_ref()));
    let retry_request = records[6].body.to_string();
    assert_eq!(retry_request.matches(failed_prompt).count(), 1);
    assert!(retry_request.contains("first final answer"));

    let sessions_dir = state_dir.join("sessions");
    assert!(sessions_dir.join(format!("{session_id}.jsonl")).is_file());
    assert!(
        !inherited_store.exists(),
        "catalog-managed sessions override inherited LEG_SESSION_DIR"
    );
    let transcript = second_controller
        .export_transcript(&session_id)
        .expect("export leg trail only");
    assert!(!transcript.contains(sentinel));
    assert!(!transcript.contains("shared session"));
    assert!(!transcript.contains("TUI draft survives"));
    let index = fs::read_to_string(state_dir.join("catalog.json")).expect("read catalog index");
    assert!(!index.contains(sentinel));
    assert_eq!(second_controller.get(&session_id).unwrap().turns.len(), 5);
    assert!(
        sessions_dir
            .join(format!("{first_failed_id}.jsonl"))
            .is_file()
    );
}

fn create_npm_fixture(root: &Path, native_leg: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let launcher = root.join("node_modules/@shukelabs/leg/leg.js");
    fs::create_dir_all(launcher.parent().unwrap()).expect("create npm package");
    fs::copy(
        root_manifest()
            .parent()
            .unwrap()
            .join("packaging/npm/leg.js"),
        &launcher,
    )
    .expect("copy published launcher");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755))
        .expect("make launcher executable");

    let node_arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => panic!("unexpected Node architecture mapping for {other}"),
    };
    let platform = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let platform_binary = root.join(format!(
        "node_modules/@shukelabs/leg-{platform}-{node_arch}/bin/leg"
    ));
    fs::create_dir_all(platform_binary.parent().unwrap()).expect("create platform package");
    fs::copy(native_leg, &platform_binary).expect("copy native leg into npm layout");
    fs::set_permissions(&platform_binary, fs::Permissions::from_mode(0o755))
        .expect("make native leg executable");

    let global_path = root.join("global/bin");
    fs::create_dir_all(&global_path).expect("create global npm bin directory");
    let global_entry = global_path.join("leg");
    std::os::unix::fs::symlink(&launcher, &global_entry).expect("create global npm symlink");
    (launcher, global_entry, platform_binary)
}

fn assert_resolution_failures(
    scratch: &Path,
    native_leg: &Path,
    supervisor: &Path,
    store: &Path,
    cwd: &Path,
) {
    fs::create_dir_all(scratch).expect("create resolution scratch directory");
    let wrapper = scratch.join("wrapper");
    fs::write(&wrapper, "#!/bin/sh\nexec leg \"$@\"\n").expect("write wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod wrapper");
    let error = client(Some(wrapper), supervisor, store)
        .start(TurnRequest::new("prompt", cwd, LegSession::New))
        .err()
        .expect("script wrapper is rejected");
    assert!(matches!(
        error,
        StartError::Resolve(ResolveError::UnsupportedWrapper(_))
    ));

    let missing_leg = scratch.join("missing-leg");
    let error = client(Some(missing_leg), supervisor, store)
        .start(TurnRequest::new("prompt", cwd, LegSession::New))
        .err()
        .expect("missing native binary is recoverable");
    assert!(matches!(
        error,
        StartError::Resolve(ResolveError::Inspect { .. })
    ));

    let fake_source = scratch.join("old-leg.rs");
    let fake_binary = scratch.join("old-leg");
    fs::write(
        &fake_source,
        "fn main() { match std::env::args().nth(1).as_deref() { Some(\"--version\") => println!(\"leg 0.0.1\"), Some(\"--help\") => println!(\"usage: leg ask\"), _ => {} } }",
    )
    .expect("write old native fixture");
    let status = Command::new("rustc")
        .arg("--edition=2021")
        .arg(&fake_source)
        .arg("-o")
        .arg(&fake_binary)
        .status()
        .expect("compile old native fixture");
    assert!(status.success(), "compile old native fixture");
    let error = client(Some(fake_binary), supervisor, store)
        .start(TurnRequest::new("prompt", cwd, LegSession::New))
        .err()
        .expect("old binary is rejected");
    assert!(matches!(
        error,
        StartError::Resolve(ResolveError::MissingStreamCapability { .. })
    ));

    let unavailable_supervisor = scratch.join("missing-supervisor");
    let error = client(
        Some(native_leg.to_path_buf()),
        &unavailable_supervisor,
        store,
    )
    .start(TurnRequest::new("prompt", cwd, LegSession::New))
    .err()
    .expect("missing supervisor is recoverable");
    assert!(matches!(error, StartError::SupervisorUnavailable { .. }));

    let missing_cwd = scratch.join("missing-cwd");
    let error = client(Some(native_leg.to_path_buf()), supervisor, store)
        .start(TurnRequest::new("prompt", missing_cwd, LegSession::New))
        .err()
        .expect("invalid working directory is recoverable");
    assert!(matches!(error, StartError::InvalidCwd(_)));
}

#[track_caller]
fn assert_success(outcome: TurnOutcome) {
    assert!(
        matches!(outcome, TurnOutcome::Succeeded { .. }),
        "expected a successful turn, got {outcome:?}"
    );
}

#[test]
fn controller_child_entrypoint() {
    if std::env::var("LEG_UI_TEST_CONTROLLER_CHILD")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let leg = PathBuf::from(std::env::var_os("LEG_UI_TEST_LEG_BIN").expect("test leg path"));
    let supervisor = PathBuf::from(
        std::env::var_os("LEG_UI_TEST_SUPERVISOR_BIN").expect("test supervisor path"),
    );
    let store = PathBuf::from(std::env::var_os("LEG_UI_TEST_STORE").expect("test store path"));
    let cwd = PathBuf::from(std::env::var_os("LEG_UI_TEST_CWD").expect("test cwd"));
    let pid_dir = PathBuf::from(std::env::var_os("LEG_UI_TEST_PID_DIR").expect("pid directory"));
    let mut turn = client(Some(leg), &supervisor, &store)
        .start(TurnRequest::new(
            "run the active process fixture",
            cwd,
            LegSession::New,
        ))
        .expect("start child controller turn");
    while let Some(event) = turn.observe().expect("observe child turn") {
        if let StreamEvent::TurnStart {
            session_id: Some(id),
            ..
        } = event
        {
            fs::write(pid_dir.join("session.id"), id).expect("record session id");
        }
    }
    let _ = turn.wait();
}

#[test]
fn companion_controller_contract_and_cleanup() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let leg = build_real_leg();
    let supervisor = supervisor_binary();
    assert!(
        supervisor.is_file(),
        "missing supervisor binary: {supervisor:?}"
    );
    let cwd = scratch.path().join("cwd");
    fs::create_dir_all(&cwd).expect("create working directory");

    let sentinel = "controller-test-secret-9381";
    let mut environment = EnvGuard::new();
    environment.set("ANTHROPIC_API_KEY", sentinel);
    environment.set("LEG_MODEL", "claude-controller-fixture");
    environment.set("LEG_TIMEOUT_SECS", "15");
    environment.set("LEG_BASH_TIMEOUT_SECS", "60");
    environment.set("LEG_MAX_RETRIES", "0");
    environment.remove("LEG_MAX_TOOL_ROUNDS");

    let (launcher, global_entry, npm_binary) =
        create_npm_fixture(&scratch.path().join("npm"), &leg);
    let node = Command::new("node").arg("--version").output();
    assert!(node.is_ok(), "Node is needed for the published npm fixture");

    assert_resolution_failures(
        &scratch.path().join("resolution"),
        &leg,
        &supervisor,
        &scratch.path().join("resolution-store"),
        &cwd,
    );

    // The direct native --leg-bin path preserves a multiline Chinese prompt,
    // streams live text, and correlates the final envelope.
    let provider = MockProvider::start(vec![MockReply::stream(text_reply("收到，正在处理。"))]);
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    let prompt = "第一行：检查会话\n第二行：保留换行与中文。";
    let mut turn = client(
        Some(leg.clone()),
        &supervisor,
        &scratch.path().join("direct-store"),
    )
    .start(TurnRequest::new(prompt, &cwd, LegSession::New))
    .expect("start with a direct native binary");
    let events = collect_events(&mut turn);
    assert_success(turn.wait().expect("wait for native turn"));
    let request = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::TurnStart { request, .. } => Some(request),
            _ => None,
        })
        .expect("turn start event");
    assert_eq!(request["body"], prompt);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TextDelta { text, .. } if text == "收到，正在处理。"
    )));
    let terminal = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::TurnEnd { response, .. } => Some(response),
            _ => None,
        })
        .expect("turn end event");
    assert_eq!(terminal["in_reply_to"], request["message_id"]);
    assert_eq!(terminal["conversation_id"], request["conversation_id"]);
    let provider_records = provider.finish();
    assert_eq!(provider_records.len(), 1);
    assert!(
        provider_records[0]
            .headers
            .to_ascii_lowercase()
            .contains(sentinel)
    );
    assert!(
        provider_records[0]
            .body
            .to_string()
            .contains("第一行：检查会话\\n第二行：保留换行与中文。")
    );

    let catalog_workspaces = tempfile::Builder::new()
        .prefix("catalog-cwd-")
        .tempdir_in("/tmp")
        .expect("catalog workspace root");
    let first_catalog_cwd = catalog_workspaces.path().join("workspace-one");
    let second_cwd = catalog_workspaces.path().join("workspace-two");
    fs::create_dir_all(&first_catalog_cwd).expect("create first catalog workspace");
    fs::create_dir_all(&second_cwd).expect("create second catalog workspace");
    shared_catalog_controller_contract(
        scratch.path(),
        &leg,
        &supervisor,
        &first_catalog_cwd,
        &second_cwd,
        sentinel,
    );

    // Default PATH resolution follows a global npm symlink to the published
    // launcher, then the native package binary. Explicitly selecting that same
    // symlink exercises --leg-bin launcher resolution as well.
    let old_path = std::env::var_os("PATH").expect("test PATH");
    let path = std::env::join_paths(
        std::iter::once(global_entry.parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&old_path)),
    )
    .expect("compose fixture PATH");
    environment.set("PATH", &path);
    for (tag, leg_override) in [
        ("npm-path", None),
        ("npm-explicit", Some(global_entry.clone())),
    ] {
        let provider = MockProvider::start(vec![MockReply::stream(text_reply("npm native ok"))]);
        environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
        let mut turn = client(
            leg_override,
            &supervisor,
            &scratch.path().join(format!("{tag}-store")),
        )
        .start(TurnRequest::new("npm fixture", &cwd, LegSession::New))
        .expect("resolve published npm launcher to native binary");
        let events = collect_events(&mut turn);
        assert_success(turn.wait().expect("wait npm turn"));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TextDelta { text, .. } if text == "npm native ok"
        )));
        assert_eq!(provider.finish().len(), 1);
    }

    let missing_platform_root = scratch.path().join("missing-platform");
    let missing_platform_launcher =
        missing_platform_root.join("node_modules/@shukelabs/leg/leg.js");
    fs::create_dir_all(missing_platform_launcher.parent().unwrap()).unwrap();
    fs::copy(
        root_manifest()
            .parent()
            .unwrap()
            .join("packaging/npm/leg.js"),
        &missing_platform_launcher,
    )
    .unwrap();
    fs::set_permissions(
        &missing_platform_launcher,
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let error = client(
        Some(missing_platform_launcher),
        &supervisor,
        &scratch.path().join("missing-platform-store"),
    )
    .start(TurnRequest::new("prompt", &cwd, LegSession::New))
    .err()
    .expect("missing npm package is recoverable");
    assert!(matches!(
        error,
        StartError::Resolve(ResolveError::NpmResolution(_))
    ));

    // The cap case emits live text and a completed tool result, then returns
    // the cap bit with the authoritative terminal envelope. The second model
    // tool request is not run after the configured one-round limit.
    environment.set("LEG_MAX_TOOL_ROUNDS", "1");
    let output_marker = scratch.path().join("should-not-run");
    let provider = MockProvider::start(vec![
        MockReply::stream(bash_tool_reply(
            "toolu-controller-1",
            "printf '工具已完成'",
            "先运行工具。",
        )),
        MockReply::stream(bash_tool_reply(
            "toolu-controller-2",
            &format!("touch {}", shell_quote(&output_marker)),
            "",
        )),
    ]);
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    let prompt = "请执行工具\n并停止在轮数上限。";
    let mut turn = client(
        Some(leg.clone()),
        &supervisor,
        &scratch.path().join("cap-store"),
    )
    .start(TurnRequest::new(prompt, &cwd, LegSession::New))
    .expect("start tool turn");
    let events = collect_events(&mut turn);
    let outcome = turn.wait().expect("wait capped turn");
    assert!(matches!(
        outcome,
        TurnOutcome::Succeeded { capped: true, .. }
    ));
    assert!(!output_marker.exists(), "capped tool call must not execute");
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TextDelta { text, .. } if text == "先运行工具。"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolResult { status, output, .. }
            if status == "completed" && output.to_string().contains("工具已完成")
    )));
    let request = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::TurnStart { request, .. } => Some(request),
            _ => None,
        })
        .unwrap();
    assert_eq!(request["body"], prompt);
    let terminal = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::TurnEnd {
                response, capped, ..
            } if *capped => Some(response),
            _ => None,
        })
        .expect("capped terminal event");
    assert_eq!(terminal["in_reply_to"], request["message_id"]);
    let cap_records = provider.finish();
    assert_eq!(cap_records.len(), 2);
    assert!(
        cap_records[0]
            .body
            .to_string()
            .contains("请执行工具\\n并停止在轮数上限。")
    );

    // Stop interrupts the owned native process, waits for the bash group to
    // disappear, and produces a resumable interrupted trail.
    environment.remove("LEG_MAX_TOOL_ROUNDS");
    let stop_pid_dir = scratch.path().join("stopped-processes");
    fs::create_dir_all(&stop_pid_dir).expect("create Stop pid directory");
    let stop_provider = MockProvider::start(vec![
        MockReply::stream(bash_tool_reply(
            "toolu-stop-cleanup",
            &format!(
                "echo $$ > {}; sleep 60 & echo $! > {}; wait",
                shell_quote(&stop_pid_dir.join("bash.pid")),
                shell_quote(&stop_pid_dir.join("sleep.pid")),
            ),
            "starting stoppable process",
        )),
        MockReply::stream(text_reply("stopped session resumed")),
    ]);
    environment.set("ANTHROPIC_BASE_URL", &stop_provider.base_url);
    let stop_store = scratch.path().join("stop-store");
    let mut stopped = client(Some(global_entry.clone()), &supervisor, &stop_store)
        .start(TurnRequest::new("start a long tool", &cwd, LegSession::New))
        .expect("start stoppable turn");
    let mut stop_session_id = None;
    loop {
        let event = stopped
            .observe()
            .expect("observe stoppable turn")
            .expect("long tool remains active");
        match event {
            StreamEvent::TurnStart {
                session_id: Some(id),
                ..
            } => stop_session_id = Some(id),
            StreamEvent::ToolCall { .. } => break,
            _ => {}
        }
    }
    let stop_session_id = stop_session_id.expect("turn_start includes session id");
    let bash_pid = read_pid(&stop_pid_dir.join("bash.pid"));
    let sleep_pid = read_pid(&stop_pid_dir.join("sleep.pid"));
    stop_provider.wait_for_records(1, Duration::from_secs(3));
    assert_native_leg_ancestor(&npm_binary, bash_pid);
    stopped.stop().expect("send Stop to supervisor");
    let _ = collect_events(&mut stopped);
    assert!(matches!(
        stopped.wait().expect("collect stopped outcome"),
        TurnOutcome::Stopped { .. }
    ));
    wait_until(Duration::from_secs(3), || {
        !process_is_running(bash_pid) && !process_is_running(sleep_pid)
    });
    let stop_trail = fs::read_to_string(stop_store.join(format!("{stop_session_id}.jsonl")))
        .expect("read Stop trail");
    assert!(
        stop_trail.contains("\"kind\":\"interrupted\""),
        "Stop trail lacks interrupted outcome: {stop_trail}"
    );
    let mut resumed = client(Some(global_entry.clone()), &supervisor, &stop_store)
        .start(TurnRequest::new(
            "resume after Stop",
            &cwd,
            LegSession::Existing(stop_session_id),
        ))
        .expect("Stop releases the session lock after cleanup");
    let _ = collect_events(&mut resumed);
    assert_success(resumed.wait().expect("resumed stopped session"));
    assert_eq!(stop_provider.finish().len(), 2);

    // An error body that repeats an inherited credential is redacted from the
    // structured outcome and the event metadata returned to the UI.
    environment.remove("LEG_MAX_TOOL_ROUNDS");
    let provider = MockProvider::start(vec![MockReply::error(
        401,
        json!({"error":{"type":"authentication_error","message":format!("invalid key {sentinel}")}}).to_string(),
    )]);
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    let mut turn = client(
        Some(leg.clone()),
        &supervisor,
        &scratch.path().join("error-store"),
    )
    .start(TurnRequest::new("failing request", &cwd, LegSession::New))
    .expect("provider failure is a running turn");
    let events = collect_events(&mut turn);
    let outcome = turn.wait().expect("provider failure is a result");
    let diagnostic = match outcome {
        TurnOutcome::Failed {
            message, response, ..
        } => {
            assert!(!response.unwrap().to_string().contains(sentinel));
            message
        }
        other => panic!("expected provider failure, got {other:?}"),
    };
    assert!(!diagnostic.contains(sentinel));
    assert!(!format!("{events:?}").contains(sentinel));
    assert_eq!(provider.finish().len(), 1);

    // The supervisor lock blocks another process before it makes a provider
    // request, while a first new-session request is still in flight.
    let reply_gate = ReplyGate::new();
    let provider = MockProvider::start(vec![
        MockReply::stream(text_reply("serialized")).gated(reply_gate.clone()),
    ]);
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    let store = scratch.path().join("busy-store");
    let mut first = client(Some(leg.clone()), &supervisor, &store)
        .start(TurnRequest::new("first", &cwd, LegSession::New))
        .expect("start first controller");
    let first_start = first.observe().expect("first start record").expect("event");
    let session_id = match first_start {
        StreamEvent::TurnStart {
            session_id: Some(id),
            ..
        } => id,
        other => panic!("expected session turn_start, got {other:?}"),
    };
    provider.wait_for_records(1, Duration::from_secs(3));
    let competing = client(Some(leg.clone()), &supervisor, &store).start(TurnRequest::new(
        "competing",
        &cwd,
        LegSession::Existing(session_id),
    ));
    let request_count_while_held = provider.records().len();
    reply_gate.release();
    let error = competing.err().expect("competing start is busy");
    assert!(matches!(error, StartError::Busy));
    assert_eq!(
        request_count_while_held, 1,
        "busy start must not call provider"
    );
    let _ = collect_events(&mut first);
    assert_success(first.wait().expect("first controller completes"));
    provider.finish();

    // Distinct new sessions may run at the same time and receive different
    // kernel-guarded session IDs.
    let provider = MockProvider::start(vec![
        MockReply::stream(text_reply("parallel one")).delayed(Duration::from_millis(200)),
        MockReply::stream(text_reply("parallel two")).delayed(Duration::from_millis(200)),
    ]);
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    let store = scratch.path().join("parallel-store");
    let mut one = client(Some(leg.clone()), &supervisor, &store)
        .start(TurnRequest::new("one", &cwd, LegSession::New))
        .expect("start independent session one");
    let one_start = one.observe().expect("one start").expect("one event");
    let one_id = match one_start {
        StreamEvent::TurnStart {
            session_id: Some(id),
            ..
        } => id,
        other => panic!("expected first turn_start, got {other:?}"),
    };
    let mut two = client(Some(leg.clone()), &supervisor, &store)
        .start(TurnRequest::new("two", &cwd, LegSession::New))
        .expect("start independent session two");
    let two_start = two.observe().expect("two start").expect("two event");
    let two_id = match two_start {
        StreamEvent::TurnStart {
            session_id: Some(id),
            ..
        } => id,
        other => panic!("expected second turn_start, got {other:?}"),
    };
    assert_ne!(one_id, two_id);
    let _ = collect_events(&mut one);
    let _ = collect_events(&mut two);
    assert_success(one.wait().expect("first independent turn"));
    assert_success(two.wait().expect("second independent turn"));
    assert_eq!(provider.finish().len(), 2);

    // Abrupt controller death while a bash tool and its sleep child are live
    // must leave an interrupted trail, clean both PIDs, and release the lock
    // for a later turn in the same session.
    let pid_dir = scratch.path().join("owned-processes");
    fs::create_dir_all(&pid_dir).expect("create pid directory");
    let provider = MockProvider::start(vec![
        MockReply::stream(bash_tool_reply(
            "toolu-crash-cleanup",
            &format!(
                "echo $$ > {}; sleep 60 & echo $! > {}; wait",
                shell_quote(&pid_dir.join("bash.pid")),
                shell_quote(&pid_dir.join("sleep.pid")),
            ),
            "starting process fixture",
        )),
        MockReply::stream(text_reply("resumed after cleanup")),
    ]);
    environment.set("ANTHROPIC_BASE_URL", &provider.base_url);
    let child_store = scratch.path().join("crash-store");
    let test_binary = std::env::current_exe().expect("integration test executable");
    let mut controller = Command::new(test_binary)
        .args(["--exact", "controller_child_entrypoint", "--nocapture"])
        .env("LEG_UI_TEST_CONTROLLER_CHILD", "1")
        .env("LEG_UI_TEST_LEG_BIN", &global_entry)
        .env("LEG_UI_TEST_SUPERVISOR_BIN", &supervisor)
        .env("LEG_UI_TEST_STORE", &child_store)
        .env("LEG_UI_TEST_CWD", &cwd)
        .env("LEG_UI_TEST_PID_DIR", &pid_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn controller process");
    let session_file = pid_dir.join("session.id");
    wait_for_file(&session_file, Duration::from_secs(5));
    let session_id = fs::read_to_string(&session_file).expect("read fixture session id");
    let bash_pid = read_pid(&pid_dir.join("bash.pid"));
    let sleep_pid = read_pid(&pid_dir.join("sleep.pid"));
    provider.wait_for_records(1, Duration::from_secs(3));
    assert_native_leg_ancestor(&npm_binary, bash_pid);
    controller.kill().expect("kill controller process");
    let _ = controller.wait();
    wait_until(Duration::from_secs(3), || {
        !process_is_running(bash_pid) && !process_is_running(sleep_pid)
    });

    let resume_client = || {
        client(Some(global_entry.clone()), &supervisor, &child_store).start(TurnRequest::new(
            "continue after cleanup",
            &cwd,
            LegSession::Existing(session_id.clone()),
        ))
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut resumed = loop {
        match resume_client() {
            Ok(turn) => break turn,
            Err(StartError::Busy) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("session did not recover after controller death: {error}"),
        }
    };

    let trail_path = child_store.join(format!("{session_id}.jsonl"));
    wait_for_file(&trail_path, Duration::from_secs(2));
    let trail = fs::read_to_string(&trail_path).expect("read interrupted session trail");
    let trail_events: Vec<Value> = trail
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert!(
        trail_events
            .iter()
            .any(|event| { event["event"] == "response_error" && event["kind"] == "interrupted" }),
        "trail should record the interrupted turn: {trail}"
    );
    assert!(
        !trail_events
            .iter()
            .any(|event| event["event"] == "session_end")
    );

    let _ = collect_events(&mut resumed);
    assert_success(resumed.wait().expect("resumed session completes"));
    assert_eq!(provider.finish().len(), 2);

    // Root cargo metadata remains a single-package graph; companion-only
    // crates stay outside the core package.
    let metadata = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(root_manifest())
        .output()
        .expect("read root cargo metadata");
    assert!(metadata.status.success());
    let metadata: Value = serde_json::from_slice(&metadata.stdout).unwrap();
    let workspace_members = metadata["workspace_members"].as_array().unwrap();
    assert_eq!(workspace_members.len(), 1);
    let core_package = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["id"] == workspace_members[0])
        .expect("root package metadata");
    assert_eq!(core_package["name"], "leg");
    assert!(
        !metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|package| package["name"] == "leg-ui-client"),
        "companion crate entered the core Cargo graph"
    );

    // The global entry really is a symlink to the copied published launcher.
    assert_eq!(
        fs::canonicalize(&global_entry).unwrap(),
        fs::canonicalize(&launcher).unwrap()
    );
}

#[track_caller]
fn wait_for_file(path: &Path, timeout: Duration) {
    wait_until(timeout, || path.is_file());
}

#[track_caller]
fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "condition did not become true in {timeout:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[track_caller]
fn read_pid(path: &Path) -> u32 {
    wait_until(Duration::from_secs(3), || {
        fs::read_to_string(path)
            .ok()
            .is_some_and(|value| value.trim().parse::<u32>().is_ok())
    });
    fs::read_to_string(path)
        .expect("read process id")
        .trim()
        .parse()
        .expect("parse process id")
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(close) = stat.rfind(')') else {
            return false;
        };
        stat[close + 1..]
            .split_whitespace()
            .next()
            .is_some_and(|state| state != "Z")
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p"])
            .arg(pid.to_string())
            .output();
        output.ok().is_some_and(|output| {
            let state = String::from_utf8_lossy(&output.stdout);
            output.status.success() && !state.trim().is_empty() && !state.contains('Z')
        })
    }
}

fn assert_native_leg_ancestor(native_leg: &Path, descendant_pid: u32) {
    let expected = fs::canonicalize(native_leg).expect("canonicalize npm platform binary");
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut pid = Pid::from_u32(descendant_pid);
    for _ in 0..64 {
        let Some(process) = system.process(pid) else {
            break;
        };
        if let Some(executable) = process.exe() {
            let actual = fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
            if actual == expected {
                return;
            }
            assert_ne!(
                executable.file_name().and_then(OsStr::to_str),
                Some("node"),
                "Node must not own the active leg process"
            );
        }
        let Some(parent) = process.parent() else {
            break;
        };
        pid = parent;
    }
    panic!(
        "no process for npm platform binary {} owns descendant PID {descendant_pid}",
        expected.display()
    );
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

use std::os::unix::fs::PermissionsExt;
