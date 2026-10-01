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
    CatalogError, Client, ClientConfig, LegSession, ResolveError, SessionCatalog,
    SessionCatalogConfig, SessionInterface, StartError, StreamEvent, TurnOutcome, TurnRequest,
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

fn write_fixture_leg(path: &Path, exchange_log: Option<&Path>, release_file: Option<&Path>) {
    let source_path = path.with_extension("rs");
    let mut source = String::from(
        r##"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args == ["--version"] {
        println!("leg 0.14.0");
        return;
    }
    if args == ["--help"] {
        println!("leg exchange --stream-json --new-session-id");
        return;
    }
    let session_id = args
        .windows(2)
        .find(|pair| pair[0] == "--new-session-id" || pair[0] == "--session")
        .map(|pair| pair[1].clone())
        .expect("session argument");
    let creates_session = args.windows(2).any(|pair| pair[0] == "--new-session-id");
    let mut prompt = String::new();
io::stdin().read_to_string(&mut prompt).unwrap();
    if let Some(path) = env::var_os("LEG_UI_FIXTURE_INVOCATION_LOG") {
        let mut log = OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(log, "{session_id}").unwrap();
    }
    if creates_session {
        let store = PathBuf::from(env::var_os("LEG_SESSION_DIR").expect("session store"));
        fs::create_dir_all(&store).unwrap();
        let path = store.join(format!("{session_id}.jsonl"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut trail) => writeln!(trail, r#"{{"schema":"baton.exchange/v1","event":"request","ts_ms":1,"model":"fixture","base_url":"fixture","prompt":"fixture","session_id":"{session_id}","turn_index":0}}"#).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create fixture session trail: {error}"),
        }
    }
    if let Some(path) = env::var_os("LEG_UI_FIXTURE_CHILD_PID_FILE") {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        fs::write(path, child.id().to_string()).unwrap();
    }
    if let Some(ready_file) = env::var_os("LEG_UI_FIXTURE_PRE_TURN_START_FILE") {
        fs::write(&ready_file, "ready").unwrap();
        let release_file = PathBuf::from(env::var_os("LEG_UI_FIXTURE_RELEASE_TURN_START").expect("turn_start release"));
        while !release_file.exists() { thread::sleep(Duration::from_millis(5)); }
    }
"##,
    );
    if let Some(exchange_log) = exchange_log {
        source.push_str(&format!(
            "    let mut log = OpenOptions::new().create(true).append(true).open(PathBuf::from({:?})).unwrap();\n    writeln!(log, \"{{session_id}}\").unwrap();\n",
            exchange_log.to_string_lossy()
        ));
    }
    source.push_str(
        r##"
    let turn_start = r#"{"schema":"leg.exchange.stream/v1","event":"turn_start","seq":0,"provider":"fixture","model":"fixture","session_id":"__SESSION_ID__","turn_index":0,"request":{"schema":"baton.message/v1","message_id":"request-1","conversation_id":"conversation-1","kind":"request","body":"fixture"}}"#.replace("__SESSION_ID__", &session_id);
    if let Some(path) = env::var_os("LEG_UI_FIXTURE_TURN_START_FILE") { fs::write(path, "ready").unwrap(); }
    println!("{turn_start}");
    if let Some(path) = env::var_os("LEG_UI_FIXTURE_AFTER_TURN_START_RELEASE") {
        while !Path::new(&path).exists() { thread::sleep(Duration::from_millis(5)); }
    }
"##,
    );
    if let Some(release_file) = release_file {
        source.push_str(&format!(
            "    while ! Path::new({:?}).exists() {{ thread::sleep(Duration::from_millis(5)); }}\n",
            release_file.to_string_lossy()
        ));
    }
    source.push_str(
        r##"
    let turn_end = r#"{"schema":"leg.exchange.stream/v1","event":"turn_end","seq":1,"capped":false,"session_id":"__SESSION_ID__","turn_index":0,"response":{"schema":"baton.message/v1","message_id":"response-1","conversation_id":"conversation-1","in_reply_to":"request-1","kind":"response","body":"ok"}}"#.replace("__SESSION_ID__", &session_id);
    println!("{turn_end}");
}
"##,
    );
    fs::write(&source_path, source).expect("write fixture leg source");
    let compiled = Command::new("rustc")
        .args(["--edition=2021"])
        .arg(&source_path)
        .arg("-o")
        .arg(path)
        .output()
        .expect("rustc is available to build the native fixture");
    assert!(
        compiled.status.success(),
        "failed to compile fixture leg: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
}

fn write_supervisor_wrapper(path: &Path, supervisor: &Path, started_file: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let script = format!(
        "#!/bin/sh\nprintf started > {}\nexec {} \"$@\"\n",
        shell_quote(started_file),
        shell_quote(supervisor)
    );
    fs::write(path, script).expect("write supervisor wrapper");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("make supervisor wrapper executable");
}

struct ReleaseFile(PathBuf);

