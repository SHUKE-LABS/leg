//! Private process guardian used by `Client`. The helper owns the exclusive
//! session lock and the native leg process, so controller death cannot release
//! ownership while a tool is still running.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sysinfo::{ProcessStatus, ProcessesToUpdate, System};

use crate::process_owner::{OwnerState, ProcessOwner};

const MAX_START_REQUEST: usize = 16 * 1024 * 1024;
const MAX_STREAM_RECORD: usize = 16 * 1024 * 1024;
const MAX_CHILD_STDERR: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STOP_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
struct StartRequest {
    kind: String,
    leg_path: PathBuf,
    cwd: PathBuf,
    store_dir: PathBuf,
    prompt: String,
    session: SessionMode,
    #[serde(default)]
    attempt: Option<StartAttempt>,
}

#[derive(Clone, Debug, Deserialize)]
struct StartAttempt {
    draft_id: String,
    token: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionMode {
    New { session_id: String },
    Existing { session_id: String },
}

enum Message {
    Output(Result<Vec<u8>, String>),
    OutputClosed,
    Control(Control),
}

enum Control {
    Stop,
    Disconnected,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ProcessIdentity {
    owner: ProcessOwner,
    pgid: i32,
    executable: Option<PathBuf>,
}

pub fn run_from_stdin() -> Result<i32, String> {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin);
    let mut line = Vec::new();
    if !read_bounded_line(&mut input, &mut line, MAX_START_REQUEST)
        .map_err(|error| error.to_string())?
    {
        return Err("controller closed before sending a start request".to_string());
    }
    let request: StartRequest = serde_json::from_slice(&line)
        .map_err(|_| "invalid controller start request".to_string())?;
    if request.kind != "start" {
        return Err("unsupported supervisor request".to_string());
    }
    supervise(request, input)
}

fn supervise(request: StartRequest, input: BufReader<io::Stdin>) -> Result<i32, String> {
    fs::create_dir_all(&request.store_dir).map_err(|error| {
        format!(
            "could not create session store {}: {error}",
            request.store_dir.display()
        )
    })?;
    let store_dir = fs::canonicalize(&request.store_dir).map_err(|error| {
        format!(
            "could not canonicalize session store {}: {error}",
            request.store_dir.display()
        )
    })?;
    let attempt_owner = match (&request.attempt, &request.session) {
        (Some(attempt), SessionMode::New { session_id }) => {
            if !safe_session_id(session_id) || !native_session_id(session_id) {
                return Err("invalid preallocated session id".to_string());
            }
            let owner = ProcessOwner::current()?;
            crate::catalog::publish_supervisor_owner(
                &store_dir,
                &attempt.draft_id,
                &attempt.token,
                session_id,
                owner.clone(),
            )?;
            Some(owner)
        }
        (Some(_), _) => return Err("catalog attempt metadata requires a new session".to_string()),
        (None, _) => None,
    };
    let (mut creation_lock, mut session_lock): (Option<File>, Option<File>) = match &request.session
    {
        SessionMode::New { session_id } => {
            if !safe_session_id(session_id) || !native_session_id(session_id) {
                return Err("invalid preallocated session id".to_string());
            }
            let creation_lock = acquire_creation_lock(&store_dir)?;
            let session_lock = match try_session_lock(&store_dir, session_id)? {
                Some(lock) => lock,
                None => {
                    write_control(&json!({"_supervisor":"busy"}))
                        .map_err(|error| error.to_string())?;
                    return Ok(0);
                }
            };
            (Some(creation_lock), Some(session_lock))
        }
        SessionMode::Existing { session_id } => {
            if !safe_session_id(session_id) {
                return Err("invalid session id".to_string());
            }
            match try_session_lock(&store_dir, session_id)? {
                Some(lock) => (None, Some(lock)),
                None => {
                    write_control(&json!({"_supervisor":"busy"}))
                        .map_err(|error| error.to_string())?;
                    return Ok(0);
                }
            }
        }
    };

    let mut command = Command::new(&request.leg_path);
    command.args(["exchange", "--stream-json"]);
    match &request.session {
        SessionMode::New { session_id } => {
            command.args(["--new-session-id", session_id]);
        }
        SessionMode::Existing { session_id } => {
            command.args(["--session", session_id]);
        }
    }
    command
        .current_dir(&request.cwd)
        .env("LEG_SESSION_DIR", &store_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            write_control(&json!({
                "_supervisor":"error",
                "message":format!("could not start native leg executable: {error}")
            }))
            .map_err(|write_error| write_error.to_string())?;
            return Err(format!("could not start native leg executable: {error}"));
        }
    };
    let leg_pid = child.id();
    let Some(root_identity) = capture_process_identity(leg_pid) else {
        let _ = child.kill();
        let _ = child.wait();
        write_control(&json!({
            "_supervisor":"error",
            "message":"could not verify the native leg process identity"
        }))
        .map_err(|error| error.to_string())?;
        return Ok(1);
    };
    let child_stdin = child.stdin.take().expect("leg stdin is piped");
    let child_stdout = child.stdout.take().expect("leg stdout is piped");
    let child_stderr = child.stderr.take().expect("leg stderr is piped");

