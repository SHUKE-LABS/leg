use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::protocol::{StreamDecoder, StreamEvent, StreamFailure};
use crate::resolve::{ResolveError, resolve_leg};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONTROL_LINE: usize = 64 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct ClientConfig {
    /// Native `leg` path or the published `@shukelabs/leg` launcher.
    pub leg_bin: Option<PathBuf>,
    /// Companion supervisor path. By default it is beside the host executable.
    pub supervisor_bin: Option<PathBuf>,
    /// Session store override. If absent, `LEG_SESSION_DIR`, XDG, then HOME apply.
    pub session_store_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum LegSession {
    New,
    Existing(String),
}

#[derive(Clone, Debug)]
pub struct TurnRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub session: LegSession,
}

impl TurnRequest {
    pub fn new(prompt: impl Into<String>, cwd: impl Into<PathBuf>, session: LegSession) -> Self {
        Self {
            prompt: prompt.into(),
            cwd: cwd.into(),
            session,
        }
    }
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error("could not determine the session store: {0}")]
    SessionStore(String),
    #[error("working directory {0:?} is not an existing directory")]
    InvalidCwd(PathBuf),
    #[error("invalid session id; use only ASCII letters, digits, '-' and '_'")]
    InvalidSessionId,
    #[error(
        "the companion supervisor is unavailable at {path:?}: {message}; install leg-ui-supervisor beside the companion or set LEG_UI_SUPERVISOR_BIN"
    )]
    SupervisorUnavailable { path: PathBuf, message: String },
    #[error("session is busy: another companion turn owns it")]
    Busy,
    #[error("could not start the companion supervisor: {0}")]
    Supervisor(String),
}

#[derive(Clone, Debug, Error)]
pub enum ClientError {
    #[error("the companion supervisor stopped before reporting a result")]
    SupervisorStopped,
    #[error("the leg stream is incomplete: {0}")]
    Incomplete(String),
    #[error("invalid leg stream: {0}")]
    Protocol(#[from] StreamFailure),
    #[error("could not send Stop to the companion supervisor: {0}")]
    Stop(String),
}

#[derive(Clone, Debug)]
pub enum TurnOutcome {
    Succeeded {
        response: Value,
        capped: bool,
    },
    Failed {
        response: Option<Value>,
        message: String,
    },
    Stopped {
        response: Option<Value>,
    },
    Incomplete {
        message: String,
        forced: bool,
    },
}

#[derive(Clone, Debug, Serialize)]
struct StartRequestWire<'a> {
    kind: &'static str,
    leg_path: String,
    cwd: String,
    store_dir: String,
    prompt: &'a str,
    session: SessionWire<'a>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionWire<'a> {
    New,
    Existing { session_id: &'a str },
}

enum WorkerMessage {
    Event(StreamEvent),
    Protocol(StreamFailure),
    Finished(TurnOutcome),
}

pub struct Client {
    config: ClientConfig,
}

impl Client {
    pub fn new(config: ClientConfig) -> Self {
        Self { config }
    }

    pub fn start(&self, request: TurnRequest) -> Result<TurnHandle, StartError> {
        let resolved = resolve_leg(self.config.leg_bin.as_deref())?;
        let store_dir = resolve_store_dir(self.config.session_store_dir.as_deref())?;
        let cwd = fs::canonicalize(&request.cwd)
            .map_err(|_| StartError::InvalidCwd(request.cwd.clone()))?;
        if !cwd.is_dir() {
            return Err(StartError::InvalidCwd(cwd));
        }
        let session = match &request.session {
            LegSession::New => SessionWire::New,
            LegSession::Existing(id) => {
                if !safe_session_id(id) {
                    return Err(StartError::InvalidSessionId);
                }
                SessionWire::Existing { session_id: id }
            }
        };
        let supervisor = resolve_supervisor(self.config.supervisor_bin.as_deref())?;
        let mut command = Command::new(&supervisor);
        command
            .arg("--internal-supervisor")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            StartError::Supervisor(format!(
                "failed to execute {}: {error}",
                supervisor.display()
            ))
        })?;
        let child_stdin = child.stdin.take().expect("supervisor stdin is piped");
        let child_stdout = child.stdout.take().expect("supervisor stdout is piped");
        let child_stderr = child.stderr.take().expect("supervisor stderr is piped");
        let stderr_thread = spawn_stderr_reader(child_stderr);

        let wire = StartRequestWire {
            kind: "start",
            leg_path: resolved.path.to_string_lossy().into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            store_dir: store_dir.to_string_lossy().into_owned(),
            prompt: &request.prompt,
            session,
        };
        let mut child_stdin = child_stdin;
        serde_json::to_writer(&mut child_stdin, &wire)
            .map_err(|error| StartError::Supervisor(error.to_string()))?;
        child_stdin
            .write_all(b"\n")
            .and_then(|()| child_stdin.flush())
            .map_err(|error| StartError::Supervisor(error.to_string()))?;

        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("leg-ui-supervisor-handshake".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(child_stdout);
                let mut line = Vec::new();
                let result = read_bounded_line(&mut reader, &mut line, MAX_CONTROL_LINE)
                    .map(|has_line| (has_line, line, reader));
                let _ = ack_tx.send(result);
            })
            .map_err(|error| StartError::Supervisor(error.to_string()))?;