impl Drop for ReleaseFile {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
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
    assert_eq!(
        retry.warning(),
        "Retry sends this prompt again and may repeat tool side effects."
    );
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
        "fn main() { match std::env::args().nth(1).as_deref() { Some(\"--version\") => println!(\"leg 0.0.1\"), Some(\"--help\") => println!(\"usage: leg exchange --stream-json\"), _ => {} } }",
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
fn preallocated_new_session_locks_before_spawn_without_blocking_other_existing_sessions() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = tempfile::tempdir().expect("scratch directory");
    let cwd = scratch.path().join("cwd");
    let store = scratch.path().join("store");
    fs::create_dir_all(&cwd).expect("create working directory");
    fs::create_dir_all(&store).expect("create session store");
    let leg_source = scratch.path().join("fixture-leg.rs");
    let leg = scratch.path().join("fixture-leg");
    fs::write(
        &leg_source,
        r##"
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::thread;
use std::time::Duration;

const HELD_ID: &str = "sess-100-200-1";

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args == ["--version"] {
        println!("leg 0.14.0");
        return;
    }
    if args == ["--help"] {
        println!("leg exchange --stream-json --new-session-id");
        return;
    }
    let session_id = args
        .windows(2)
        .find(|pair| pair[0] == "--new-session-id" || pair[0] == "--session")
        .map(|pair| pair[1].clone())
        .expect("session argument");
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt).unwrap();
    if session_id == HELD_ID {
        fs::write("preallocated-started", "started").unwrap();
        while !Path::new("release-preallocated").exists() {
            thread::sleep(Duration::from_millis(10));
        }
    }
    println!(
        r#"{{"schema":"leg.exchange.stream/v1","event":"turn_start","seq":0,"provider":"fixture","model":"fixture","session_id":"{}","turn_index":0,"request":{{"schema":"baton.message/v1","message_id":"request-1","conversation_id":"conversation-1","kind":"request","body":"fixture"}}}}"#,
        session_id
    );
    println!(
        r#"{{"schema":"leg.exchange.stream/v1","event":"turn_end","seq":1,"capped":false,"session_id":"{}","turn_index":0,"response":{{"schema":"baton.message/v1","message_id":"response-1","conversation_id":"conversation-1","in_reply_to":"request-1","kind":"response","body":"ok"}}}}"#,
        session_id
    );
}
"##,
    )
    .expect("write native fixture");
    let compiled = Command::new("rustc")
        .args(["--edition=2021"])
        .arg(&leg_source)
        .arg("-o")
        .arg(&leg)
        .output()
        .expect("rustc is available to build the native fixture");
    assert!(
        compiled.status.success(),
        "failed to compile native fixture: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    fs::set_permissions(&leg, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
    let supervisor = supervisor_binary();

    let mut new_turn = client(Some(leg.clone()), &supervisor, &store)
        .start(TurnRequest::new(
            "hold new startup",
            &cwd,
            LegSession::NewWithId("sess-100-200-1".into()),
        ))
        .expect("start new session");
    let started = cwd.join("preallocated-started");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "new leg did not reach its startup gate"
        );
        thread::sleep(Duration::from_millis(5));
    }

    let (same_tx, same_rx) = std::sync::mpsc::sync_channel(1);
    let same_leg = leg.clone();
    let same_supervisor = supervisor.clone();
    let same_store = store.clone();
    let same_cwd = cwd.clone();
    let same_thread = thread::spawn(move || {
        let result = client(Some(same_leg), &same_supervisor, &same_store).start(TurnRequest::new(
            "same session must be busy",
            same_cwd,
            LegSession::Existing("sess-100-200-1".into()),
        ));
        let _ = same_tx.send(result);
    });
    let same_result = same_rx.recv_timeout(Duration::from_secs(2));
    if same_result.is_err() {
        fs::write(cwd.join("release-preallocated"), "release").unwrap();
        let _ = same_thread.join();
        let _ = new_turn.wait();
        panic!("same-ID Existing start waited behind the new-session lock");
    }
    match same_result.unwrap() {
        Err(StartError::Busy) => {}
        Err(error) => panic!("same-ID Existing start returned the wrong error: {error}"),
        Ok(mut turn) => {
            fs::write(cwd.join("release-preallocated"), "release").unwrap();
            let _ = turn.wait();
            let _ = new_turn.wait();
            panic!("same-ID Existing start unexpectedly acquired the session lock");
        }
    }
    same_thread.join().expect("same-ID start thread");

    let (other_tx, other_rx) = std::sync::mpsc::sync_channel(1);
    let other_leg = leg.clone();
    let other_supervisor = supervisor.clone();
    let other_store = store.clone();
    let other_cwd = cwd.clone();
    let other_thread = thread::spawn(move || {
        let result =
            client(Some(other_leg), &other_supervisor, &other_store).start(TurnRequest::new(
                "unrelated existing session",
                other_cwd,
                LegSession::Existing("sess-unrelated-99".into()),
            ));
        let _ = other_tx.send(result);
    });
    let other_result = other_rx.recv_timeout(Duration::from_secs(2));
    if other_result.is_err() {
        fs::write(cwd.join("release-preallocated"), "release").unwrap();
        let _ = other_thread.join();
        let _ = new_turn.wait();
        panic!("unrelated Existing start waited behind the new-session lock");
    }
    let mut other_turn = other_result
        .unwrap()
        .expect("unrelated Existing session proceeds");
    other_thread.join().expect("unrelated start thread");
    assert_success(
        other_turn
            .wait()
            .expect("unrelated existing turn completes"),
    );

    fs::write(cwd.join("release-preallocated"), "release").unwrap();
    assert_success(new_turn.wait().expect("new session completes"));
}

#[test]
fn display_probe_does_not_block_new_or_idle_existing_session() {
    assert_display_probe_allows_start(
        "sess-100-200-108",
        LegSession::NewWithId("sess-100-200-108".into()),
    );
    assert_display_probe_allows_start(
        "sess-100-200-109",
        LegSession::Existing("sess-100-200-109".into()),
    );
}

fn assert_display_probe_allows_start(id: &'static str, session: LegSession) {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let state = scratch.path().join("state");
    let cwd = scratch.path().join("cwd");
    fs::create_dir_all(&cwd).expect("create workspace");
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state.clone()),
        ..SessionCatalogConfig::default()
    })
    .expect("open catalog");
    let store = catalog.sessions_dir().to_path_buf();
    fs::write(store.join(format!("{id}.jsonl")), "").expect("create existing trail");
    catalog
        .set_workspace(id, &cwd)
        .expect("register existing session");
    assert_eq!(
        catalog.get(id).unwrap().run_state,
        leg_ui_client::CatalogRunState::Idle
    );

    let ready = scratch.path().join("probe-ready");
    let release = scratch.path().join("probe-release");
    let _release_on_drop = ReleaseFile(release.clone());
    let mut probe = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "display_probe_race_process_worker"])
        .env("LEG_UI_PROBE_RACE_WORKER", "1")
        .env("LEG_UI_PROBE_RACE_STATE", &state)
        .env("LEG_UI_PROBE_RACE_ID", id)
        .env("LEG_UI_PROBE_RACE_READY", &ready)
        .env("LEG_UI_PROBE_RACE_RELEASE", &release)
        .spawn()
        .expect("spawn display probe process");
    wait_for_file(&ready, Duration::from_secs(5));

    let leg = scratch.path().join("fixture-leg");
    write_fixture_leg(&leg, None, None);
    let wrapper_started = scratch.path().join("supervisor-wrapper-started");
    let wrapper = scratch.path().join("supervisor-wrapper");
    write_supervisor_wrapper(&wrapper, &supervisor_binary(), &wrapper_started);

    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let leg_for_start = leg.clone();
    let wrapper_for_start = wrapper.clone();
    let store_for_start = store.clone();
    let cwd_for_start = cwd.clone();
    let start_thread = thread::spawn(move || {
        let _ = started_tx.send(());
        let result = client(Some(leg_for_start), &wrapper_for_start, &store_for_start).start(
            TurnRequest::new("probe must not defeat this claim", cwd_for_start, session),
        );
        let _ = result_tx.send(result);
    });
    started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("start attempt began");
    wait_for_file(&wrapper_started, Duration::from_secs(5));
    let early_result = result_rx.recv_timeout(Duration::from_millis(300));

    fs::write(&release, "release").expect("release probe process");
    let probe_status = probe.wait().expect("wait for display probe process");
    assert!(
        probe_status.success(),
        "display probe process failed: {probe_status}"
    );
    let early_description = match &early_result {
        Ok(Ok(_)) => {
            "supervisor returned success before the display probe released its lock".into()
        }
        Ok(Err(error)) => {
            format!("supervisor returned {error:?} before the display probe released its lock")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => String::new(),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            "start thread exited before the display probe released its lock".into()
        }
    };
    assert!(
        matches!(
            early_result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "{early_description}"
    );
    let mut turn = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("supervisor proceeds after the display probe")
        .expect("display probe must not make the new turn busy");
    start_thread.join().expect("join start thread");
    assert_success(turn.wait().expect("new turn completes"));
}

