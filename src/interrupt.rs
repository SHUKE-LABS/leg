//! Unix signal state shared by the CLI, tool loop, and bash process runner.

use crate::error::{InterruptSignal, LegError, Result};

#[cfg(unix)]
use std::io::{self, BufRead, Read};
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

#[cfg(unix)]
static SIGNAL: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static ACTIVE_PROCESS_GROUP: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static SIGNAL_PIPE_READ: AtomicI32 = AtomicI32::new(-1);
#[cfg(unix)]
static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

#[cfg(unix)]
pub(crate) fn install() -> Result<()> {
    if ENABLED.load(Ordering::Acquire) {
        return Ok(());
    }
    let (read_fd, write_fd) = create_signal_pipe()?;
    SIGNAL_PIPE_READ.store(read_fd, Ordering::Release);
    SIGNAL_PIPE_WRITE.store(write_fd, Ordering::Release);
    install_handler(libc::SIGINT)?;
    install_handler(libc::SIGTERM)?;
    ENABLED.store(true, Ordering::Release);
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn install() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn install_handler(signal: libc::c_int) -> Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handle_signal as *const () as usize;
    action.sa_flags = 0;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
        return Err(LegError::Io(format!(
            "failed to initialize signal handler: {}",
            std::io::Error::last_os_error()
        )));
    }
    if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } == -1 {
        return Err(LegError::Io(format!(
            "failed to install signal handler: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(unix)]
extern "C" fn handle_signal(signal: libc::c_int) {
    if SIGNAL
        .compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        let process_group = ACTIVE_PROCESS_GROUP.load(Ordering::Relaxed);
        if process_group > 0 {
            // A forced exit must not orphan an active tool process group.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
        unsafe { libc::_exit(128 + signal) };
    }
    let signal_fd = SIGNAL_PIPE_WRITE.load(Ordering::Relaxed);
    if signal_fd >= 0 {
        let byte = signal as u8;
        unsafe {
            libc::write(signal_fd, (&byte as *const u8).cast(), 1);
        }
    }
}

#[cfg(unix)]
fn create_signal_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(LegError::Io(format!(
            "failed to create signal pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    // Do not let a missing standard stream turn a pipe end into stdin/stdout/stderr.
    for index in 0..fds.len() {
        if fds[index] <= libc::STDERR_FILENO {
            let duplicate =
                unsafe { libc::fcntl(fds[index], libc::F_DUPFD_CLOEXEC, libc::STDERR_FILENO + 1) };
            if duplicate == -1 {
                return close_signal_pipe(
                    fds,
                    format!(
                        "failed to move signal pipe away from standard streams: {}",
                        std::io::Error::last_os_error()
                    ),
                );
            }
            unsafe {
                libc::close(fds[index]);
            }
            fds[index] = duplicate;
        }
    }
    for fd in fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return close_signal_pipe(
                fds,
                format!(
                    "failed to configure signal pipe: {}",
                    std::io::Error::last_os_error()
                ),
            );
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
        {
            return close_signal_pipe(
                fds,
                format!(
                    "failed to configure signal pipe: {}",
                    std::io::Error::last_os_error()
                ),
            );
        }
    }
    Ok((fds[0], fds[1]))
}

#[cfg(unix)]
fn close_signal_pipe(fds: [RawFd; 2], message: String) -> Result<(RawFd, RawFd)> {
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
    Err(LegError::Io(message))
}

pub(crate) fn signal() -> Option<InterruptSignal> {
    #[cfg(unix)]
    {
        match SIGNAL.load(Ordering::Relaxed) {
            libc::SIGINT => Some(InterruptSignal::Interrupt),
            libc::SIGTERM => Some(InterruptSignal::Terminate),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub(crate) fn error() -> Option<LegError> {
    signal().map(|signal| LegError::Interrupted { signal })
}

pub(crate) fn check() -> Result<()> {
    match error() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(crate) fn is_signaled() -> bool {
    signal().is_some()
}

#[cfg(unix)]
/// Polls stdin together with the signal pipe, so a signal wakes blocked reads.
pub(crate) struct SignalAwareStdin {
    fd: RawFd,
    buffer: [u8; 8192],
    start: usize,
    end: usize,
}

#[cfg(unix)]
impl SignalAwareStdin {
    pub(crate) fn stdin() -> Self {
        Self {
            fd: libc::STDIN_FILENO,
            buffer: [0; 8192],
            start: 0,
            end: 0,
        }
    }

    fn refill(&mut self) -> io::Result<()> {
        loop {
            if is_signaled() {
                return Err(io::Error::other("input interrupted by signal"));
            }
            let signal_fd = SIGNAL_PIPE_READ.load(Ordering::Acquire);
            if signal_fd < 0 {
                return Err(io::Error::other("signal handlers are not installed"));
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: signal_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if ready == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    if is_signaled() {
                        return Err(io::Error::other("input interrupted by signal"));
                    }
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                return Err(error);
            }
            if is_signaled() || fds[1].revents != 0 {
                return Err(io::Error::other("input interrupted by signal"));
            }
            if fds[0].revents & libc::POLLNVAL != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stdin is not pollable",
                ));
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                continue;
            }

            let read =
                unsafe { libc::read(self.fd, self.buffer.as_mut_ptr().cast(), self.buffer.len()) };
            if read >= 0 {
                self.start = 0;
                self.end = read as usize;
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                if is_signaled() {
                    return Err(io::Error::other("input interrupted by signal"));
                }
                continue;
            }
            return Err(error);
        }
    }
}

#[cfg(unix)]
impl Read for SignalAwareStdin {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let buffer = self.fill_buf()?;
        let length = buffer.len().min(output.len());
        output[..length].copy_from_slice(&buffer[..length]);
        self.consume(length);
        Ok(length)
    }
}

#[cfg(unix)]
impl BufRead for SignalAwareStdin {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.start == self.end {
            self.refill()?;
        }
        Ok(&self.buffer[self.start..self.end])
    }

    fn consume(&mut self, amount: usize) {
        self.start = (self.start + amount).min(self.end);
    }
}

#[cfg(unix)]
pub(crate) struct ActiveProcessGroup;

#[cfg(unix)]
impl ActiveProcessGroup {
    pub(crate) fn new(pid: u32) -> Self {
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            ACTIVE_PROCESS_GROUP.store(pid, Ordering::Release);
        }
        Self
    }
}

#[cfg(unix)]
impl Drop for ActiveProcessGroup {
    fn drop(&mut self) {
        ACTIVE_PROCESS_GROUP.store(0, Ordering::Release);
    }
}

#[cfg(unix)]
pub(crate) fn run_cancellable<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};
    use std::time::Duration;

    check()?;
    if !ENABLED.load(Ordering::Acquire) {
        return operation();
    }

    let (sender, receiver) = sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("leg-provider-request".to_string())
        .spawn(move || {
            let _ = sender.send(operation());
        })
        .map_err(|error| {
            LegError::Transport(format!("failed to start provider request: {error}"))
        })?;

    loop {
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => {
                if worker.join().is_err() {
                    return Err(LegError::Transport(
                        "provider request worker panicked".to_string(),
                    ));
                }
                check()?;
                return result;
            }
            Err(RecvTimeoutError::Timeout) => check()?,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(if worker.join().is_err() {
                    LegError::Transport("provider request worker panicked".to_string())
                } else {
                    LegError::Transport(
                        "provider request worker exited without a result".to_string(),
                    )
                });
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn run_cancellable<T, F>(operation: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    operation()
}