        let (has_line, ack_line, reader) = match ack_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                disconnect_supervisor_for_cleanup(child_stdin, child, stderr_thread);
                return Err(StartError::Supervisor(format!(
                    "could not read supervisor startup: {error}; the supervisor was disconnected so it can clean up any owned child"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                disconnect_supervisor_for_cleanup(child_stdin, child, stderr_thread);
                return Err(StartError::Supervisor(
                    "supervisor startup timed out; its control pipe was closed so it can clean up any owned child"
                        .to_string(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                disconnect_supervisor_for_cleanup(child_stdin, child, stderr_thread);
                return Err(StartError::Supervisor(
                    "supervisor startup reader stopped; its control pipe was closed so it can clean up any owned child"
                        .to_string(),
                ));
            }
        };
        if !has_line {
            let status = child.wait().ok();
            let stderr = join_stderr(stderr_thread);
            return Err(StartError::Supervisor(format!(
                "supervisor exited before startup ({status:?}); {}",
                bounded_text(&stderr)
            )));
        }
        let ack: Value = serde_json::from_slice(&ack_line)
            .map_err(|_| StartError::Supervisor("invalid startup response".to_string()))?;
        match ack.get("_supervisor").and_then(Value::as_str) {
            Some("ready") => {}
            Some("busy") => {
                drop(child_stdin);
                let _ = child.wait();
                let _ = stderr_thread.join();
                return Err(StartError::Busy);
            }
            Some("error") => {
                let message = ack
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("supervisor setup failed");
                drop(child_stdin);
                let _ = child.wait();
                let stderr = join_stderr(stderr_thread);
                return Err(StartError::Supervisor(format!(
                    "{message}; {}",
                    bounded_text(&stderr)
                )));
            }
            _ => {
                disconnect_supervisor_for_cleanup(child_stdin, child, stderr_thread);
                return Err(StartError::Supervisor(
                    "invalid supervisor startup response; the supervisor was disconnected so it can clean up any owned child"
                        .to_string(),
                ));
            }
        }

        let control = Arc::new(Mutex::new(Some(child_stdin)));
        let weak_control = Arc::downgrade(&control);
        let (message_tx, message_rx) = mpsc::channel();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(true));
        let worker_stop_requested = Arc::clone(&stop_requested);
        thread::Builder::new()
            .name("leg-ui-turn-reader".to_string())
            .spawn(move || {
                watch_supervisor(
                    child,
                    reader,
                    stderr_thread,
                    message_tx,
                    weak_control,
                    worker_stop_requested,
                );
            })
            .map_err(|error| StartError::Supervisor(error.to_string()))?;

        Ok(TurnHandle {
            control,
            messages: message_rx,
            pending: VecDeque::new(),
            finished: None,
            protocol_error: None,
            stop_requested,
            active,
        })
    }
}

pub struct TurnHandle {
    control: Arc<Mutex<Option<ChildStdin>>>,
    messages: mpsc::Receiver<WorkerMessage>,
    pending: VecDeque<StreamEvent>,
    finished: Option<TurnOutcome>,
    protocol_error: Option<StreamFailure>,
    stop_requested: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct TurnStopHandle {
    control: Arc<Mutex<Option<ChildStdin>>>,
    stop_requested: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
}

impl TurnStopHandle {
    /// Requests interruption without needing access to the stream reader.
    pub fn stop(&self) -> Result<(), ClientError> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.stop_requested.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        send_control(&self.control, "stop").map_err(ClientError::Stop)
    }
}

impl TurnHandle {
    pub fn stop_handle(&self) -> TurnStopHandle {
        TurnStopHandle {
            control: Arc::clone(&self.control),
            stop_requested: Arc::clone(&self.stop_requested),
            active: Arc::clone(&self.active),
        }
    }

    /// Waits for the next validated stream event. `None` means the turn ended.
    pub fn observe(&mut self) -> Result<Option<StreamEvent>, ClientError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if let Some(error) = self.protocol_error.clone() {
            return Err(ClientError::Protocol(error));
        }
        if self.finished.is_some() {
            return Ok(None);
        }
        match self.messages.recv() {
            Ok(WorkerMessage::Event(event)) => Ok(Some(event)),
            Ok(WorkerMessage::Protocol(error)) => {
                self.protocol_error = Some(error.clone());
                Err(ClientError::Protocol(error))
            }
            Ok(WorkerMessage::Finished(outcome)) => {
                self.active.store(false, Ordering::Release);
                self.finished = Some(outcome);
                Ok(None)
            }
            Err(_) => {
                self.active.store(false, Ordering::Release);
                Err(ClientError::SupervisorStopped)
            }
        }
    }

    /// Requests interruption. Call `wait` to collect the final distinct outcome.
    pub fn stop(&mut self) -> Result<(), ClientError> {
        self.stop_handle().stop()
    }

    /// Waits for process cleanup and returns the authoritative terminal outcome.
    /// Stream events not previously observed remain available through `observe`.
    pub fn wait(&mut self) -> Result<TurnOutcome, ClientError> {
        if let Some(outcome) = &self.finished {
            return Ok(outcome.clone());
        }
        loop {
            match self.messages.recv() {
                Ok(WorkerMessage::Event(event)) => self.pending.push_back(event),
                Ok(WorkerMessage::Protocol(error)) => self.protocol_error = Some(error),
                Ok(WorkerMessage::Finished(outcome)) => {
                    self.active.store(false, Ordering::Release);
                    self.finished = Some(outcome.clone());
                    return Ok(outcome);
                }
                Err(_) => {
                    self.active.store(false, Ordering::Release);
                    return Err(ClientError::Incomplete(
                        "supervisor message channel closed before a result".to_string(),
                    ));
                }
            }
        }
    }
}

impl Drop for TurnHandle {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Ok(mut slot) = self.control.lock() {
            slot.take();
        }
    }
}