#[test]
fn catalog_display_process_loops_over_starts_and_probes() {
    if std::env::var("LEG_UI_CATALOG_DISPLAY_WORKER")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let state = PathBuf::from(
        std::env::var_os("LEG_UI_CATALOG_DISPLAY_STATE").expect("display worker state"),
    );
    let stop = PathBuf::from(
        std::env::var_os("LEG_UI_CATALOG_DISPLAY_STOP").expect("display worker stop file"),
    );
    let progress = PathBuf::from(
        std::env::var_os("LEG_UI_CATALOG_DISPLAY_PROGRESS").expect("display worker progress"),
    );
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state),
        ..SessionCatalogConfig::default()
    })
    .expect("open display worker catalog");
    let mut iterations = 0;
    while !stop.exists() {
        let entries = catalog.list().expect("list sessions during stress");
        for entry in entries {
            match catalog.get(&entry.id) {
                Ok(_) | Err(CatalogError::NotFound(_)) => {}
                Err(error) => panic!("get session during stress: {error}"),
            }
        }
        iterations += 1;
        fs::write(&progress, iterations.to_string()).expect("write display progress");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn thirty_two_new_and_existing_turns_survive_concurrent_catalog_probes() {
    const TURN_COUNT: usize = 32;

    let scratch = tempfile::tempdir().expect("scratch directory");
    let cwd = scratch.path().join("cwd");
    let state = scratch.path().join("state");
    fs::create_dir_all(&cwd).expect("create workspace");
    let leg = scratch.path().join("fixture-leg");
    let exchange_log = scratch.path().join("exchanges.log");
    let release = scratch.path().join("turn-release");
    let _release_turns_on_drop = ReleaseFile(release.clone());
    write_fixture_leg(&leg, Some(&exchange_log), Some(&release));
    let supervisor = supervisor_binary();
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state.clone()),
        leg_bin: Some(leg),
        supervisor_bin: Some(supervisor),
    })
    .expect("open stress catalog");

    let mut drafts = Vec::new();
    for index in 0..TURN_COUNT {
        drafts.push(
            catalog
                .create_draft(
                    SessionInterface::Web,
                    Some(format!("new-{index}")),
                    Some(&cwd),
                )
                .expect("create new-session draft"),
        );
    }
    let mut existing_ids = Vec::new();
    for index in 0..TURN_COUNT {
        let id = format!("sess-stress-{index}");
        fs::write(catalog.sessions_dir().join(format!("{id}.jsonl")), "")
            .expect("create existing-session trail");
        catalog
            .set_workspace(&id, &cwd)
            .expect("register existing session");
        existing_ids.push(id);
    }

    let stop_display = scratch.path().join("display-stop");
    let _stop_display_on_drop = ReleaseFile(stop_display.clone());
    let display_progress = scratch.path().join("display-progress");
    let mut display = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "catalog_display_process_loops_over_starts_and_probes",
        ])
        .env("LEG_UI_CATALOG_DISPLAY_WORKER", "1")
        .env("LEG_UI_CATALOG_DISPLAY_STATE", &state)
        .env("LEG_UI_CATALOG_DISPLAY_STOP", &stop_display)
        .env("LEG_UI_CATALOG_DISPLAY_PROGRESS", &display_progress)
        .spawn()
        .expect("spawn display process");
    wait_for_file(&display_progress, Duration::from_secs(5));
    let initial_iterations = fs::read_to_string(&display_progress)
        .expect("read initial display progress")
        .parse::<usize>()
        .expect("parse initial display progress");

    let mut turns = Vec::with_capacity(TURN_COUNT * 2);
    for draft in drafts {
        let mut turn = catalog
            .start_new(&draft.id, SessionInterface::Web, "new stress turn")
            .expect("start new turn during display probes");
        let event = turn
            .observe()
            .expect("observe new turn")
            .expect("new turn event");
        assert!(matches!(event, StreamEvent::TurnStart { .. }));
        turns.push(turn);
    }
    for id in existing_ids {
        let mut turn = catalog
            .start_existing(&id, SessionInterface::Web, "existing stress turn")
            .expect("start existing turn during display probes");
        let event = turn
            .observe()
            .expect("observe existing turn")
            .expect("existing turn event");
        assert!(matches!(event, StreamEvent::TurnStart { .. }));
        turns.push(turn);
    }
    assert_eq!(turns.len(), TURN_COUNT * 2);
    assert_eq!(
        fs::read_to_string(&exchange_log)
            .expect("read exchange log")
            .lines()
            .count(),
        TURN_COUNT * 2,
        "each deliberate start must run exactly one exchange"
    );
    wait_until(Duration::from_secs(20), || {
        fs::read_to_string(&display_progress)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|iterations| iterations > initial_iterations)
    });

    fs::write(&release, "release").expect("release all turns");
    for mut turn in turns {
        assert_success(turn.wait().expect("stress turn completes"));
    }
    fs::write(&stop_display, "stop").expect("stop display process");
    let display_status = display.wait().expect("wait for display process");
    assert!(
        display_status.success(),
        "display process failed: {display_status}"
    );
}

