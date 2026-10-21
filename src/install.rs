//! The `leg install` command: materializing the native leg binaries into a
//! stable per-user directory.
//!
//! The installer discovers the four native binaries that sit beside the
//! running `leg` executable, copies them into a per-user destination, and — on
//! Windows — registers that destination in the current user's PATH. Unix
//! installations never touch shell startup files.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{LegError, Result};

/// The binaries an install run materializes, in installation order.
pub const COMPANION_BINARIES: [&str; 4] = ["leg", "leg-tui", "leg-web", "leg-ui-supervisor"];

/// The options `leg install` accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOptions {
    /// Overrides the platform's default destination directory.
    pub destination: Option<PathBuf>,
    /// Skips the Windows PATH update.
    pub no_path: bool,
}

/// What an install run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The directory the binaries were installed into.
    pub destination: PathBuf,
    /// The installed file names, in installation order.
    pub installed: Vec<String>,
    /// Whether the destination was added to the current user's PATH.
    pub path_updated: bool,
    /// Whether the destination was already present in the current user's PATH.
    pub path_already_present: bool,
}

/// A native binary an install run copies, paired with its source path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBinary {
    /// The file name used in the destination directory.
    pub file_name: String,
    /// The file the bytes are copied from.
    pub path: PathBuf,
}

/// A point at which the installer deliberately fails. The seam exists so the
/// failure paths are exercised by tests instead of only by chance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultPoint {
    /// No injected failure.
    None,
    /// Fail while staging the copies into the destination.
    Stage,
    /// Fail while committing the staged copies over the installed files.
    Commit,
}

/// The platform's executable file suffix: `.exe` on Windows, empty elsewhere.
pub const fn executable_suffix() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

/// The `leg install` help body.
pub fn help_text() -> String {
    "leg install — install the native leg binaries into a per-user directory\n\n\
     Usage: leg install [--dest <dir>] [--no-path]\n\n\
     Copies leg, leg-tui, leg-web, and leg-ui-supervisor from the directory\n\
     that holds the running leg executable into a stable per-user bin\n\
     directory. The sources must be native binaries: a package-manager shim\n\
     is never copied or executed.\n\n\
     Options:\n\
     \x20 --dest <dir>   Install into <dir> instead of the platform default.\n\
     \x20 --no-path      Do not add the destination to PATH (Windows only).\n\
     \x20 --help, -h     Show this message.\n\n\
     Default destinations:\n\
     \x20 Windows  %LOCALAPPDATA%\\leg\\bin\n\
     \x20 Unix     $XDG_BIN_HOME, else $HOME/.local/bin\n\n\
     On Windows the destination is added to the current user's PATH. The\n\
     change is persistent but applies to newly started shells only, so open\n\
     a new shell after installing.\n\
     Unix installations never edit shell startup files.\n\
     Add the destination to your PATH yourself."
        .to_string()
}

/// The platform's default per-user destination for the installed binaries.
pub fn default_destination() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local) = non_blank_env("LOCALAPPDATA") {
            return Ok(PathBuf::from(local).join("leg").join("bin"));
        }
        if let Some(profile) = non_blank_env("USERPROFILE") {
            return Ok(PathBuf::from(profile)
                .join("AppData")
                .join("Local")
                .join("leg")
                .join("bin"));
        }
        Err(LegError::Install(
            "cannot determine the default install directory: neither LOCALAPPDATA nor \
             USERPROFILE is set; pass --dest <dir>"
                .to_string(),
        ))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = non_blank_env("XDG_BIN_HOME") {
            return Ok(PathBuf::from(xdg));
        }
        if let Some(home) = non_blank_env("HOME") {
            return Ok(PathBuf::from(home).join(".local").join("bin"));
        }
        Err(LegError::Install(
            "cannot determine the default install directory: neither XDG_BIN_HOME nor \
             HOME is set; pass --dest <dir>"
                .to_string(),
        ))
    }
}

/// Reads an environment variable, treating a blank value as unset.
fn non_blank_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Installs the native binaries into the destination the options select.
pub fn install(options: &InstallOptions) -> Result<InstallReport> {
    install_with(options, FaultPoint::None)
}

