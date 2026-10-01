#[cfg(target_os = "linux")]
use std::fs;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub(crate) struct ProcessOwner {
    pub(crate) pid: u32,
    pub(crate) birth_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnerState {
    Alive,
    Dead,
    Reused,
    Unknown(String),
}

impl ProcessOwner {
    pub(crate) fn current() -> Result<Self, String> {
        Self::capture(std::process::id())
    }

    pub(crate) fn capture(pid: u32) -> Result<Self, String> {
        let birth_token = read_birth_token(pid)?
            .ok_or_else(|| format!("process {pid} has no verifiable birth identity"))?;
        Ok(Self { pid, birth_token })
    }

    pub(crate) fn inspect(&self) -> OwnerState {
        if self.pid == 0 || self.birth_token.is_empty() {
            return OwnerState::Unknown("recorded process birth identity is incomplete".into());
        }
        match read_birth_token(self.pid) {
            Ok(None) => OwnerState::Dead,
            Ok(Some(token)) if token == self.birth_token => OwnerState::Alive,
            Ok(Some(_)) => OwnerState::Reused,
            Err(error) => OwnerState::Unknown(error),
        }
    }
}

#[cfg(target_os = "linux")]
fn read_birth_token(pid: u32) -> Result<Option<String>, String> {
    let path = format!("/proc/{pid}/stat");
    let stat = match fs::read_to_string(&path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read process identity {path}: {error}")),
    };
    let Some(close) = stat.rfind(')') else {
        return Err(format!("process identity {path} is malformed"));
    };
    let mut fields = stat[close + 1..].split_whitespace();
    let state = fields
        .next()
        .ok_or_else(|| format!("process identity {path} is incomplete"))?;
    if matches!(state, "Z" | "X") {
        return Ok(None);
    }
    let start_time = fields
        .nth(18)
        .ok_or_else(|| format!("process identity {path} has no start time"))?;
    let start_time = start_time
        .parse::<u64>()
        .map_err(|error| format!("process identity {path} has an invalid start time: {error}"))?;
    Ok(Some(format!("linux:{start_time}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_birth_identity_distinguishes_a_reused_process_id() {
        let owner = ProcessOwner::current().expect("capture current process identity");
        assert_eq!(owner.inspect(), OwnerState::Alive);

        let reused = ProcessOwner {
            pid: owner.pid,
            birth_token: format!("{}-different-incarnation", owner.birth_token),
        };
        assert_eq!(reused.inspect(), OwnerState::Reused);
    }
}

#[cfg(target_os = "macos")]
fn read_birth_token(pid: u32) -> Result<Option<String>, String> {
    let pid = libc::c_int::try_from(pid).map_err(|_| "process id is out of range".to_string())?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let result = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if result != std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int {
        let error = unsafe { *libc::__error() };
        return match error {
            libc::ESRCH => Ok(None),
            libc::EPERM | libc::EACCES => Err(format!(
                "permission denied while checking process {pid} birth identity"
            )),
            _ => Err(format!(
                "could not inspect process {pid} birth identity (errno {error})"
            )),
        };
    }
    let info = unsafe { info.assume_init() };
    Ok(Some(format!(
        "macos:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )))
}

#[cfg(target_os = "windows")]
fn read_birth_token(pid: u32) -> Result<Option<String>, String> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, GetLastError,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let error = unsafe { GetLastError() };
        return if error == ERROR_INVALID_PARAMETER {
            Ok(None)
        } else {
            Err(format!(
                "could not inspect process {pid} birth identity (Windows error {error})"
            ))
        };
    }
    let mut created = unsafe { std::mem::zeroed::<FILETIME>() };
    let mut exited = unsafe { std::mem::zeroed::<FILETIME>() };
    let mut kernel = unsafe { std::mem::zeroed::<FILETIME>() };
    let mut user = unsafe { std::mem::zeroed::<FILETIME>() };
    let times_succeeded =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    let mut exit_code = 0;
    let exit_code_succeeded = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    let error = if times_succeeded == 0 || exit_code_succeeded == 0 {
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    if let Some(error) = error {
        return Err(format!(
            "could not read process {pid} birth identity (Windows error {error})"
        ));
    }
    if exit_code != 259 {
        return Ok(None);
    }
    let token = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Ok(Some(format!("windows:{token}")))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn read_birth_token(pid: u32) -> Result<Option<String>, String> {
    Err(format!(
        "process birth identity checks are unsupported on this platform (pid {pid})"
    ))
}