#[test]
fn display_probe_race_process_worker() {
    if std::env::var("LEG_UI_PROBE_RACE_WORKER").ok().as_deref() != Some("1") {
        return;
    }
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let state =
        PathBuf::from(std::env::var_os("LEG_UI_PROBE_RACE_STATE").expect("probe state directory"));
    let id = std::env::var("LEG_UI_PROBE_RACE_ID").expect("probe session id");
    let ready =
        PathBuf::from(std::env::var_os("LEG_UI_PROBE_RACE_READY").expect("probe ready path"));
    let release =
        PathBuf::from(std::env::var_os("LEG_UI_PROBE_RACE_RELEASE").expect("probe release path"));
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state.clone()),
        ..SessionCatalogConfig::default()
    })
    .expect("open probe worker catalog");
    assert!(catalog.get(&id).is_ok(), "probe worker reads the session");

    // Model the short cross-process interval while the production unit test
    // checks the probe's actual primary-lock and coordination-guard release order.
    let store = state.join("sessions");
    let primary_path = store.join(format!(".leg-ui-session-{id}.lock"));
    let coordination_path = primary_path.with_file_name(format!(".leg-ui-session-{id}.lock.coord"));
    let primary = OpenOptions::new()
        .read(true)
        .write(true)
        .open(primary_path)
        .expect("open primary lock");
    let coordination = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(coordination_path)
        .expect("open coordination guard");
    coordination
        .lock_exclusive()
        .expect("lock probe coordination guard");
    primary.lock_exclusive().expect("hold display probe lock");
    fs::write(ready, "ready").expect("signal held display probe");
    wait_until(Duration::from_secs(10), || release.exists());
    FileExt::unlock(&primary).expect("release primary probe lock");
    FileExt::unlock(&coordination).expect("release probe coordination guard");
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
fn catalog_crash_window_worker() {
    if std::env::var("LEG_UI_CATALOG_CRASH_WORKER").ok().as_deref() != Some("1") {
        return;
    }
    let state = PathBuf::from(std::env::var_os("LEG_UI_CRASH_STATE").expect("state"));
    let leg = PathBuf::from(std::env::var_os("LEG_UI_CRASH_LEG").expect("leg fixture"));
    let supervisor =
        PathBuf::from(std::env::var_os("LEG_UI_CRASH_SUPERVISOR").expect("supervisor binary"));
    let draft_id = std::env::var("LEG_UI_CRASH_DRAFT").expect("draft ID");
    let session_id = std::env::var("LEG_UI_CRASH_SESSION").expect("session ID");
    let ready = PathBuf::from(std::env::var_os("LEG_UI_CRASH_READY").expect("ready file"));
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state),
        leg_bin: Some(leg),
        supervisor_bin: Some(supervisor),
    })
    .expect("open crash worker catalog");
    let mut turn = catalog
        .start_new_with_id(
            &draft_id,
            session_id,
            SessionInterface::Web,
            "prompt must not replay",
        )
        .expect("start catalog turn in child controller");
    fs::write(ready, "ready").expect("signal controller started");
    while let Some(_event) = turn.observe().expect("observe catalog crash turn") {}
    let _ = turn.wait();
}

#[test]
fn stale_attempt_controller_worker() {
    if std::env::var("LEG_UI_STALE_ATTEMPT_WORKER").ok().as_deref() != Some("1") {
        return;
    }
    let state = PathBuf::from(std::env::var_os("LEG_UI_STALE_STATE").expect("state"));
    let leg = PathBuf::from(std::env::var_os("LEG_UI_STALE_LEG").expect("leg fixture"));
    let supervisor =
        PathBuf::from(std::env::var_os("LEG_UI_STALE_SUPERVISOR").expect("supervisor binary"));
    let draft_id = std::env::var("LEG_UI_STALE_DRAFT").expect("draft ID");
    let session_id = std::env::var("LEG_UI_STALE_SESSION").expect("session ID");
    let ready = PathBuf::from(std::env::var_os("LEG_UI_STALE_READY").expect("ready file"));
    let result_path = PathBuf::from(std::env::var_os("LEG_UI_STALE_RESULT").expect("result file"));
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state),
        leg_bin: Some(leg),
        supervisor_bin: Some(supervisor),
    })
    .expect("open stale-attempt worker catalog");
    let mut turn = catalog
        .start_new_with_id(
            &draft_id,
            session_id,
            SessionInterface::Web,
            "prompt for stale-attempt interleave",
        )
        .expect("start catalog turn in stale-attempt worker");
    fs::write(ready, "ready").expect("signal controller started");
    let result = loop {
        match turn.observe() {
            Ok(Some(_)) => {}
            Ok(None) => {
                break match turn.wait() {
                    Ok(outcome) => format!("outcome:{outcome:?}"),
                    Err(error) => format!("wait-error:{error}"),
                };
            }
            Err(error) => break format!("observe-error:{error}"),
        }
    };
    fs::write(result_path, result).expect("record catalog turn result");
}

struct StaleAttemptWorker<'a> {
    state: &'a Path,
    leg: &'a Path,
    supervisor: &'a Path,
    draft_id: &'a str,
    session_id: &'a str,
    ready: &'a Path,
    result: &'a Path,
    invocation_log: &'a Path,
    handoff_pause: Option<(&'a Path, &'a Path)>,
    pre_turn_start: Option<(&'a Path, &'a Path)>,
}