/// Installs the native binaries, failing at `fault` when one is injected.
pub(crate) fn install_with(options: &InstallOptions, fault: FaultPoint) -> Result<InstallReport> {
    let sources = discover_sources()?;
    install_sources(&sources, options, fault)
}

/// Installs the given sources into the destination the options select.
pub(crate) fn install_sources(
    sources: &[SourceBinary],
    options: &InstallOptions,
    fault: FaultPoint,
) -> Result<InstallReport> {
    let destination = match &options.destination {
        Some(destination) => destination.clone(),
        None => default_destination()?,
    };
    prepare_destination(&destination)?;

    // The current user's PATH is read before anything is written, so a
    // registry failure cannot leave a half-finished install behind.
    let path_update = PathUpdate::prepare(&destination, options.no_path)?;

    let commit = commit_files(sources, &destination, fault)?;
    let installed = commit.installed.clone();

    let Some(update) = path_update else {
        commit.finish();
        return Ok(InstallReport {
            destination,
            installed,
            path_updated: false,
            path_already_present: false,
        });
    };

    if let Err(error) = update.commit() {
        // The PATH write is the last mutating step; undo the file install so
        // the command never leaves a partial install behind.
        commit.rollback();
        return Err(error);
    }
    commit.finish();
    Ok(InstallReport {
        destination,
        installed,
        path_updated: update.updated(),
        path_already_present: update.already_present(),
    })
}

/// Discovers the native binaries that sit beside the running `leg` executable.
pub fn discover_sources() -> Result<Vec<SourceBinary>> {
    let current = std::env::current_exe().map_err(|error| {
        LegError::Install(format!("cannot locate the running leg executable: {error}"))
    })?;
    let directory = current.parent().ok_or_else(|| {
        LegError::Install(format!(
            "the running leg executable {} has no parent directory",
            current.display()
        ))
    })?;
    discover_sources_in(directory, &current)
}

/// Discovers the four native binaries in `directory`.
///
/// `running` stands in for the `leg` binary when the directory does not hold
/// a sibling copy of it.
pub fn discover_sources_in(directory: &Path, running: &Path) -> Result<Vec<SourceBinary>> {
    let suffix = executable_suffix();
    let mut sources = Vec::new();
    let mut missing = Vec::new();

    for name in COMPANION_BINARIES {
        let file_name = format!("{name}{suffix}");
        let path = directory.join(&file_name);
        if !path.is_file() {
            if name == "leg" && running.is_file() {
                sources.push(SourceBinary {
                    file_name,
                    path: running.to_path_buf(),
                });
                continue;
            }
            missing.push(file_name);
            continue;
        }
        if is_script(&path)? {
            return Err(LegError::Install(format!(
                "{} is a script, not a native binary; package-manager shims are never installed",
                path.display()
            )));
        }
        sources.push(SourceBinary { file_name, path });
    }

    if !missing.is_empty() {
        return Err(LegError::Install(format!(
            "missing companion binaries beside the running leg executable ({}): {}",
            directory.display(),
            missing.join(", ")
        )));
    }
    Ok(sources)
}

/// Whether `path` starts with a `#!` interpreter line, which marks a script.
fn is_script(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)
        .map_err(|error| LegError::Install(format!("cannot read {}: {error}", path.display())))?;
    let mut prefix = [0_u8; 2];
    let count = file
        .read(&mut prefix)
        .map_err(|error| LegError::Install(format!("cannot read {}: {error}", path.display())))?;
    Ok(count == prefix.len() && &prefix == b"#!")
}

/// Creates the destination directory and proves it is writable.
fn prepare_destination(destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).map_err(|error| {
        LegError::Install(format!(
            "cannot create the install directory {}: {error}",
            destination.display()
        ))
    })?;
    let probe = destination.join(format!(".leg-install-probe-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|error| {
        LegError::Install(format!(
            "the install directory {} is not writable: {error}",
            destination.display()
        ))
    })?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// A file that was committed to the destination during this run.
struct Committed {
    /// The installed file name.
    file_name: String,
    /// Whether an earlier file with that name was replaced.
    replaced: bool,
}