fn resolve_supervisor(override_path: Option<&Path>) -> Result<PathBuf, StartError> {
    let candidate = if let Some(path) = override_path {
        path.to_path_buf()
    } else if let Some(path) = std::env::var_os("LEG_UI_SUPERVISOR_BIN") {
        PathBuf::from(path)
    } else {
        let executable =
            std::env::current_exe().map_err(|error| StartError::SupervisorUnavailable {
                path: PathBuf::new(),
                message: error.to_string(),
            })?;
        let parent = executable.parent().unwrap_or_else(|| Path::new("."));
        parent.join(supervisor_name())
    };
    let canonical =
        fs::canonicalize(&candidate).map_err(|error| StartError::SupervisorUnavailable {
            path: candidate.clone(),
            message: error.to_string(),
        })?;
    if !canonical.is_file() {
        return Err(StartError::SupervisorUnavailable {
            path: canonical,
            message: "not a regular file".to_string(),
        });
    }
    Ok(canonical)
}

#[cfg(windows)]
fn supervisor_name() -> &'static OsStr {
    OsStr::new("leg-ui-supervisor.exe")
}

#[cfg(not(windows))]
fn supervisor_name() -> &'static OsStr {
    OsStr::new("leg-ui-supervisor")
}

fn resolve_store_dir(override_path: Option<&Path>) -> Result<PathBuf, StartError> {
    let path = if let Some(path) = override_path {
        path.to_path_buf()
    } else if let Some(path) = std::env::var_os("LEG_SESSION_DIR") {
        if path.is_empty() {
            return Err(StartError::SessionStore(
                "LEG_SESSION_DIR must not be blank".to_string(),
            ));
        }
        PathBuf::from(path)
    } else if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(path).join("leg").join("sessions")
    } else {
        let home = std::env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|path| !path.is_empty()))
            .ok_or_else(|| {
                StartError::SessionStore(
                    "could not determine HOME for the default session store".to_string(),
                )
            })?;
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("leg")
            .join("sessions")
    };
    fs::create_dir_all(&path).map_err(|error| {
        StartError::SessionStore(format!("could not create {}: {error}", path.display()))
    })?;
    fs::canonicalize(&path).map_err(|error| {
        StartError::SessionStore(format!(
            "could not canonicalize {}: {error}",
            path.display()
        ))
    })
}

fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    output: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    output.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!output.is_empty());
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if output.len() + length > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "supervisor control record is too large",
            ));
        }
        let has_newline = available[length - 1] == b'\n';
        output.extend_from_slice(&available[..length]);
        reader.consume(length);
        if has_newline {
            return Ok(true);
        }
    }
}

fn spawn_stderr_reader(stderr: ChildStderr) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || drain_capped(stderr, MAX_DIAGNOSTIC_BYTES))
}

fn drain_capped(mut reader: impl Read, cap: usize) -> Vec<u8> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let keep = cap.saturating_sub(output.len()).min(count);
                output.extend_from_slice(&chunk[..keep]);
            }
        }
    }
    output
}

fn join_stderr(reader: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    reader.join().unwrap_or_default()
}

fn disconnect_supervisor_for_cleanup(
    child_stdin: ChildStdin,
    mut child: Child,
    stderr_reader: thread::JoinHandle<Vec<u8>>,
) {
    // Closing the control pipe lets the guardian stop its owned process tree.
    // Reap it in the background so dropping a handle stays nonblocking and
    // long-lived hosts do not retain a zombie supervisor.
    drop(child_stdin);
    thread::spawn(move || {
        let _ = child.wait();
        let _ = stderr_reader.join();
    });
}

fn bounded_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    let mut chars = text.chars();
    let result: String = chars.by_ref().take(600).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

fn send_control(control: &Arc<Mutex<Option<ChildStdin>>>, kind: &str) -> Result<(), String> {
    let mut slot = control
        .lock()
        .map_err(|_| "control stream lock is poisoned".to_string())?;
    let writer = slot
        .as_mut()
        .ok_or_else(|| "supervisor control stream is closed".to_string())?;
    serde_json::to_writer(&mut *writer, &serde_json::json!({"kind": kind}))
        .map_err(|error| error.to_string())?;
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(|error| error.to_string())
}