fn spawn_stale_attempt_worker(worker: StaleAttemptWorker<'_>) -> std::process::Child {
    let StaleAttemptWorker {
        state,
        leg,
        supervisor,
        draft_id,
        session_id,
        ready,
        result,
        invocation_log,
        handoff_pause,
        pre_turn_start,
    } = worker;
    let mut command = Command::new(std::env::current_exe().expect("integration test executable"));
    command
        .args(["--exact", "stale_attempt_controller_worker", "--nocapture"])
        .env("LEG_UI_STALE_ATTEMPT_WORKER", "1")
        .env("LEG_UI_STALE_STATE", state)
        .env("LEG_UI_STALE_LEG", leg)
        .env("LEG_UI_STALE_SUPERVISOR", supervisor)
        .env("LEG_UI_STALE_DRAFT", draft_id)
        .env("LEG_UI_STALE_SESSION", session_id)
        .env("LEG_UI_STALE_READY", ready)
        .env("LEG_UI_STALE_RESULT", result)
        .env("LEG_UI_FIXTURE_INVOCATION_LOG", invocation_log)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some((pause_ready, pause_release)) = handoff_pause {
        command
            .env("LEG_UI_TEST_HANDOFF_PAUSE_READY", pause_ready)
            .env("LEG_UI_TEST_HANDOFF_PAUSE_RELEASE", pause_release);
    } else {
        command
            .env_remove("LEG_UI_TEST_HANDOFF_PAUSE_READY")
            .env_remove("LEG_UI_TEST_HANDOFF_PAUSE_RELEASE");
    }
    if let Some((turn_start_ready, turn_start_release)) = pre_turn_start {
        command
            .env("LEG_UI_FIXTURE_PRE_TURN_START_FILE", turn_start_ready)
            .env("LEG_UI_FIXTURE_RELEASE_TURN_START", turn_start_release);
    } else {
        command
            .env_remove("LEG_UI_FIXTURE_PRE_TURN_START_FILE")
            .env_remove("LEG_UI_FIXTURE_RELEASE_TURN_START");
    }
    command
        .spawn()
        .expect("spawn stale-attempt controller worker")
}

fn spawn_catalog_crash_worker(
    state: &Path,
    cwd: &Path,
    leg: &Path,
    supervisor: &Path,
    draft_id: &str,
    session_id: &str,
    ready: &Path,
    pre_turn_start: Option<&Path>,
    release_turn_start: Option<&Path>,
    turn_start_file: Option<&Path>,
    invocation_log: Option<&Path>,
    child_pid_file: Option<&Path>,
    handoff_pause_ready: Option<&Path>,
    handoff_pause_release: Option<&Path>,
    after_turn_start_release: Option<&Path>,
) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().expect("integration test executable"));
    command
        .args(["--exact", "catalog_crash_window_worker", "--nocapture"])
        .env("LEG_UI_CATALOG_CRASH_WORKER", "1")
        .env("LEG_UI_CRASH_STATE", state)
        .env("LEG_UI_CRASH_CWD", cwd)
        .env("LEG_UI_CRASH_LEG", leg)
        .env("LEG_UI_CRASH_SUPERVISOR", supervisor)
        .env("LEG_UI_CRASH_DRAFT", draft_id)
        .env("LEG_UI_CRASH_SESSION", session_id)
        .env("LEG_UI_CRASH_READY", ready)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(path) = pre_turn_start {
        command.env("LEG_UI_FIXTURE_PRE_TURN_START_FILE", path);
    }
    if let Some(path) = release_turn_start {
        command.env("LEG_UI_FIXTURE_RELEASE_TURN_START", path);
    }
    if let Some(path) = turn_start_file {
        command.env("LEG_UI_FIXTURE_TURN_START_FILE", path);
    }
    if let Some(path) = invocation_log {
        command.env("LEG_UI_FIXTURE_INVOCATION_LOG", path);
    }
    if let Some(path) = child_pid_file {
        command.env("LEG_UI_FIXTURE_CHILD_PID_FILE", path);
    }
    if let Some(path) = handoff_pause_ready {
        command.env("LEG_UI_TEST_HANDOFF_PAUSE_READY", path);
    } else {
        command.env_remove("LEG_UI_TEST_HANDOFF_PAUSE_READY");
    }
    if let Some(path) = handoff_pause_release {
        command.env("LEG_UI_TEST_HANDOFF_PAUSE_RELEASE", path);
    } else {
        command.env_remove("LEG_UI_TEST_HANDOFF_PAUSE_RELEASE");
    }
    if let Some(path) = after_turn_start_release {
        command.env("LEG_UI_FIXTURE_AFTER_TURN_START_RELEASE", path);
    } else {
        command.env_remove("LEG_UI_FIXTURE_AFTER_TURN_START_RELEASE");
    }
    command.spawn().expect("spawn catalog controller worker")
}

fn create_crash_draft(
    state: &Path,
    cwd: &Path,
    leg: &Path,
    supervisor: &Path,
    name: &str,
) -> (SessionCatalog, String) {
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state.to_path_buf()),
        leg_bin: Some(leg.to_path_buf()),
        supervisor_bin: Some(supervisor.to_path_buf()),
    })
    .expect("open crash test catalog");
    let draft = catalog
        .create_draft(SessionInterface::Web, Some(name.into()), Some(cwd))
        .expect("create crash test draft");
    catalog
        .save_draft(&draft.id, SessionInterface::Web, "web text survives".into())
        .unwrap();
    catalog
        .save_draft(&draft.id, SessionInterface::Tui, "tui text survives".into())
        .unwrap();
    catalog
        .save_display_metadata(&draft.id, "color".into(), json!("blue"))
        .unwrap();
    (catalog, draft.id)
}

fn wait_for_session_handoff(state: &Path, draft_id: &str, session_id: &str) {
    wait_until(Duration::from_secs(5), || {
        fs::read(state.join("catalog.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|index| {
                index["sessions"][draft_id]["pending_attempt"]["native_session_id"] == session_id
            })
    });
}

fn wait_for_session_lock_release(store: &Path, session_id: &str) {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let path = store.join(format!(".leg-ui-session-{session_id}.lock"));
    wait_until(Duration::from_secs(5), || {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            return !path.exists();
        };
        if file.try_lock_exclusive().is_ok() {
            let _ = FileExt::unlock(&file);
            true
        } else {
            false
        }
    });
}