/// A completed file commit that can still be undone.
struct FileCommit {
    /// The directory the files were installed into.
    destination: PathBuf,
    /// The staging directory holding the replaced files.
    staging: PathBuf,
    /// The files this run committed.
    committed: Vec<Committed>,
    /// The installed file names, in installation order.
    installed: Vec<String>,
}

impl FileCommit {
    /// Drops the staging directory and the replaced files it kept.
    fn finish(self) {
        let _ = fs::remove_dir_all(&self.staging);
    }

    /// Restores the files the destination held before this run.
    fn rollback(&self) {
        for entry in self.committed.iter().rev() {
            let target = self.destination.join(&entry.file_name);
            let backup = self.staging.join("old").join(&entry.file_name);
            if entry.replaced {
                let _ = fs::rename(&backup, &target);
            } else {
                let _ = fs::remove_file(&target);
            }
        }
        let _ = fs::remove_dir_all(&self.staging);
    }
}

/// Copies the sources into the destination and swaps them into place.
///
/// Every source is staged inside the destination first, so a failure during
/// staging never touches the installed files. The commit then replaces the
/// installed files one by one and restores the previous files if any of the
/// replacements fails.
fn commit_files(
    sources: &[SourceBinary],
    destination: &Path,
    fault: FaultPoint,
) -> Result<FileCommit> {
    let staging = destination.join(format!(".leg-install-staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let staged_dir = staging.join("new");
    let backup_dir = staging.join("old");
    fs::create_dir_all(&staged_dir).map_err(|error| {
        LegError::Install(format!(
            "cannot create the staging directory {}: {error}",
            staged_dir.display()
        ))
    })?;
    fs::create_dir_all(&backup_dir).map_err(|error| {
        LegError::Install(format!(
            "cannot create the staging directory {}: {error}",
            backup_dir.display()
        ))
    })?;

    for source in sources {
        if fault == FaultPoint::Stage {
            let _ = fs::remove_dir_all(&staging);
            return Err(LegError::Install(
                "injected failure while staging the installed binaries".to_string(),
            ));
        }
        let staged = staged_dir.join(&source.file_name);
        fs::copy(&source.path, &staged).map_err(|error| {
            LegError::Install(format!(
                "cannot copy {} into the staging directory: {error}",
                source.path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).map_err(|error| {
                LegError::Install(format!(
                    "cannot mark {} executable: {error}",
                    staged.display()
                ))
            })?;
        }
    }

    let mut committed: Vec<Committed> = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        // The injected commit failure lands after the first file was swapped
        // in, so the rollback path is exercised with real state to restore.
        if fault == FaultPoint::Commit && index == 1 {
            let commit = FileCommit {
                destination: destination.to_path_buf(),
                staging,
                committed,
                installed: Vec::new(),
            };
            commit.rollback();
            return Err(LegError::Install(
                "injected failure while committing the installed binaries".to_string(),
            ));
        }
        let target = destination.join(&source.file_name);
        let backup = backup_dir.join(&source.file_name);
        let staged = staged_dir.join(&source.file_name);
        let replaced = target.exists();
        let backup_error = if replaced {
            fs::rename(&target, &backup).err()
        } else {
            None
        };
        if let Some(error) = backup_error {
            let commit = FileCommit {
                destination: destination.to_path_buf(),
                staging,
                committed,
                installed: Vec::new(),
            };
            commit.rollback();
            return Err(LegError::Install(format!(
                "cannot replace {}: {error}; close every running copy and retry",
                target.display()
            )));
        }
        if let Err(error) = fs::rename(&staged, &target) {
            if replaced {
                let _ = fs::rename(&backup, &target);
            }
            let commit = FileCommit {
                destination: destination.to_path_buf(),
                staging,
                committed,
                installed: Vec::new(),
            };
            commit.rollback();
            return Err(LegError::Install(format!(
                "cannot install {}: {error}",
                target.display()
            )));
        }
        committed.push(Committed {
            file_name: source.file_name.clone(),
            replaced,
        });
    }

    Ok(FileCommit {
        destination: destination.to_path_buf(),
        staging,
        committed,
        installed: sources
            .iter()
            .map(|source| source.file_name.clone())
            .collect(),
    })
}

/// The current-user PATH update an install run performs.
struct PathUpdate {
    /// The PATH value to write, empty when the entry is already present.
    value: String,
    /// Whether the destination was already present in the current user's PATH.
    already_present: bool,
    /// Whether the merged value differs from the original.
    changed: bool,
}

impl PathUpdate {
    /// Prepares the PATH update, or returns `None` when no update applies.
    fn prepare(destination: &Path, no_path: bool) -> Result<Option<Self>> {
        if no_path || !cfg!(windows) {
            return Ok(None);
        }
        let existing = read_user_path()?;
        let merged = merge_path_value(&existing, destination);
        Ok(Some(PathUpdate {
            value: merged.value,
            already_present: merged.already_present,
            changed: merged.changed,
        }))
    }

    /// Whether this update changes the current user's PATH.
    fn updated(&self) -> bool {
        self.changed
    }

    /// Whether the destination was already registered in PATH.
    fn already_present(&self) -> bool {
        self.already_present
    }

    /// Writes the new PATH value and tells Windows about the change.
    fn commit(&self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }
        write_user_path(&self.value)?;
        broadcast_environment_change();
        Ok(())
    }
}

/// The result of merging a destination into a PATH value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPath {
    /// The merged PATH value.
    pub value: String,
    /// Whether the destination was already present.
    pub already_present: bool,
    /// Whether the merged value differs from the original.
    pub changed: bool,
}

/// Merges `entry` into a Windows PATH value.
///
/// The comparison is case-insensitive, existing entries are preserved, and the
/// destination is appended when it is missing.
pub fn merge_path_value(existing: &str, entry: &Path) -> MergedPath {
    let wanted = normalize_path_entry(entry);
    let mut entries: Vec<String> = Vec::new();
    let mut already_present = false;
    for part in existing.split(';') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if normalize_path_entry(Path::new(trimmed)).eq_ignore_ascii_case(&wanted) {
            if already_present {
                // A repeated entry is dropped; the first one is kept.
                continue;
            }
            already_present = true;
        }
        entries.push(trimmed.to_string());
    }
    if !already_present {
        entries.push(wanted);
    }
    let value = entries.join(";");
    let changed = value != existing;
    MergedPath {
        value,
        already_present,
        changed,
    }
}