    let (message_tx, message_rx) = mpsc::channel();
    spawn_output_reader(child_stdout, message_tx.clone());
    let stderr_thread = thread::spawn(move || drain_capped(child_stderr, MAX_CHILD_STDERR));
    spawn_control_reader(input, message_tx);

    let mut child_stdin = child_stdin;
    let prompt_write = child_stdin
        .write_all(request.prompt.as_bytes())
        .and_then(|()| child_stdin.flush());
    drop(child_stdin);
    if let Err(error) = prompt_write {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("could not send the prompt to leg stdin: {error}"));
    }
    write_control(&json!({"_supervisor":"ready"})).map_err(|error| error.to_string())?;

    let mut stdout_closed = false;
    let mut status = None;
    let mut stop_started: Option<Instant> = None;
    let mut stop_deadline: Option<Instant> = None;
    let mut forced = false;
    let mut observed = HashSet::new();
    let mut active_tool_ids = HashSet::new();
    let mut session_id_seen = matches!(&request.session, SessionMode::Existing { .. });
    let mut handoff_recorded = false;

    loop {
        if !handoff_recorded
            && let (Some(attempt), Some(owner), SessionMode::New { session_id }) = (
                request.attempt.as_ref(),
                attempt_owner.as_ref(),
                &request.session,
            )
            && managed_session_trail_exists(&store_dir, session_id)
        {
            match crate::catalog::publish_native_session_handoff(
                &store_dir,
                &attempt.draft_id,
                &attempt.token,
                session_id,
                session_id,
                owner,
            ) {
                Ok(()) => {
                    handoff_recorded = true;
                    creation_lock.take();
                }
                Err(error) => {
                    eprintln!("could not persist native session handoff: {error}");
                    begin_stop(
                        leg_pid,
                        &root_identity,
                        status.is_none(),
                        &mut observed,
                        &mut stop_started,
                        &mut stop_deadline,
                    );
                }
            }
        }
        if status.is_none() && (stop_started.is_some() || !active_tool_ids.is_empty()) {
            observed.extend(snapshot_owned_processes(&root_identity));
            observed.insert(root_identity.clone());
        }
        if let Some(deadline) = stop_deadline
            && Instant::now() >= deadline
            && !forced
            && (status.is_none() || any_observed_alive(&observed))
        {
            observed.extend(snapshot_owned_processes(&root_identity));
            observed.insert(root_identity.clone());
            force_owned_tree(&observed, &mut child, status.is_none());
            forced = true;
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(child_status)) => status = Some(child_status),
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("could not inspect native leg process: {error}"));
                }
            }
        }

        if status.is_some() && stdout_closed {
            if stop_started.is_none() {
                if any_observed_alive(&observed) {
                    // The native leg exited while an observed tool process
                    // survived. Keep the lock through a bounded cleanup grace.
                    begin_stop(
                        leg_pid,
                        &root_identity,
                        false,
                        &mut observed,
                        &mut stop_started,
                        &mut stop_deadline,
                    );
                } else {
                    break;
                }
            } else if !any_observed_alive(&observed) {
                break;
            }
            if stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) && forced {
                while any_observed_alive(&observed) {
                    force_owned_tree(&observed, &mut child, false);
                    thread::sleep(POLL_INTERVAL);
                }
                break;
            }
        }

        match message_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Message::Output(Ok(line))) => {
                update_active_tools(&line, &mut active_tool_ids);
                if !session_id_seen && is_turn_start_for_new_session(&line) {
                    let Some(id) = extract_session_id(&line) else {
                        eprintln!("leg stream did not expose the new session id at turn_start");
                        begin_stop(
                            leg_pid,
                            &root_identity,
                            status.is_none(),
                            &mut observed,
                            &mut stop_started,
                            &mut stop_deadline,
                        );
                        continue;
                    };
                    let expected_id = match &request.session {
                        SessionMode::New { session_id } => session_id.as_str(),
                        SessionMode::Existing { .. } => unreachable!("existing IDs start seen"),
                    };
                    if !safe_session_id(&id) || id != expected_id {
                        eprintln!("leg stream returned an unexpected new session id");
                        begin_stop(
                            leg_pid,
                            &root_identity,
                            status.is_none(),
                            &mut observed,
                            &mut stop_started,
                            &mut stop_deadline,
                        );
                        continue;
                    }
                    if session_lock.is_none() {
                        eprintln!("new session lock was not reserved before native startup");
                        begin_stop(
                            leg_pid,
                            &root_identity,
                            status.is_none(),
                            &mut observed,
                            &mut stop_started,
                            &mut stop_deadline,
                        );
                        continue;
                    }
                    if !handoff_recorded && let Some(attempt) = request.attempt.as_ref() {
                        let SessionMode::New {
                            session_id: candidate_session_id,
                        } = &request.session
                        else {
                            unreachable!("catalog attempts only start new sessions")
                        };
                        if let Err(error) = crate::catalog::publish_native_session_handoff(
                            &store_dir,
                            &attempt.draft_id,
                            &attempt.token,
                            candidate_session_id,
                            &id,
                            attempt_owner
                                .as_ref()
                                .expect("catalog attempt has a verified supervisor identity"),
                        ) {
                            eprintln!("could not persist native session handoff: {error}");
                            begin_stop(
                                leg_pid,
                                &root_identity,
                                status.is_none(),
                                &mut observed,
                                &mut stop_started,
                                &mut stop_deadline,
                            );
                            continue;
                        }
                        handoff_recorded = true;
                    }
                    creation_lock.take();
                    session_id_seen = true;
                }
                if let Err(error) = write_raw_record(&line) {
                    if error.kind() == io::ErrorKind::BrokenPipe {
                        begin_stop(
                            leg_pid,
                            &root_identity,
                            status.is_none(),
                            &mut observed,
                            &mut stop_started,
                            &mut stop_deadline,
                        );
                    } else {
                        eprintln!("could not forward leg stream record: {error}");
                        begin_stop(
                            leg_pid,
                            &root_identity,
                            status.is_none(),
                            &mut observed,
                            &mut stop_started,
                            &mut stop_deadline,
                        );
                    }
                }
            }
            Ok(Message::Output(Err(error))) => {
                eprintln!("could not read native leg stdout: {error}");
                stdout_closed = true;
                begin_stop(
                    leg_pid,
                    &root_identity,
                    status.is_none(),
                    &mut observed,
                    &mut stop_started,
                    &mut stop_deadline,
                );
            }
            Ok(Message::OutputClosed) => stdout_closed = true,
            Ok(Message::Control(Control::Stop)) | Ok(Message::Control(Control::Disconnected)) => {
                begin_stop(
                    leg_pid,
                    &root_identity,
                    status.is_none(),
                    &mut observed,
                    &mut stop_started,
                    &mut stop_deadline,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => stdout_closed = true,
        }
    }

    if status.is_none() {
        status = child.wait().ok();
    }
    // `try_wait` reaps on success, but calling wait again is harmless and makes
    // the guard's lifetime visibly extend through process completion.
    let child_code = status.and_then(exit_code).unwrap_or(1);
    let stderr = stderr_thread.join().unwrap_or_default();
    let stderr = redact_credentials(&String::from_utf8_lossy(&stderr));
    if !stderr.is_empty() {
        let _ = io::stderr().write_all(stderr.as_bytes());
        let _ = io::stderr().flush();
    }
    let _ = write_control(&json!({"_supervisor":"finished","forced":forced}));
    drop(session_lock.take());
    drop(creation_lock.take());
    if forced { Ok(124) } else { Ok(child_code) }
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("could not open lock file {}: {error}", path.display()))
}