fn write_blocking_version_leg(path: &Path, pid_file: &Path, release_file: &Path) {
    let source_path = path.with_extension("rs");
    let pid_literal = serde_json::to_string(&pid_file.to_string_lossy().to_string()).unwrap();
    let release_literal =
        serde_json::to_string(&release_file.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

fn main() {{
    match std::env::args().nth(1).as_deref() {{
        Some("--version") => {{
            fs::write({pid_literal}, std::process::id().to_string()).unwrap();
            while !Path::new({release_literal}).exists() {{
                thread::sleep(Duration::from_millis(5));
            }}
            println!("leg 0.14.0");
        }}
        Some("--help") => println!("--stream-json"),
        _ => std::process::exit(1),
    }}
}}
"#
    );
    fs::write(&source_path, source).expect("write blocking version fixture source");
    let output = Command::new("rustc")
        .args(["--edition=2021"])
        .arg(&source_path)
        .arg("-o")
        .arg(path)
        .output()
        .expect("rustc is available to build the blocking version fixture");
    assert!(
        output.status.success(),
        "failed to compile blocking version fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct KillPidOnDrop(u32);

impl Drop for KillPidOnDrop {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGKILL);
        }
    }
}

struct ReleaseFileOnDrop(PathBuf);

impl Drop for ReleaseFileOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
}

#[test]
fn pending_catalog_drafts_recover_at_each_controller_crash_window() {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let scratch = tempfile::tempdir().expect("scratch directory");
    let cwd = scratch.path().join("workspace");
    fs::create_dir_all(&cwd).expect("create workspace");
    let supervisor = supervisor_binary();
    let fixture_leg = scratch.path().join("fixture-leg");
    write_fixture_leg(&fixture_leg, None, None);

    // Window (a): the draft reservation is durable, but the controller dies
    // while resolving the leg executable and before it spawns a supervisor.
    let state_a = scratch.path().join("before-spawn-state");
    let blocking_leg = scratch.path().join("blocking-leg");
    let version_pid = scratch.path().join("version.pid");
    let version_release = scratch.path().join("version-release");
    write_blocking_version_leg(&blocking_leg, &version_pid, &version_release);
    let (_catalog_a, draft_a) =
        create_crash_draft(&state_a, &cwd, &blocking_leg, &supervisor, "before spawn");
    let ready_a = scratch.path().join("worker-a-ready");
    let mut controller_a = spawn_catalog_crash_worker(
        &state_a,
        &cwd,
        &blocking_leg,
        &supervisor,
        &draft_a,
        "sess-109-101",
        &ready_a,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    wait_for_file(&version_pid, Duration::from_secs(5));
    let version_process = read_pid(&version_pid);
    controller_a.kill().expect("kill pre-spawn controller");
    let _ = controller_a.wait();
    unsafe {
        libc::kill(version_process as libc::pid_t, libc::SIGKILL);
    }
    wait_until(Duration::from_secs(3), || {
        !process_is_running(version_process)
    });

    let recovered_a = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state_a.clone()),
        ..SessionCatalogConfig::default()
    })
    .expect("reopen pre-spawn catalog");
    let draft = recovered_a
        .get(&draft_a)
        .expect("dead pre-spawn draft remains");
    assert!(!draft.pending_new_turn);
    assert_eq!(draft.run_state, leg_ui_client::CatalogRunState::Idle);
    assert_eq!(draft.name.as_deref(), Some("before spawn"));
    assert_eq!(draft.drafts[&SessionInterface::Web], "web text survives");
    assert_eq!(draft.drafts[&SessionInterface::Tui], "tui text survives");
    assert_eq!(draft.display["color"], json!("blue"));
    recovered_a
        .save_draft(
            &draft_a,
            SessionInterface::Web,
            "editable after recovery".into(),
        )
        .expect("recovered draft stays editable");
    let submit_catalog = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state_a),
        leg_bin: Some(fixture_leg.clone()),
        supervisor_bin: Some(supervisor.clone()),
    })
    .unwrap();
    let mut submitted = submit_catalog
        .start_new(&draft_a, SessionInterface::Web, "submit after recovery")
        .expect("recovered draft stays submittable");
    while submitted.observe().unwrap().is_some() {}
    assert_success(submitted.wait().unwrap());

    // Windows (b) and (c): the native process has created its trail. The
    // supervisor commits the token-matched session association before the
    // fixture emits turn_start.
    for (window, bind_before_kill) in [("after-spawn", false), ("before-bind", true)] {
        let state = scratch.path().join(format!("{window}-state"));
        let (catalog, draft_id) =
            create_crash_draft(&state, &cwd, &fixture_leg, &supervisor, window);
        let session_id = if bind_before_kill {
            "sess-109-303"
        } else {
            "sess-109-202"
        };
        let native_ready = scratch.path().join(format!("{window}-native-ready"));
        let turn_start_file = scratch.path().join(format!("{window}-turn-start"));
        let turn_start_release = scratch.path().join(format!("{window}-turn-release"));
        let invocation_log = scratch.path().join(format!("{window}-invocations"));
        let child_pid_file = scratch.path().join(format!("{window}-child.pid"));
        let worker_ready = scratch.path().join(format!("{window}-worker-ready"));
        let handoff_pause_ready = scratch.path().join(format!("{window}-handoff-ready"));
        let handoff_pause_release = scratch.path().join(format!("{window}-handoff-release"));
        let after_turn_start_release = scratch.path().join(format!("{window}-turn-end-release"));
        let _handoff_pause =
            (!bind_before_kill).then(|| ReleaseFileOnDrop(handoff_pause_release.clone()));
        let _turn_end_release =
            bind_before_kill.then(|| ReleaseFileOnDrop(after_turn_start_release.clone()));
        let mut controller = spawn_catalog_crash_worker(
            &state,
            &cwd,
            &fixture_leg,
            &supervisor,
            &draft_id,
            session_id,
            &worker_ready,
            Some(&native_ready),
            Some(&turn_start_release),
            Some(&turn_start_file),
            Some(&invocation_log),
            Some(&child_pid_file),
            (!bind_before_kill).then_some(handoff_pause_ready.as_path()),
            (!bind_before_kill).then_some(handoff_pause_release.as_path()),
            bind_before_kill.then_some(after_turn_start_release.as_path()),
        );
        wait_for_file(&native_ready, Duration::from_secs(5));
        if !bind_before_kill {
            // The supervisor has created the native trail and is paused just
            // before committing its token-matched handoff. A fresh catalog
            // reader must complete while startup is held at this barrier.
            wait_for_file(&handoff_pause_ready, Duration::from_secs(5));
            let read_state = state.clone();
            let read_draft = draft_id.clone();
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            let started = Instant::now();
            thread::spawn(move || {
                let result = SessionCatalog::open(SessionCatalogConfig {
                    state_dir: Some(read_state),
                    ..SessionCatalogConfig::default()
                })
                .and_then(|catalog| catalog.get(&read_draft))
                .map(|session| session.run_state)
                .map_err(|error| error.to_string());
                let _ = done_tx.send(result);
            });
            let read_state = done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("independent catalog read must finish within one second")
                .expect("read pending catalog while handoff is paused");
            assert_eq!(read_state, leg_ui_client::CatalogRunState::Active);
            assert!(started.elapsed() < Duration::from_secs(1));
            fs::write(&handoff_pause_release, "release").expect("release handoff barrier");
        }
        wait_for_session_handoff(&state, &draft_id, session_id);
        let child_pid = read_pid(&child_pid_file);
        let _kill_child_on_drop = KillPidOnDrop(child_pid);
        assert_eq!(
            fs::read_to_string(&invocation_log).unwrap().lines().count(),
            1
        );

        if bind_before_kill {
            let catalog_lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(state.join(".catalog.lock"))
                .expect("open catalog lock");
            catalog_lock.lock_exclusive().expect("hold catalog lock");
            fs::write(&turn_start_release, "release").expect("release turn_start");
            wait_for_file(&turn_start_file, Duration::from_secs(5));
            let index: Value = serde_json::from_slice(
                &fs::read(state.join("catalog.json")).expect("read pending catalog under lock"),
            )
            .unwrap();
            assert_eq!(index["sessions"][&draft_id]["pending_new_turn"], true);
            assert!(index["sessions"][session_id].is_null());
            controller
                .kill()
                .expect("kill controller before catalog bind");
            let _ = controller.wait();
            FileExt::unlock(&catalog_lock).expect("release catalog lock");
        } else {
            let active = SessionCatalog::open(SessionCatalogConfig {
                state_dir: Some(state.clone()),
                ..SessionCatalogConfig::default()
            })
            .unwrap();
            let pending = active.get(&draft_id).unwrap();
            assert!(pending.pending_new_turn);
            assert_eq!(pending.run_state, leg_ui_client::CatalogRunState::Active);
            assert!(matches!(
                active.start_new_with_id(
                    &draft_id,
                    "sess-109-222".into(),
                    SessionInterface::Web,
                    "second prompt must not start",
                ),
                Err(CatalogError::Busy)
            ));
            controller
                .kill()
                .expect("kill controller before turn_start");
            let _ = controller.wait();
        }

        let after_controller_death = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state.clone()),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let still_owned = after_controller_death.get(&draft_id).unwrap();
        assert!(still_owned.pending_new_turn);
        assert_eq!(
            still_owned.run_state,
            leg_ui_client::CatalogRunState::Active
        );
        assert!(process_is_running(child_pid));
        assert!(matches!(
            after_controller_death.start_new_with_id(
                &draft_id,
                "sess-109-404".into(),
                SessionInterface::Web,
                "second prompt must not start",
            ),
            Err(CatalogError::Busy)
        ));

        wait_for_session_lock_release(&state.join("sessions"), session_id);
        assert!(
            !process_is_running(child_pid),
            "supervisor must clean its child"
        );
        let index: Value = serde_json::from_slice(
            &fs::read(state.join("catalog.json")).expect("read supervisor owner record"),
        )
        .unwrap();
        let supervisor_pid = index["sessions"][&draft_id]["pending_attempt"]["supervisor"]["pid"]
            .as_u64()
            .expect("recorded supervisor PID") as u32;
        wait_until(Duration::from_secs(5), || {
            !process_is_running(supervisor_pid)
        });
        let recovered = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state.clone()),
            ..SessionCatalogConfig::default()
        })
        .expect("reopen after controller death");
        assert!(matches!(
            recovered.get(&draft_id),
            Err(CatalogError::NotFound(_))
        ));
        let adopted = recovered
            .get(session_id)
            .expect("adopt exact native session");
        assert_eq!(adopted.name.as_deref(), Some(window));
        assert_eq!(
            adopted.cwd.as_deref(),
            Some(fs::canonicalize(&cwd).unwrap().as_path())
        );
        assert_eq!(adopted.drafts[&SessionInterface::Web], "web text survives");
        assert_eq!(adopted.drafts[&SessionInterface::Tui], "tui text survives");
        assert_eq!(adopted.display["color"], json!("blue"));
        assert_eq!(
            fs::read_to_string(&invocation_log).unwrap().lines().count(),
            1,
            "catalog recovery must not launch another exchange"
        );
        drop(catalog);
    }
}