/// Normalizes a PATH entry for comparison: no trailing separator, no quotes.
fn normalize_path_entry(entry: &Path) -> String {
    let raw = entry.to_string_lossy().into_owned();
    raw.trim()
        .trim_matches('"')
        .trim_end_matches(['\\', '/'])
        .to_string()
}

#[cfg(windows)]
mod windows_path {
    //! The current-user PATH backend. It reads and writes the `Path` value
    //! under `HKEY_CURRENT_USER\Environment` and never touches the
    //! machine-wide PATH.

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    #[cfg(test)]
    use windows_sys::Win32::System::Registry::RegCreateKeyW;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    use crate::error::{LegError, Result};

    /// The subkey under `HKEY_CURRENT_USER` that holds the user environment.
    const ENVIRONMENT_KEY: &str = "Environment";

    /// The name of the PATH value inside the environment key.
    const PATH_VALUE: &str = "Path";

    /// Encodes a string as a null-terminated UTF-16 string.
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Opens a key under `HKEY_CURRENT_USER` with the requested access.
    fn open_key(name: &str, access: u32) -> Result<HKEY> {
        let wide_name = wide(name);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: the key name is a null-terminated UTF-16 string that outlives
        // the call, and the output handle is written by the call itself.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide_name.as_ptr(),
                0,
                access,
                &mut key as *mut HKEY,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(LegError::Install(format!(
                "cannot open the current user's environment (registry error {status})"
            )));
        }
        Ok(key)
    }

    /// Closes a registry key.
    fn close_key(key: HKEY) {
        // SAFETY: the key was opened by `open_key` and is not used afterwards.
        unsafe {
            RegCloseKey(key);
        }
    }

    /// Reads the PATH value of the key under `HKEY_CURRENT_USER`.
    pub(super) fn read_path_at(key_name: &str) -> Result<String> {
        let key = open_key(key_name, KEY_READ)?;
        let value_name = wide(PATH_VALUE);
        let mut value_type: u32 = 0;
        let mut size: u32 = 0;
        // SAFETY: the value name is a null-terminated UTF-16 string, and the
        // size query writes no data.
        let status = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type as *mut u32,
                std::ptr::null_mut(),
                &mut size as *mut u32,
            )
        };
        if status != ERROR_SUCCESS {
            close_key(key);
            return Err(LegError::Install(format!(
                "cannot read the current user's PATH (registry error {status})"
            )));
        }
        if value_type != REG_EXPAND_SZ && value_type != REG_SZ {
            close_key(key);
            return Err(LegError::Install(format!(
                "the current user's PATH is not a string value (registry type {value_type})"
            )));
        }
        let mut buffer = vec![0_u8; size as usize];
        let mut written = size;
        // SAFETY: the buffer is `size` bytes long and the call writes at most
        // that many bytes into it.
        let status = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut value_type as *mut u32,
                buffer.as_mut_ptr(),
                &mut written as *mut u32,
            )
        };
        close_key(key);
        if status != ERROR_SUCCESS {
            return Err(LegError::Install(format!(
                "cannot read the current user's PATH (registry error {status})"
            )));
        }
        let text = decode_registry_string(&buffer[..written as usize]);
        Ok(text)
    }

    /// Decodes a registry string value, which is stored as UTF-16.
    fn decode_registry_string(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let text = String::from_utf16_lossy(&units);
        text.trim_end_matches('\0').to_string()
    }

    /// Writes the PATH value of the key under `HKEY_CURRENT_USER`.
    pub(super) fn write_path_at(key_name: &str, value: &str) -> Result<()> {
        let key = open_key(key_name, KEY_SET_VALUE)?;
        let value_name = wide(PATH_VALUE);
        let data = wide(value);
        let byte_len = (data.len() * std::mem::size_of::<u16>()) as u32;
        // SAFETY: the value name and the data are null-terminated UTF-16
        // strings that outlive the call, and the byte length matches the data.
        let status = unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_EXPAND_SZ,
                data.as_ptr().cast::<u8>(),
                byte_len,
            )
        };
        close_key(key);
        if status != ERROR_SUCCESS {
            return Err(LegError::Install(format!(
                "cannot write the current user's PATH (registry error {status})"
            )));
        }
        Ok(())
    }

    /// Reads the current user's PATH value.
    pub(super) fn read_user_path() -> Result<String> {
        read_path_at(ENVIRONMENT_KEY)
    }

    /// Writes the current user's PATH value.
    pub(super) fn write_user_path(value: &str) -> Result<()> {
        write_path_at(ENVIRONMENT_KEY, value)
    }

    /// Creates a key under `HKEY_CURRENT_USER`, used by the registry tests to
    /// exercise the backend without touching the real environment.
    #[cfg(test)]
    pub(super) fn create_key(name: &str) -> Result<()> {
        let wide_name = wide(name);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: the key name is a null-terminated UTF-16 string that
        // outlives the call, and the output handle is written by the call.
        let status =
            unsafe { RegCreateKeyW(HKEY_CURRENT_USER, wide_name.as_ptr(), &mut key as *mut HKEY) };
        if status != ERROR_SUCCESS {
            return Err(LegError::Install(format!(
                "cannot create the registry key {name} (registry error {status})"
            )));
        }
        close_key(key);
        Ok(())
    }

    /// Tells Windows that the user environment changed, so newly started
    /// shells pick the new PATH up.
    pub(super) fn broadcast_environment_change() {
        let name = wide("Environment");
        let mut result: usize = 0;
        // SAFETY: the message targets the broadcast window with a null
        // terminated string that outlives the call.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                name.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result as *mut usize,
            );
        }
    }
}

#[cfg(windows)]
use windows_path::{broadcast_environment_change, read_user_path, write_user_path};

#[cfg(not(windows))]
fn read_user_path() -> Result<String> {
    Ok(String::new())
}

#[cfg(not(windows))]
fn write_user_path(_value: &str) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn broadcast_environment_change() {}

#[cfg(test)]
mod tests;