pub(crate) fn try_session_lock(store_dir: &Path, session_id: &str) -> Result<Option<File>, String> {
    let path = store_dir.join(format!(".leg-ui-session-{session_id}.lock"));
    let _coordination = lock_coordination(&path, session_id)?;
    let lock = open_lock_file(&path)?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(format!("could not lock session {session_id}: {error}")),
    }
}

/// Reads whether a driver lock is held without creating the primary lock. The
/// short coordination sidecar keeps this display probe from reserving the
/// primary lock while a supervisor is claiming ownership.
pub(crate) fn session_lock_is_held(store_dir: &Path, session_id: &str) -> Result<bool, String> {
    let path = store_dir.join(format!(".leg-ui-session-{session_id}.lock"));
    lock_is_held_if_present(&path, session_id)
}

fn lock_is_held_if_present(path: &Path, label: &str) -> Result<bool, String> {
    lock_is_held_if_present_after_coordination_release(path, label, || {})
}

/// The callback marks a deterministic test boundary after both locks are released.
fn lock_is_held_if_present_after_coordination_release(
    path: &Path,
    label: &str,
    after_coordination_release: impl FnOnce(),
) -> Result<bool, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let lock = match options.open(path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not open {label} lock: {error}")),
    };
    let coordination = lock_coordination(path, label)?;
    match lock.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&lock)
                .map_err(|error| format!("could not release {label} lock probe: {error}"))?;
            drop(coordination);
            after_coordination_release();
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(format!("could not inspect {label} lock: {error}")),
    }
}

