//! Bounded synchronous subprocess execution and output capture for `bash`.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::interrupt;

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
const HALF_LINES: usize = MAX_LINES / 2;
const HALF_BYTES: usize = MAX_BYTES / 2;
const TERMINATION_GRACE: Duration = Duration::from_millis(50);
const PIPE_DRAIN_GUARD: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_DRAIN_PER_STREAM: usize = 64 * 1024;

pub(super) struct ProcessOutput {
    pub(super) wall_time_seconds: f64,
    pub(super) status: &'static str,
    pub(super) exit_code: i32,
    pub(super) stdout: CappedOutput,
    pub(super) stderr: CappedOutput,
}

pub(super) struct CappedOutput {
    pub(super) text: String,
    pub(super) omitted_bytes: u64,
}

pub(super) fn run(
    shell: &OsStr,
    command: &str,
    timeout: Duration,
    env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> io::Result<ProcessOutput> {
    let started = Instant::now();
    let mut command_builder = Command::new(shell);
    command_builder
        .arg("-lc")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // On Windows, Rust searches the system directory before the parent's
    // `PATH` unless the child's `PATH` is set explicitly, so a bare `bash`
    // would resolve to the WSL launcher in System32 ahead of Git Bash.
    // Re-setting the inherited `PATH` makes the `PATH` search come first.
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("PATH") {
        command_builder.env("PATH", path);
    }
    command_builder.envs(env.iter().map(|(key, value)| (key, value)));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command_builder.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command_builder.creation_flags(windows::CREATE_NEW_PROCESS_GROUP_FLAG);
    }

    #[cfg(windows)]
    let job = windows::JobObject::new()?;

    let mut child = command_builder.spawn()?;
    #[cfg(unix)]
    let _active_process_group = interrupt::ActiveProcessGroup::new(child.id());

    #[cfg(windows)]
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    #[cfg(unix)]
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        terminate_force(&mut child);
        let _ = child.wait();
        return Err(error);
    }

    let mut stdout = CapturedPipe::new(stdout);
    let mut stderr = CapturedPipe::new(stderr);
    let mut exit_status = None;
    let mut exited_at = None;
    let mut timed_out = false;
    let mut graceful_at = None;
    let mut forced_at = None;

    loop {
        stdout.drain();
        stderr.drain();

        if exit_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_status = Some(status);
                    exited_at = Some(Instant::now());
                }
                Ok(None) => {}
                Err(error) => {
                    #[cfg(unix)]
                    terminate_force(&mut child);
                    #[cfg(windows)]
                    terminate_force(&mut child, &job);
                    let _ = child.wait();
                    return Err(error);
                }
            }
        }

        let now = Instant::now();
        if interrupt::is_signaled() && graceful_at.is_none() {
            graceful_at = Some(now);
            terminate_gracefully(&child);
        }

        if !timed_out
            && graceful_at.is_none()
            && exit_status.is_none()
            && now.duration_since(started) >= timeout
        {
            timed_out = true;
            graceful_at = Some(now);
            terminate_gracefully(&child);
        }

        if forced_at.is_none()
            && graceful_at.is_some_and(|sent| now.duration_since(sent) >= TERMINATION_GRACE)
        {
            #[cfg(unix)]
            terminate_force(&mut child);
            #[cfg(windows)]
            terminate_force(&mut child, &job);
            forced_at = Some(now);
        }

        if exit_status.is_some() && stdout.is_closed() && stderr.is_closed() {
            break;
        }

        let drain_started = if timed_out { forced_at } else { exited_at };
        if drain_started.is_some_and(|at| now.duration_since(at) >= PIPE_DRAIN_GUARD) {
            stdout.close();
            stderr.close();
            if exit_status.is_none() {
                #[cfg(unix)]
                terminate_force(&mut child);
                #[cfg(windows)]
                terminate_force(&mut child, &job);
                match child.wait() {
                    Ok(status) => exit_status = Some(status),
                    Err(error) => return Err(error),
                }
            }
            break;
        }

        thread::sleep(POLL_INTERVAL);
    }

    let status = exit_status.expect("the process is reaped before returning");
    let exit_code = if timed_out { 124 } else { exit_code(status) };
    Ok(ProcessOutput {
        wall_time_seconds: started.elapsed().as_secs_f64(),
        status: if timed_out { "timed_out" } else { "exited" },
        exit_code,
        stdout: stdout.capture.finish(),
        stderr: stderr.capture.finish(),
    })
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn terminate_gracefully(child: &Child) {
    #[cfg(unix)]
    {
        signal_process_group(child.id(), libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        windows::send_ctrl_break(child.id());
    }
}

#[cfg(unix)]
fn terminate_force(child: &mut Child) {
    signal_process_group(child.id(), libc::SIGKILL);
    let _ = child.kill();
}

#[cfg(windows)]
fn terminate_force(child: &mut Child, job: &windows::JobObject) {
    job.terminate();
    let _ = child.kill();
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // The child was placed in a new group before exec; a negative pid targets
    // the shell and commands it launched in that group.
    // ESRCH is expected when all processes in the group already exited.
    let _ = unsafe { libc::kill(-pid, signal) };
}

#[cfg(unix)]
fn set_nonblocking<R: std::os::fd::AsRawFd>(pipe: &R) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: `fd` is an open pipe descriptor owned by the child handle.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` remains open and the flags preserve its existing mode.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct CapturedPipe<R> {
    reader: Option<R>,
    capture: Capture,
}

#[cfg(unix)]
trait PipeReader: Read {}

#[cfg(unix)]
impl<R: Read> PipeReader for R {}

#[cfg(windows)]
trait PipeReader: Read + std::os::windows::io::AsRawHandle {}

#[cfg(windows)]
impl<R: Read + std::os::windows::io::AsRawHandle> PipeReader for R {}

impl<R: PipeReader> CapturedPipe<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: Some(reader),
            capture: Capture::default(),
        }
    }

    fn drain(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        let mut buffer = [0; 8192];
        let mut closed = false;
        let mut drained = 0;
        loop {
            match read_available(reader, &mut buffer) {
                Ok(ReadResult::Data(length)) => {
                    self.capture.push(&buffer[..length]);
                    drained += length;
                    if drained >= MAX_DRAIN_PER_STREAM {
                        break;
                    }
                }
                Ok(ReadResult::Pending) => break,
                Ok(ReadResult::Closed) | Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            self.reader = None;
        }
    }

    fn close(&mut self) {
        self.reader = None;
    }

    fn is_closed(&self) -> bool {
        self.reader.is_none()
    }
}

enum ReadResult {
    Data(usize),
    Pending,
    Closed,
}

#[cfg(unix)]
fn read_available<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<ReadResult> {
    match reader.read(buffer) {
        Ok(0) => Ok(ReadResult::Closed),
        Ok(length) => Ok(ReadResult::Data(length)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(ReadResult::Pending),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(ReadResult::Pending),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn read_available<R: Read + std::os::windows::io::AsRawHandle>(
    reader: &mut R,
    buffer: &mut [u8],
) -> io::Result<ReadResult> {
    let Some(available) = windows::available_pipe_bytes(reader)? else {
        return Ok(ReadResult::Closed);
    };
    if available == 0 {
        return Ok(ReadResult::Pending);
    }
    let length = buffer.len().min(available as usize);
    match reader.read(&mut buffer[..length]) {
        Ok(0) => Ok(ReadResult::Closed),
        Ok(length) => Ok(ReadResult::Data(length)),
        Err(error) if windows::is_closed_pipe(&error) => Ok(ReadResult::Closed),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(ReadResult::Pending),
        Err(error) => Err(error),
    }
}

#[derive(Default)]
struct Capture {
    state: CaptureState,
    total_bytes: u64,
}

enum CaptureState {
    Full {
        bytes: Vec<u8>,
        newlines: usize,
    },
    Truncated {
        head: Vec<u8>,
        tail: Vec<u8>,
        tail_newlines: usize,
    },
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::Full {
            bytes: Vec::new(),
            newlines: 0,
        }
    }
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let mut index = 0;
        while index < bytes.len() {
            let exceeded = match &mut self.state {
                CaptureState::Full {
                    bytes: output,
                    newlines,
                } => {
                    let byte = bytes[index];
                    output.push(byte);
                    *newlines += usize::from(byte == b'\n');
                    self.total_bytes = self.total_bytes.saturating_add(1);
                    index += 1;
                    output.len() > MAX_BYTES
                        || *newlines + usize::from(output.last() != Some(&b'\n')) > MAX_LINES
                }
                CaptureState::Truncated {
                    tail,
                    tail_newlines,
                    ..
                } => {
                    append_tail(tail, tail_newlines, &bytes[index..]);
                    self.total_bytes = self
                        .total_bytes
                        .saturating_add((bytes.len() - index) as u64);
                    return;
                }
            };

            if exceeded {
                let previous = std::mem::take(&mut self.state);
                let CaptureState::Full { bytes, .. } = previous else {
                    unreachable!("only full captures cross the initial output cap")
                };
                let (head, tail, tail_newlines) = split_initial(&bytes);
                self.state = CaptureState::Truncated {
                    head,
                    tail,
                    tail_newlines,
                };
            }
        }
    }

    fn finish(self) -> CappedOutput {
        match self.state {
            CaptureState::Full { bytes, .. } => CappedOutput {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                omitted_bytes: 0,
            },
            CaptureState::Truncated {
                head,
                tail,
                tail_newlines: _,
            } => {
                let omitted_bytes = self
                    .total_bytes
                    .saturating_sub((head.len() + tail.len()) as u64);
                let head = String::from_utf8_lossy(&head);
                let tail = String::from_utf8_lossy(&tail);
                let text =
                    format!("{head}\n...[truncated: {omitted_bytes} bytes omitted]...\n{tail}");
                CappedOutput {
                    text,
                    omitted_bytes,
                }
            }
        }
    }
}

fn split_initial(bytes: &[u8]) -> (Vec<u8>, Vec<u8>, usize) {
    let mut head = Vec::with_capacity(HALF_BYTES);
    let mut head_newlines = 0;
    for byte in bytes {
        if head.len() == HALF_BYTES || head_newlines == HALF_LINES {
            break;
        }
        head.push(*byte);
        head_newlines += usize::from(*byte == b'\n');
    }

    let mut tail = Vec::with_capacity(HALF_BYTES);
    let mut tail_newlines = 0;
    append_tail(&mut tail, &mut tail_newlines, bytes);
    (head, tail, tail_newlines)
}

fn append_tail(tail: &mut Vec<u8>, newlines: &mut usize, bytes: &[u8]) {
    *newlines += bytes.iter().filter(|byte| **byte == b'\n').count();
    tail.extend_from_slice(bytes);
    if tail.len() > HALF_BYTES {
        let excess = tail.len() - HALF_BYTES;
        *newlines -= tail[..excess].iter().filter(|byte| **byte == b'\n').count();
        tail.drain(..excess);
    }

    while *newlines + usize::from(tail.last().is_some_and(|byte| *byte != b'\n')) > HALF_LINES {
        let Some(first_line_end) = tail.iter().position(|byte| *byte == b'\n') else {
            break;
        };
        tail.drain(..=first_line_end);
        *newlines -= 1;
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr::null_mut;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CTRL_BREAK_EVENT: u32 = 1;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const ERROR_BROKEN_PIPE: i32 = 109;
    const ERROR_NO_DATA: i32 = 232;
    const ERROR_PIPE_NOT_CONNECTED: i32 = 233;

    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: Dword,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: Dword,
        affinity: usize,
        priority_class: Dword,
        scheduling_class: Dword,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: i32,
            information: *const c_void,
            information_length: Dword,
        ) -> Bool;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> Bool;
        fn GenerateConsoleCtrlEvent(event: Dword, process_group_id: Dword) -> Bool;
        fn PeekNamedPipe(
            pipe: Handle,
            buffer: *mut c_void,
            buffer_size: Dword,
            bytes_read: *mut Dword,
            total_bytes_available: *mut Dword,
            bytes_left_this_message: *mut Dword,
        ) -> Bool;
    }

    pub(super) const CREATE_NEW_PROCESS_GROUP_FLAG: u32 = CREATE_NEW_PROCESS_GROUP;

    pub(super) struct JobObject(OwnedHandle);

    impl JobObject {
        pub(super) fn new() -> io::Result<Self> {
            // SAFETY: null attributes/name request an unnamed job object.
            let handle = unsafe { CreateJobObjectW(null_mut(), null_mut()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateJobObjectW returned a fresh owned handle.
            let owned = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
            Ok(Self(owned))
        }

        pub(super) fn assign(&self, child: &Child) -> io::Result<()> {
            // SAFETY: both handles are live and owned for this call.
            let ok = unsafe {
                AssignProcessToJobObject(
                    self.0.as_raw_handle() as Handle,
                    child.as_raw_handle() as Handle,
                )
            };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn terminate(&self) {
            // SAFETY: the job handle remains alive until command cleanup ends.
            let terminated = unsafe { TerminateJobObject(self.0.as_raw_handle() as Handle, 1) };
            if terminated == 0 {
                // If force termination fails, close-time cleanup is a fallback
                // for any job members that still have inherited pipe handles.
                let mut information = ExtendedLimitInformation::default();
                information.basic_limit_information.limit_flags =
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                // SAFETY: the structure has the class's required C layout/size.
                unsafe {
                    SetInformationJobObject(
                        self.0.as_raw_handle() as Handle,
                        JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                        &information as *const _ as *const c_void,
                        size_of::<ExtendedLimitInformation>() as Dword,
                    );
                }
            }
        }
    }

    pub(super) fn send_ctrl_break(process_group_id: u32) {
        // SAFETY: this only requests a console control event for the child's
        // process group; it may fail when this process has no console.
        unsafe {
            GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id);
        }
    }

    pub(super) fn available_pipe_bytes<R: AsRawHandle>(reader: &R) -> io::Result<Option<u32>> {
        let mut available = 0;
        // SAFETY: the borrowed handle is a live child pipe and no buffer is
        // requested; PeekNamedPipe reports its currently available bytes.
        let ok = unsafe {
            PeekNamedPipe(
                reader.as_raw_handle() as Handle,
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if is_closed_pipe(&error) {
                Ok(None)
            } else {
                Err(error)
            }
        } else {
            Ok(Some(available))
        }
    }

    pub(super) fn is_closed_pipe(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(ERROR_BROKEN_PIPE) | Some(ERROR_NO_DATA) | Some(ERROR_PIPE_NOT_CONNECTED)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_output_is_preserved_without_truncation() {
        let mut capture = Capture::default();
        capture.push(b"first\nsecond");
        let output = capture.finish();
        assert_eq!(output.text, "first\nsecond");
        assert_eq!(output.omitted_bytes, 0);
    }

    #[test]
    fn byte_cap_keeps_head_and_tail_and_exact_omitted_count() {
        let mut capture = Capture::default();
        capture.push(&vec![b'x'; MAX_BYTES + 100]);
        let output = capture.finish();
        assert_eq!(output.omitted_bytes, 100);
        assert!(output.text.starts_with(&"x".repeat(HALF_BYTES)));
        assert!(output.text.ends_with(&"x".repeat(HALF_BYTES)));
        assert!(output.text.contains("100 bytes omitted"));
    }

    #[test]
    fn line_cap_keeps_first_and_last_thousand_lines() {
        let mut capture = Capture::default();
        let bytes = (0..=MAX_LINES)
            .map(|line| format!("{line:04}\n"))
            .collect::<String>();
        capture.push(bytes.as_bytes());
        let output = capture.finish();
        assert_eq!(output.omitted_bytes, 5);
        assert!(output.text.starts_with("0000\n"));
        assert!(output.text.contains("...[truncated: 5 bytes omitted]..."));
        assert!(output.text.contains("2000\n"));
    }
}