#[test]
fn delayed_stale_supervisor_and_controller_leave_new_pending_attempt_untouched() {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let scratch = tempfile::tempdir().expect("scratch directory");
    let cwd = scratch.path().join("workspace");
    fs::create_dir_all(&cwd).expect("create workspace");
    let state = scratch.path().join("state");
    let supervisor = supervisor_binary();
    let fixture_leg = scratch.path().join("fixture-leg");
    write_fixture_leg(&fixture_leg, None, None);
    let (_catalog, draft_id) =
        create_crash_draft(&state, &cwd, &fixture_leg, &supervisor, "stale interleave");

    let invocation_log = scratch.path().join("invocations");
    let old_session = "sess-109-201";
    let new_session = "sess-109-202";
    let old_ready = scratch.path().join("old-controller-ready");
    let old_result = scratch.path().join("old-controller-result");
    let old_handoff_ready = scratch.path().join("old-handoff-ready");
    let old_handoff_release = scratch.path().join("old-handoff-release");
    let _old_handoff_release = ReleaseFileOnDrop(old_handoff_release.clone());
    let mut old_controller = spawn_stale_attempt_worker(StaleAttemptWorker {
        state: &state,
        leg: &fixture_leg,
        supervisor: &supervisor,
        draft_id: &draft_id,
        session_id: old_session,
        ready: &old_ready,
        result: &old_result,
        invocation_log: &invocation_log,
        handoff_pause: Some((&old_handoff_ready, &old_handoff_release)),
        pre_turn_start: None,
    });
    wait_for_file(&old_ready, Duration::from_secs(5));
    wait_for_file(&old_handoff_ready, Duration::from_secs(5));
    wait_for_file(
        &state.join("sessions").join(format!("{old_session}.jsonl")),
        Duration::from_secs(5),
    );
    wait_until(Duration::from_secs(5), || {
        fs::read_to_string(&invocation_log)
            .ok()
            .is_some_and(|log| log.lines().count() == 1)
    });

    let catalog_path = state.join("catalog.json");
    let old_index: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    let old_attempt = &old_index["sessions"][&draft_id]["pending_attempt"];
    let old_token = old_attempt["token"].as_str().unwrap().to_string();
    let old_supervisor_pid = old_attempt["supervisor"]["pid"].as_u64().unwrap() as u32;
    assert!(old_attempt["native_session_id"].is_null());

    // Model a newer reservation replacing the old lease while its supervisor
    // is delayed before the token-matched handoff. Both attempts below still
    // run through the real catalog client, supervisor, and leg fixture.
    let catalog_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join(".catalog.lock"))
        .expect("open catalog lock");
    catalog_lock.lock_exclusive().expect("hold catalog lock");
    let mut index: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    let record = index["sessions"][&draft_id]
        .as_object_mut()
        .expect("draft record");
    record.insert("pending_new_turn".into(), Value::Bool(false));
    record.insert("pending_attempt".into(), Value::Null);
    fs::write(&catalog_path, serde_json::to_vec_pretty(&index).unwrap())
        .expect("write superseded reservation");
    FileExt::unlock(&catalog_lock).expect("release catalog lock");
    let cleared_index: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    assert!(
        !cleared_index["sessions"][&draft_id]["pending_new_turn"]
            .as_bool()
            .unwrap()
    );
    assert!(cleared_index["sessions"][&draft_id]["pending_attempt"].is_null());

    let new_ready = scratch.path().join("new-controller-ready");
    let new_result = scratch.path().join("new-controller-result");
    let new_turn_start_ready = scratch.path().join("new-turn-start-ready");
    let new_turn_start_release = scratch.path().join("new-turn-start-release");
    let _new_turn_start_release = ReleaseFileOnDrop(new_turn_start_release.clone());
    let mut new_controller = spawn_stale_attempt_worker(StaleAttemptWorker {
        state: &state,
        leg: &fixture_leg,
        supervisor: &supervisor,
        draft_id: &draft_id,
        session_id: new_session,
        ready: &new_ready,
        result: &new_result,
        invocation_log: &invocation_log,
        handoff_pause: None,
        pre_turn_start: Some((&new_turn_start_ready, &new_turn_start_release)),
    });
    wait_until(Duration::from_secs(5), || {
        fs::read(&catalog_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|index| {
                let attempt = &index["sessions"][&draft_id]["pending_attempt"];
                attempt["token"]
                    .as_str()
                    .is_some_and(|token| token != old_token)
                    && attempt["candidate_session_id"] == new_session
            })
    });
    assert_eq!(
        fs::read_to_string(&invocation_log).unwrap().lines().count(),
        1,
        "the newer supervisor waits behind the old creation lock"
    );

    fs::write(&old_handoff_release, "release").expect("release stale handoff");
    wait_for_file(&new_ready, Duration::from_secs(5));
    wait_for_file(&new_turn_start_ready, Duration::from_secs(5));
    wait_for_session_handoff(&state, &draft_id, new_session);
    wait_for_file(&old_result, Duration::from_secs(5));
    let old_status = wait_for_child_status(&mut old_controller, Duration::from_secs(5));
    assert!(
        old_status.success(),
        "stale controller worker: {old_status}"
    );
    let old_outcome = fs::read_to_string(&old_result).unwrap();
    assert!(
        !old_outcome.contains("Succeeded"),
        "superseded attempt unexpectedly succeeded: {old_outcome}"
    );
    wait_until(Duration::from_secs(5), || {
        !process_is_running(old_supervisor_pid)
    });

    let pending_index: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    let new_attempt = &pending_index["sessions"][&draft_id]["pending_attempt"];
    let new_token = new_attempt["token"].as_str().unwrap().to_string();
    assert_ne!(new_token, old_token);
    assert_eq!(new_attempt["candidate_session_id"], new_session);
    assert_eq!(new_attempt["native_session_id"], new_session);
    assert!(
        pending_index["sessions"][&draft_id]["pending_new_turn"]
            .as_bool()
            .unwrap()
    );
    let pending = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state.clone()),
        ..SessionCatalogConfig::default()
    })
    .unwrap()
    .get(&draft_id)
    .expect("stale failure must leave the newer reservation readable");
    assert_eq!(pending.run_state, leg_ui_client::CatalogRunState::Active);
    assert!(pending.pending_new_turn);
    assert_eq!(
        fs::read_to_string(&invocation_log).unwrap().lines().count(),
        2,
        "the stale handoff and failure must not start another exchange"
    );

    fs::write(&new_turn_start_release, "release").expect("release newer turn_start");
    wait_for_file(&new_result, Duration::from_secs(5));
    let new_status = wait_for_child_status(&mut new_controller, Duration::from_secs(5));
    assert!(new_status.success(), "new controller worker: {new_status}");
    assert!(
        fs::read_to_string(&new_result)
            .unwrap()
            .starts_with("outcome:Succeeded"),
        "newer attempt did not finish successfully"
    );

    let recovered = SessionCatalog::open(SessionCatalogConfig {
        state_dir: Some(state.clone()),
        ..SessionCatalogConfig::default()
    })
    .unwrap();
    assert!(matches!(
        recovered.get(&draft_id),
        Err(CatalogError::NotFound(_))
    ));
    let index = fs::read(&catalog_path).unwrap();
    let index: Value = serde_json::from_slice(&index).unwrap();
    assert_eq!(
        index["sessions"][&new_session]["bound_attempt_token"],
        new_token
    );
    assert!(index["sessions"].get(old_session).is_none());
    assert_eq!(
        fs::read_to_string(&invocation_log).unwrap().lines().count(),
        2,
        "only the old and the new intentional exchanges ran"
    );
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
fn wait_for_child_status(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::process::ExitStatus {
    let mut status = None;
    wait_until(timeout, || {
        status = child.try_wait().expect("inspect child status");
        status.is_some()
    });
    status.expect("child exited before deadline")
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