fn watch_supervisor(
    mut child: Child,
    mut reader: BufReader<ChildStdout>,
    stderr_thread: thread::JoinHandle<Vec<u8>>,
    message_tx: mpsc::Sender<WorkerMessage>,
    control: Weak<Mutex<Option<ChildStdin>>>,
    stop_requested: Arc<AtomicBool>,
) {
    let mut decoder = StreamDecoder::default();
    let mut terminal: Option<Value> = None;
    let mut terminal_capped = false;
    let mut forced = false;
    let mut protocol_error: Option<StreamFailure> = None;
    let mut line = Vec::new();
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_RECORD_BYTES) {
            Ok(false) => break,
            Err(_) => {
                protocol_error.get_or_insert(StreamFailure::InvalidJson);
                request_internal_stop(&control);
                break;
            }
            Ok(true) => {
                let record: Value = match serde_json::from_slice(&line) {
                    Ok(value) => value,
                    Err(_) => {
                        protocol_error.get_or_insert(StreamFailure::InvalidJson);
                        let _ =
                            message_tx.send(WorkerMessage::Protocol(StreamFailure::InvalidJson));
                        request_internal_stop(&control);
                        continue;
                    }
                };
                if let Some(helper_message) = record.get("_supervisor").and_then(Value::as_str) {
                    if helper_message == "finished" {
                        forced = record
                            .get("forced")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                    } else {
                        protocol_error
                            .get_or_insert(StreamFailure::InvalidField("supervisor record"));
                    }
                    continue;
                }
                match decoder.accept(&line) {
                    Ok(event) => {
                        let mut event = event;
                        redact_stream_event(&mut event);
                        if let StreamEvent::TurnEnd {
                            response, capped, ..
                        } = &event
                        {
                            terminal = Some(response.clone());
                            terminal_capped = *capped;
                        }
                        if message_tx.send(WorkerMessage::Event(event)).is_err() {
                            request_internal_stop(&control);
                            break;
                        }
                    }
                    Err(error) => {
                        if protocol_error.is_none() {
                            let _ = message_tx.send(WorkerMessage::Protocol(error.clone()));
                            protocol_error = Some(error);
                        }
                        request_internal_stop(&control);
                    }
                }
            }
        }
    }
    let status = child.wait();
    let stderr = join_stderr(stderr_thread);
    let exit_code = status.ok().and_then(|status| status.code());
    let outcome = classify_outcome(
        terminal,
        protocol_error,
        exit_code,
        forced,
        terminal_capped,
        stop_requested.load(Ordering::Acquire),
        &bounded_text(&stderr),
    );
    let _ = message_tx.send(WorkerMessage::Finished(outcome));
}

fn request_internal_stop(control: &Weak<Mutex<Option<ChildStdin>>>) {
    if let Some(control) = control.upgrade() {
        let _ = send_control(&control, "stop");
    }
}

fn classify_outcome(
    terminal: Option<Value>,
    protocol_error: Option<StreamFailure>,
    exit_code: Option<i32>,
    forced: bool,
    capped: bool,
    stop_requested: bool,
    stderr: &str,
) -> TurnOutcome {
    if forced {
        return TurnOutcome::Incomplete {
            message: "owned process tree required forced termination".to_string(),
            forced: true,
        };
    }
    if let Some(error) = protocol_error {
        return TurnOutcome::Incomplete {
            message: error.to_string(),
            forced: false,
        };
    }
    let Some(response) = terminal else {
        return match (exit_code, stop_requested) {
            (Some(130 | 143), true) => TurnOutcome::Stopped { response: None },
            (code, _) => TurnOutcome::Incomplete {
                message: if stderr.is_empty() {
                    format!("leg exited with status {code:?} without a terminal turn_end record")
                } else {
                    format!("leg exited without a terminal turn_end record: {stderr}")
                },
                forced: false,
            },
        };
    };
    let kind = response.get("kind").and_then(Value::as_str).unwrap_or("");
    let outcome_kind = response
        .get("exchange")
        .and_then(|value| value.get("exchange"))
        .and_then(|value| value.get("outcome"))
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match (kind, exit_code) {
        ("response", Some(0)) => TurnOutcome::Succeeded { response, capped },
        ("error", Some(code)) if code != 0 && outcome_kind == "interrupted" => {
            TurnOutcome::Stopped {
                response: Some(response),
            }
        }
        ("error", Some(code)) if code != 0 => TurnOutcome::Failed {
            response: Some(response.clone()),
            message: response_message(&response)
                .map(redact_credentials)
                .unwrap_or_else(|| {
                    if stderr.is_empty() {
                        format!("leg failed with exit code {code}")
                    } else {
                        stderr.to_string()
                    }
                }),
        },
        ("response", Some(code)) if code != 0 => TurnOutcome::Incomplete {
            message: format!("successful response disagrees with leg exit code {code}"),
            forced: false,
        },
        ("error", Some(0)) => TurnOutcome::Incomplete {
            message: "error response disagrees with successful leg exit status".to_string(),
            forced: false,
        },
        (_, _) if stop_requested => TurnOutcome::Stopped {
            response: Some(response),
        },
        _ => TurnOutcome::Incomplete {
            message: "leg exited without a consistent terminal outcome".to_string(),
            forced: false,
        },
    }
}