fn acquire_creation_lock(store_dir: &Path) -> Result<File, String> {
    let path = store_dir.join(".leg-ui-client-create.lock");
    let lock = open_lock_file(&path)?;
    let coordination = lock_coordination(&path, "session creation")?;
    match lock.try_lock_exclusive() {
        Ok(()) => {
            drop(coordination);
            Ok(lock)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            drop(coordination);
            // Wait without the sidecar; the exclusive primary lock marks ownership to probes.
            lock.lock_exclusive()
                .map_err(|error| format!("could not acquire session creation guard: {error}"))?;
            Ok(lock)
        }
        Err(error) => Err(format!("could not acquire session creation guard: {error}")),
    }
}

fn lock_coordination(path: &Path, label: &str) -> Result<File, String> {
    let mut name = path
        .file_name()
        .ok_or_else(|| format!("invalid {label} lock path"))?
        .to_os_string();
    name.push(".coord");
    let guard_path = path.with_file_name(name);
    let guard = open_lock_file(&guard_path)
        .map_err(|error| format!("could not open {label} lock coordination guard: {error}"))?;
    guard
        .lock_exclusive()
        .map_err(|error| format!("could not acquire {label} lock coordination guard: {error}"))?;
    Ok(guard)
}

fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn native_session_id(id: &str) -> bool {
    let Some(suffix) = id.strip_prefix("sess-") else {
        return false;
    };
    let components = suffix.split('-').collect::<Vec<_>>();
    (components.len() == 2 || components.len() == 3)
        && components.iter().all(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        })
}

fn spawn_output_reader(stdout: ChildStdout, tx: mpsc::Sender<Message>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            match read_bounded_line(&mut reader, &mut line, MAX_STREAM_RECORD) {
                Ok(true) => {
                    if tx.send(Message::Output(Ok(line.clone()))).is_err() {
                        return;
                    }
                }
                Ok(false) => {
                    let _ = tx.send(Message::OutputClosed);
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Message::Output(Err(error.to_string())));
                    return;
                }
            }
        }
    });
}

fn spawn_control_reader(input: BufReader<io::Stdin>, tx: mpsc::Sender<Message>) {
    thread::spawn(move || {
        let mut input = input;
        let mut line = Vec::new();
        loop {
            match read_bounded_line(&mut input, &mut line, MAX_START_REQUEST) {
                Ok(false) => {
                    let _ = tx.send(Message::Control(Control::Disconnected));
                    return;
                }
                Err(_) => {
                    let _ = tx.send(Message::Control(Control::Disconnected));
                    return;
                }
                Ok(true) => {
                    if serde_json::from_slice::<Value>(&line)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("kind")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .as_deref()
                        == Some("stop")
                    {
                        let _ = tx.send(Message::Control(Control::Stop));
                    }
                }
            }
        }
    });
}

fn managed_session_trail_exists(store_dir: &Path, session_id: &str) -> bool {
    let path = store_dir.join(format!("{session_id}.jsonl"));
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    fs::canonicalize(path)
        .ok()
        .and_then(|canonical| canonical.parent().map(Path::to_path_buf))
        .as_deref()
        == Some(store_dir)
}

fn is_turn_start_for_new_session(line: &[u8]) -> bool {
    serde_json::from_slice::<Value>(line)
        .ok()
        .is_some_and(|value| {
            value.get("event").and_then(Value::as_str) == Some("turn_start")
                && value.get("schema").and_then(Value::as_str) == Some("leg.exchange.stream/v1")
        })
}

fn extract_session_id(line: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(line)
        .ok()?
        .get("session_id")?
        .as_str()
        .map(str::to_string)
}

fn update_active_tools(line: &[u8], active: &mut HashSet<String>) {
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if record.get("schema").and_then(Value::as_str) != Some("leg.exchange.stream/v1") {
        return;
    }
    let Some(id) = record.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    match record.get("event").and_then(Value::as_str) {
        Some("tool_call") => {
            active.insert(id.to_string());
        }
        Some("tool_result") => {
            active.remove(id);
        }
        _ => {}
    }
}

fn begin_stop(
    leg_pid: u32,
    root_identity: &ProcessIdentity,
    child_running: bool,
    observed: &mut HashSet<ProcessIdentity>,
    started: &mut Option<Instant>,
    deadline: &mut Option<Instant>,
) {
    if started.is_some() {
        return;
    }
    observed.extend(snapshot_owned_processes(root_identity));
    observed.insert(root_identity.clone());
    #[cfg(unix)]
    if child_running && current_identity(root_identity) {
        // Child::try_wait has not reaped this child, so the PID cannot have
        // been reused by an unrelated process.
        unsafe {
            let _ = libc::kill(leg_pid as libc::pid_t, libc::SIGINT);
        }
    }
    #[cfg(windows)]
    {
        if child_running && current_identity(root_identity) {
            send_windows_ctrl_break(leg_pid);
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = leg_pid;
    }
    *started = Some(Instant::now());
    *deadline = Some(Instant::now() + STOP_GRACE);
}

fn capture_process_identity(pid: u32) -> Option<ProcessIdentity> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    system
        .processes()
        .iter()
        .find(|(candidate, _)| candidate.as_u32() == pid)
        .and_then(|(_, process)| process_identity(pid, process))
}