fn response_message(response: &Value) -> Option<String> {
    response
        .get("exchange")?
        .get("exchange")?
        .get("outcome")?
        .get("message")?
        .as_str()
        .map(str::to_string)
        .or_else(|| response.get("body")?.as_str().map(str::to_string))
}

pub(crate) fn redact_credentials(message: String) -> String {
    let mut secrets: Vec<String> = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            let secret_name = name.contains("API_KEY")
                || name.contains("AUTH_TOKEN")
                || name.contains("OAUTH_TOKEN")
                || name.contains("SECRET")
                || name.contains("PASSWORD")
                || name.contains("CREDENTIAL");
            let value = value.to_string_lossy().into_owned();
            (secret_name && !value.is_empty()).then_some(value)
        })
        .collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets
        .iter()
        .fold(message, |text, secret| text.replace(secret, "[REDACTED]"))
}

fn redact_stream_event(event: &mut StreamEvent) {
    match event {
        StreamEvent::TurnStart {
            request,
            provider,
            model,
            session_id,
            ..
        } => {
            redact_value(request);
            *provider = redact_credentials(std::mem::take(provider));
            *model = redact_credentials(std::mem::take(model));
            if let Some(session_id) = session_id {
                *session_id = redact_credentials(std::mem::take(session_id));
            }
        }
        StreamEvent::TextDelta { text, .. } => {
            *text = redact_credentials(std::mem::take(text));
        }
        StreamEvent::ToolRound { content, .. } => redact_value(content),
        StreamEvent::ToolCall {
            tool_use_id,
            tool_name,
            input,
            ..
        } => {
            *tool_use_id = redact_credentials(std::mem::take(tool_use_id));
            *tool_name = redact_credentials(std::mem::take(tool_name));
            redact_value(input);
        }
        StreamEvent::ToolResult {
            tool_use_id,
            tool_name,
            status,
            output,
            ..
        } => {
            *tool_use_id = redact_credentials(std::mem::take(tool_use_id));
            *tool_name = redact_credentials(std::mem::take(tool_name));
            *status = redact_credentials(std::mem::take(status));
            redact_value(output);
        }
        StreamEvent::TurnEnd {
            response,
            session_id,
            ..
        } => {
            redact_value(response);
            if let Some(session_id) = session_id {
                *session_id = redact_credentials(std::mem::take(session_id));
            }
        }
        StreamEvent::Unknown { event, record, .. } => {
            *event = redact_credentials(std::mem::take(event));
            redact_value(record);
        }
    }
}

pub(crate) fn redact_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_credentials(std::mem::take(text)),
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        Value::Object(object) => object.values_mut().for_each(redact_value),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(kind: &str) -> Value {
        json!({
            "kind": kind,
            "exchange": {"exchange": {"outcome": {"kind": "provider_error"}}}
        })
    }

    #[test]
    fn missing_terminal_is_incomplete_even_after_zero_exit() {
        assert!(matches!(
            classify_outcome(None, None, Some(0), false, false, false, ""),
            TurnOutcome::Incomplete { forced: false, .. }
        ));
    }

    #[test]
    fn exit_and_terminal_disagreement_is_incomplete() {
        assert!(matches!(
            classify_outcome(
                Some(response("response")),
                None,
                Some(1),
                false,
                false,
                false,
                ""
            ),
            TurnOutcome::Incomplete { forced: false, .. }
        ));
        assert!(matches!(
            classify_outcome(
                Some(response("error")),
                None,
                Some(0),
                false,
                false,
                false,
                ""
            ),
            TurnOutcome::Incomplete { forced: false, .. }
        ));
    }

    #[test]
    fn consistent_terminal_outcomes_preserve_failure_and_cap_status() {
        assert!(matches!(
            classify_outcome(
                Some(response("error")),
                None,
                Some(1),
                false,
                false,
                false,
                ""
            ),
            TurnOutcome::Failed { .. }
        ));
        assert!(matches!(
            classify_outcome(
                Some(response("response")),
                None,
                Some(0),
                false,
                true,
                false,
                ""
            ),
            TurnOutcome::Succeeded { capped: true, .. }
        ));
    }
}