fn snapshot_owned_processes(root: &ProcessIdentity) -> HashSet<ProcessIdentity> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let Some((_, process)) = system
        .processes()
        .iter()
        .find(|(pid, _)| pid.as_u32() == root.owner.pid)
    else {
        return HashSet::new();
    };
    if process_identity(root.owner.pid, process).as_ref() != Some(root) {
        return HashSet::new();
    }
    let mut processes = HashMap::new();
    for (pid, process) in system.processes() {
        let raw_pid = pid.as_u32();
        let parent = process.parent().map(|parent| parent.as_u32());
        if let Some(identity) = process_identity(raw_pid, process) {
            processes.insert(raw_pid, (parent, identity));
        }
    }
    let mut descendants = HashSet::new();
    let mut queue = VecDeque::from([root.owner.pid]);
    while let Some(parent) = queue.pop_front() {
        for (pid, (candidate_parent, identity)) in &processes {
            if *candidate_parent == Some(parent)
                && *pid != root.owner.pid
                && descendants.insert(identity.clone())
            {
                queue.push_back(*pid);
            }
        }
    }
    descendants
}

fn process_group(pid: u32) -> i32 {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return -1;
        };
        let group = unsafe { libc::getpgid(pid) };
        if group < 0 { -1 } else { group }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        -1
    }
}

#[cfg(windows)]
fn send_windows_ctrl_break(process_group_id: u32) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

    // SAFETY: `leg` starts in its own console process group; Windows scopes
    // this control event to that group rather than the TUI's group.
    unsafe {
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id);
    }
}

fn process_identity(pid: u32, process: &sysinfo::Process) -> Option<ProcessIdentity> {
    if process.status() == ProcessStatus::Zombie {
        return None;
    }
    Some(ProcessIdentity {
        owner: ProcessOwner::capture(pid).ok()?,
        pgid: process_group(pid),
        executable: process.exe().map(Path::to_path_buf),
    })
}

fn current_identity(identity: &ProcessIdentity) -> bool {
    if identity.owner.inspect() != OwnerState::Alive {
        return false;
    }
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    system.processes().iter().any(|(pid, process)| {
        pid.as_u32() == identity.owner.pid
            && process.status() != ProcessStatus::Zombie
            && process_identity(identity.owner.pid, process).as_ref() == Some(identity)
    })
}

fn any_observed_alive(observed: &HashSet<ProcessIdentity>) -> bool {
    observed.iter().any(current_identity)
}

fn terminate_if_birth_token_matches<R>(
    expected_birth_token: &str,
    actual_birth_token: Option<&str>,
    terminate: impl FnOnce() -> R,
) -> Option<R> {
    (actual_birth_token == Some(expected_birth_token)).then(terminate)
}

#[cfg(windows)]
fn terminate_windows_process_if_identity_matches(identity: &ProcessIdentity) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };

    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            identity.owner.pid,
        )
    };
    if handle.is_null() {
        return false;
    }

    let mut created = unsafe { std::mem::zeroed::<FILETIME>() };
    let mut exited = unsafe { std::mem::zeroed::<FILETIME>() };
    let mut kernel = unsafe { std::mem::zeroed::<FILETIME>() };
    let mut user = unsafe { std::mem::zeroed::<FILETIME>() };
    let birth_token =
        (unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
            != 0)
            .then(|| {
                let created =
                    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
                format!("windows:{created}")
            });
    let terminated = terminate_if_birth_token_matches(
        &identity.owner.birth_token,
        birth_token.as_deref(),
        || unsafe { TerminateProcess(handle, 1) != 0 },
    )
    .unwrap_or(false);
    unsafe {
        CloseHandle(handle);
    }
    terminated
}

fn force_owned_tree(observed: &HashSet<ProcessIdentity>, child: &mut Child, child_running: bool) {
    #[cfg(unix)]
    {
        for identity in observed {
            if current_identity(identity) {
                unsafe {
                    let _ = libc::kill(identity.owner.pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
    #[cfg(windows)]
    {
        for identity in observed {
            let _ = terminate_windows_process_if_identity_matches(identity);
        }
    }
    if child_running {
        // This Child has not been reaped; its PID still names our process.
        let _ = child.kill();
    }
}

fn write_control(value: &Value) -> io::Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn write_raw_record(line: &[u8]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    writer.write_all(line)?;
    if !line.ends_with(b"\n") {
        writer.write_all(b"\n")?;
    }
    writer.flush()
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
                "stream record too large",
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

fn drain_capped(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return kept,
            Ok(count) => {
                let remaining = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(remaining)]);
            }
        }
    }
}

fn redact_credentials(stderr: &str) -> String {
    crate::client::redact_credentials(stderr.to_string())
}

fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| 128 + signal)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn session_ids_match_core_filename_contract() {
        assert!(safe_session_id("abc-123_X"));
        assert!(!safe_session_id(""));
        assert!(!safe_session_id("../trail"));
        assert!(!safe_session_id("你好"));
        assert!(native_session_id("sess-12-34"));
        assert!(native_session_id("sess-12-34-5"));
        assert!(!native_session_id("session-12-34"));
        assert!(!native_session_id("sess-12"));
        assert!(!native_session_id("sess-12-x"));
        assert!(!native_session_id("sess-12-34/../trail"));
    }

    #[test]
    fn stream_tool_events_track_process_work_until_its_result() {
        let mut active = HashSet::new();
        update_active_tools(
            br#"{"schema":"leg.exchange.stream/v1","event":"tool_call","tool_use_id":"toolu-1"}"#,
            &mut active,
        );
        assert!(active.contains("toolu-1"));
        update_active_tools(
            br#"{"schema":"leg.exchange.stream/v1","event":"tool_result","tool_use_id":"toolu-1"}"#,
            &mut active,
        );
        assert!(active.is_empty());
    }

    #[test]
    fn process_birth_identity_is_verifiable() {
        #[cfg(target_os = "linux")]
        {
            let owner = ProcessOwner::current().expect("capture current process birth identity");
            assert!(owner.birth_token.starts_with("linux:"));
            assert_eq!(owner.inspect(), OwnerState::Alive);
        }
    }

    #[test]
    fn reused_pid_identity_is_not_terminated() {
        let mut terminated = false;
        let result =
            terminate_if_birth_token_matches("windows:expected", Some("windows:reused"), || {
                terminated = true
            });
        assert_eq!(result, None);
        assert!(!terminated);
    }

    #[cfg(unix)]
    #[test]
    fn reused_pid_identity_is_never_signaled() {
        let mut child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("start process for identity check");
        let mut identity =
            capture_process_identity(child.id()).expect("capture child process birth identity");
        identity.owner.birth_token.push_str("-reused-pid");

        force_owned_tree(&HashSet::from([identity]), &mut child, false);
        assert!(child.try_wait().expect("inspect child status").is_none());
        child.kill().expect("clean up identity test child");
        let _ = child.wait();
    }

    #[test]
    fn display_probe_releases_primary_before_coordination_guard() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let store = scratch.path();
        let session_id = "sess-123-456";
        let lock_path = store.join(format!(".leg-ui-session-{session_id}.lock"));
        File::create(&lock_path).expect("create primary lock");

        let (released_tx, released_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let probe_path = lock_path.clone();
        let probe = std::thread::spawn(move || {
            lock_is_held_if_present_after_coordination_release(&probe_path, session_id, || {
                released_tx.send(()).expect("signal guard release");
                resume_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("resume probe");
            })
        });

        released_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("probe released its coordination guard");
        let lock = try_session_lock(store, session_id)
            .expect("acquire session lock after probe")
            .expect("probe must release the primary lock before its guard");
        FileExt::unlock(&lock).expect("release session lock");
        resume_tx.send(()).expect("resume probe");
        assert!(!probe.join().expect("join probe").expect("probe succeeds"));
    }
}
