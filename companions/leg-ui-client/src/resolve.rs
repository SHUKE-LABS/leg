use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CHECK_OUTPUT: usize = 1024 * 1024;
const MAX_NPM_LAUNCHER_BYTES: u64 = 256 * 1024;

/// The executable extensions Windows searches when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";

/// The file extensions that mark a wrapper script rather than a native binary.
const SCRIPT_EXTENSIONS: [&str; 15] = [
    "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "mjs", "cjs", "sh", "bash", "zsh", "fish",
    "py", "rb",
];

#[derive(Clone, Debug)]
pub(crate) struct ResolvedLeg {
    pub(crate) path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("could not find leg on PATH; install leg or set --leg-bin to its native executable")]
    NotFound,
    #[error("could not inspect leg executable {path:?}: {source}")]
    Inspect { path: PathBuf, source: io::Error },
    #[error(
        "--leg-bin must name a native leg binary or the published @shukelabs/leg npm launcher; {0}"
    )]
    UnsupportedWrapper(String),
    #[error(
        "could not resolve the installed npm platform binary: {0}. Install the matching @shukelabs/leg platform package or select a native executable with --leg-bin"
    )]
    NpmResolution(String),
    #[error("leg binary {path:?} did not identify itself as leg (version output: {detail})")]
    NotLeg { path: PathBuf, detail: String },
    #[error(
        "leg binary {path:?} lacks required exchange features (--stream-json, --new-session-id); install a version that provides leg.exchange.stream/v1"
    )]
    MissingStreamCapability { path: PathBuf },
    #[error("could not run leg binary {path:?}: {message}")]
    Run { path: PathBuf, message: String },
}

/// What a candidate file on PATH turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    /// A native executable.
    Native,
    /// The published `@shukelabs/leg` npm launcher.
    PublishedLauncher,
    /// A package-manager or shell script wrapper.
    ScriptWrapper,
}

/// The candidate the default PATH search selected.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selected {
    /// A native executable.
    Native(PathBuf),
    /// The published npm launcher.
    Launcher(PathBuf),
}

/// The PATH lookup inputs. The tests inject their own PATH value and
/// extension list instead of reading the process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchPath {
    /// The PATH value, split into directories when the lookup runs.
    value: Option<OsString>,
    /// The executable extensions to try after the bare file name.
    extensions: Vec<String>,
}

impl SearchPath {
    /// The PATH lookup of the running process.
    fn from_environment() -> Self {
        Self {
            value: std::env::var_os("PATH"),
            extensions: executable_extensions(),
        }
    }

    /// The candidate files for `name`, in search order.
    fn candidates(&self, name: &str) -> Vec<PathBuf> {
        let Some(value) = &self.value else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for directory in std::env::split_paths(value) {
            for file_name in self.file_names(name) {
                let candidate = directory.join(&file_name);
                if candidate.is_file() {
                    candidates.push(candidate);
                }
            }
        }
        candidates
    }

    /// The file names a lookup of `name` tries, in order.
    fn file_names(&self, name: &str) -> Vec<String> {
        let mut names = vec![name.to_string()];
        for extension in &self.extensions {
            names.push(format!("{name}{extension}"));
        }
        names
    }
}

/// The executable extensions to try, taken from `PATHEXT` on Windows.
fn executable_extensions() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut extensions = parse_extensions(std::env::var_os("PATHEXT").as_deref());
        // A native binary and the published launcher stay reachable even when
        // `PATHEXT` was edited: the native one is `leg.exe`, and the published
        // npm launcher is `leg.js`.
        for required in [".exe", ".js"] {
            if !extensions.iter().any(|extension| extension == required) {
                extensions.push(required.to_string());
            }
        }
        extensions
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Parses a `PATHEXT` value into lowercase extensions.
#[cfg(any(windows, test))]
fn parse_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    let raw = match pathext {
        Some(value) => value.to_string_lossy().into_owned(),
        None => DEFAULT_PATHEXT.to_string(),
    };
    let mut extensions = Vec::new();
    for part in raw.split(';') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = if trimmed.starts_with('.') {
            trimmed.to_ascii_lowercase()
        } else {
            format!(".{}", trimmed.to_ascii_lowercase())
        };
        if !extensions.contains(&normalized) {
            extensions.push(normalized);
        }
    }
    extensions
}

pub(crate) fn resolve_leg(override_path: Option<&Path>) -> Result<ResolvedLeg, ResolveError> {
    resolve_leg_in(override_path, &SearchPath::from_environment())
}

/// Resolves the leg binary, either the explicit override or the first usable
/// candidate on PATH.
fn resolve_leg_in(
    override_path: Option<&Path>,
    search: &SearchPath,
) -> Result<ResolvedLeg, ResolveError> {
    let path = match override_path {
        Some(path) => {
            let canonical = canonical_path(path)?;
            match classify(&canonical)? {
                CandidateKind::PublishedLauncher => resolve_npm_binary(&canonical, search)?,
                CandidateKind::ScriptWrapper => {
                    return Err(ResolveError::UnsupportedWrapper(format!(
                        "{} is a script wrapper. Select a native leg binary, or use the published @shukelabs/leg launcher",
                        canonical.display()
                    )));
                }
                CandidateKind::Native => canonical,
            }
        }
        None => match select_candidate(search)? {
            Selected::Native(path) => path,
            Selected::Launcher(path) => resolve_npm_binary(&path, search)?,
        },
    };

    check_executable(&path)?;
    let version_output =
        run_bounded(&path, &[OsStr::new("--version")]).map_err(|message| ResolveError::Run {
            path: path.clone(),
            message,
        })?;
    let version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_string();
    if !version.starts_with("leg ") {
        return Err(ResolveError::NotLeg {
            path,
            detail: bounded_detail(&version),
        });
    }

    let help =
        run_bounded(&path, &[OsStr::new("--help")]).map_err(|message| ResolveError::Run {
            path: path.clone(),
            message,
        })?;
    let help = String::from_utf8_lossy(&help.stdout);
    if !help.contains("--stream-json") || !help.contains("--new-session-id") {
        return Err(ResolveError::MissingStreamCapability { path });
    }

    Ok(ResolvedLeg { path })
}

/// Resolves a path to its canonical form.
///
/// `fs::canonicalize` returns a verbatim `\\?\` path on Windows. Node.js
/// cannot `require` such a path, so the prefix is removed and the plain
/// path is used from then on.
fn canonical_path(path: &Path) -> Result<PathBuf, ResolveError> {
    let canonical = fs::canonicalize(path).map_err(|source| ResolveError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(without_verbatim_prefix(canonical))
}

/// Removes the Windows verbatim path prefix from a canonical path.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let Some(text) = path.to_str() else {
            return path;
        };
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Finds the first usable `leg` on PATH, skipping script wrappers.
///
/// A package-manager shim that comes first on PATH is not an error: the
/// search continues until it reaches a native binary or the published npm
/// launcher.
fn select_candidate(search: &SearchPath) -> Result<Selected, ResolveError> {
    for candidate in search.candidates("leg") {
        let kind = classify(&candidate)?;
        if kind == CandidateKind::ScriptWrapper {
            continue;
        }
        let canonical = canonical_path(&candidate)?;
        return Ok(match kind {
            CandidateKind::PublishedLauncher => Selected::Launcher(canonical),
            _ => Selected::Native(canonical),
        });
    }
    Err(ResolveError::NotFound)
}

/// Classifies a candidate file as a native binary, the published npm
/// launcher, or a script wrapper.
fn classify(path: &Path) -> Result<CandidateKind, ResolveError> {
    let mut file = fs::File::open(path).map_err(|source| ResolveError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    let mut prefix = [0_u8; 2];
    let count = file
        .read(&mut prefix)
        .map_err(|source| ResolveError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    if count == prefix.len() && &prefix == b"#!" {
        return Ok(if is_published_launcher(path)? {
            CandidateKind::PublishedLauncher
        } else {
            CandidateKind::ScriptWrapper
        });
    }
    Ok(if is_script_extension(path) {
        CandidateKind::ScriptWrapper
    } else {
        CandidateKind::Native
    })
}

/// Whether the file is the published `@shukelabs/leg` npm launcher.
fn is_published_launcher(path: &Path) -> Result<bool, ResolveError> {
    let file = fs::File::open(path).map_err(|source| ResolveError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    let length = file
        .metadata()
        .map_err(|source| ResolveError::Inspect {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > MAX_NPM_LAUNCHER_BYTES {
        return Ok(false);
    }
    let mut content = Vec::new();
    file.take(MAX_NPM_LAUNCHER_BYTES)
        .read_to_end(&mut content)
        .map_err(|source| ResolveError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    let content = String::from_utf8_lossy(&content);
    Ok(content.contains("const platformPackages = {")
        && content.contains("function resolvePlatformBinary(")
        && content.contains("module.exports = { platformPackages, resolvePlatformBinary }")
        && content.contains("require.resolve(`${packageName}/bin/${binaryName}`)"))
}

/// Whether the file name carries a script extension.
fn is_script_extension(path: &Path) -> bool {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => {
            let extension = extension.to_ascii_lowercase();
            SCRIPT_EXTENSIONS.contains(&extension.as_str())
        }
        None => false,
    }
}

fn check_executable(path: &Path) -> Result<(), ResolveError> {
    let metadata = fs::metadata(path).map_err(|source| ResolveError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ResolveError::UnsupportedWrapper(format!(
            "{} is not a file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ResolveError::UnsupportedWrapper(format!(
                "{} is not executable",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Finds the Node.js executable on PATH.
///
/// The lookup honours the executable extensions, so a Windows installation
/// that only exposes `node.exe` is found.
fn find_node(search: &SearchPath) -> Result<PathBuf, ResolveError> {
    for candidate in search.candidates("node") {
        if classify(&candidate)? == CandidateKind::Native {
            return Ok(candidate);
        }
    }
    Err(ResolveError::NotFound)
}

fn resolve_npm_binary(launcher: &Path, search: &SearchPath) -> Result<PathBuf, ResolveError> {
    let node = find_node(search).map_err(|_| {
        ResolveError::NpmResolution(
            "Node.js is required to resolve the published launcher".to_string(),
        )
    })?;
    let script = r#"const m=require(process.argv[1]);if(!m.platformPackages||typeof m.resolvePlatformBinary!=='function'){process.stderr.write('unrecognized launcher');process.exit(4)}try{process.stdout.write(m.resolvePlatformBinary().binaryPath)}catch(e){process.stderr.write(e.message);process.exit(2)}"#;
    let output = run_node(&node, launcher, script).map_err(ResolveError::NpmResolution)?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ResolveError::NpmResolution(if message.is_empty() {
            format!("Node resolver exited with {}", output.status)
        } else {
            message
        }));
    }
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if resolved.is_empty() {
        return Err(ResolveError::NpmResolution(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let path = PathBuf::from(resolved);
    let canonical = fs::canonicalize(&path).map_err(|error| {
        ResolveError::NpmResolution(format!(
            "platform package binary {} cannot be resolved: {error}",
            path.display()
        ))
    })?;
    Ok(without_verbatim_prefix(canonical))
}

fn run_node(node: &Path, launcher: &Path, script: &str) -> Result<BoundedOutput, String> {
    let mut command = Command::new(node);
    command.arg("-e").arg(script).arg(launcher).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    add_system_environment(&mut command);
    run_command_bounded(command)
}

/// Adds the system variables a child process needs to start on this platform.
///
/// The children run with a cleared environment so credentials never reach
/// them, but a Windows process cannot start without `SystemRoot`: Node.js
/// aborts in its crypto initialisation when the variable is missing.
fn add_system_environment(command: &mut Command) {
    #[cfg(windows)]
    for name in [
        "ALLUSERSPROFILE",
        "APPDATA",
        "COMSPEC",
        "HOMEDRIVE",
        "HOMEPATH",
        "LOCALAPPDATA",
        "NUMBER_OF_PROCESSORS",
        "OS",
        "PATHEXT",
        "PROCESSOR_ARCHITECTURE",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PUBLIC",
        "SystemDrive",
        "SystemRoot",
        "TEMP",
        "TMP",
        "USERDOMAIN",
        "USERNAME",
        "USERPROFILE",
        "windir",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn run_bounded(path: &Path, args: &[&OsStr]) -> Result<BoundedOutput, String> {
    let mut command = Command::new(path);
    command.args(args).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let output = run_command_bounded(command)?;
    if !output.status.success() {
        return Err(format!("exited with {}", output.status));
    }
    Ok(output)
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command_bounded(mut command: Command) -> Result<BoundedOutput, String> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || read_limited(stdout));
    let stderr_reader = thread::spawn(move || read_limited(stderr));
    let status = match wait_timeout(&mut child, CHECK_TIMEOUT) {
        Ok(status) => status,
        Err(error) => {
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error);
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked".to_string())?
        .map_err(|error| error.to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked".to_string())?
        .map_err(|error| error.to_string())?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("capability check timed out after 3 seconds".to_string());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    }
}

fn read_limited(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(kept);
        }
        let remaining = MAX_CHECK_OUTPUT.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..count.min(remaining)]);
    }
}

fn bounded_detail(value: &str) -> String {
    let detail: String = value.chars().take(160).collect();
    if detail.is_empty() {
        "empty output".to_string()
    } else {
        detail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// The published launcher shipped by this repository.
    fn published_launcher() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("packaging")
            .join("npm")
            .join("leg.js")
    }

    /// The canonical path of a file, without the Windows verbatim prefix.
    fn plain(path: &Path) -> PathBuf {
        without_verbatim_prefix(fs::canonicalize(path).expect("canonical path"))
    }

    /// A PATH value that lists `directories` in order.
    fn search(directories: &[PathBuf], extensions: &[&str]) -> SearchPath {
        let value = std::env::join_paths(directories).expect("join the search path");
        SearchPath {
            value: Some(value),
            extensions: extensions.iter().map(|value| value.to_string()).collect(),
        }
    }

    #[test]
    #[cfg(windows)]
    fn the_verbatim_prefix_is_removed_from_a_canonical_path() {
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"\\?\C:\tools\leg.exe")),
            PathBuf::from(r"C:\tools\leg.exe")
        );
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\leg.exe")),
            PathBuf::from(r"\\server\share\leg.exe")
        );
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(r"C:\tools\leg.exe")),
            PathBuf::from(r"C:\tools\leg.exe")
        );
    }

    #[test]
    #[cfg(windows)]
    fn the_published_launcher_resolves_through_node() {
        let root = tempfile::tempdir().expect("scratch directory");
        let package = root
            .path()
            .join("node_modules")
            .join("@shukelabs")
            .join("leg");
        fs::create_dir_all(&package).expect("create the package directory");
        fs::copy(published_launcher(), package.join("leg.js")).expect("copy the launcher");
        let platform_binary = root
            .path()
            .join("node_modules")
            .join("@shukelabs")
            .join("leg-win32-x64")
            .join("bin")
            .join("leg.exe");
        fs::create_dir_all(platform_binary.parent().expect("the bin directory"))
            .expect("create the platform package");
        fs::write(&platform_binary, b"MZ binary").expect("write the platform binary");

        let Some(node_directory) = std::env::var_os("PATH").and_then(|value| {
            std::env::split_paths(&value).find(|entry| entry.join("node.exe").is_file())
        }) else {
            eprintln!("skipping the published launcher check: Node.js is not installed");
            return;
        };
        let search = search(&[package.clone(), node_directory], &[".exe", ".js"]);

        let resolved = resolve_npm_binary(&package.join("leg.js"), &search)
            .expect("the published launcher resolves");
        assert_eq!(resolved, plain(&platform_binary));
    }

    #[test]
    fn parse_extensions_reads_the_pathext_value() {
        let extensions = parse_extensions(Some(OsStr::new(".EXE;.CMD;.EXE")));
        assert_eq!(extensions, vec![".exe", ".cmd"]);
    }

    #[test]
    fn parse_extensions_falls_back_to_the_windows_default() {
        let extensions = parse_extensions(None);
        assert!(extensions.contains(&".exe".to_string()));
        assert!(extensions.contains(&".cmd".to_string()));
    }

    #[test]
    fn parse_extensions_skips_blank_entries() {
        let extensions = parse_extensions(Some(OsStr::new(".EXE;;.CMD;")));
        assert_eq!(extensions, vec![".exe", ".cmd"]);
    }

    #[test]
    fn candidates_try_the_bare_name_and_every_extension() {
        let directory = tempfile::tempdir().expect("scratch directory");
        fs::write(directory.path().join("leg.exe"), b"binary").expect("write the candidate");
        let search = search(&[directory.path().to_path_buf()], &[".exe"]);

        let candidates = search.candidates("leg");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], directory.path().join("leg.exe"));
    }

    #[test]
    fn candidates_skip_names_that_do_not_exist() {
        let directory = tempfile::tempdir().expect("scratch directory");
        fs::write(directory.path().join("leg.cmd"), b"shim").expect("write the candidate");
        let search = search(&[directory.path().to_path_buf()], &[".exe", ".cmd"]);

        let candidates = search.candidates("leg");
        assert_eq!(candidates, vec![directory.path().join("leg.cmd")]);
    }

    #[test]
    fn classify_recognises_a_native_binary() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let path = directory.path().join("leg.exe");
        fs::write(&path, b"MZ binary").expect("write the candidate");

        assert_eq!(classify(&path).expect("classify"), CandidateKind::Native);
    }

    #[test]
    fn classify_recognises_a_script_wrapper() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let path = directory.path().join("leg");
        fs::write(&path, b"#!/bin/sh\nexec leg \"$@\"\n").expect("write the candidate");

        assert_eq!(
            classify(&path).expect("classify"),
            CandidateKind::ScriptWrapper
        );
    }

    #[test]
    fn classify_recognises_a_windows_command_wrapper() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let path = directory.path().join("leg.cmd");
        fs::write(&path, b"@ECHO off\r\n").expect("write the candidate");

        assert_eq!(
            classify(&path).expect("classify"),
            CandidateKind::ScriptWrapper
        );
    }

    #[test]
    fn classify_recognises_the_published_npm_launcher() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let path = directory.path().join("leg.js");
        fs::copy(published_launcher(), &path).expect("copy the published launcher");

        assert_eq!(
            classify(&path).expect("classify"),
            CandidateKind::PublishedLauncher
        );
    }

    #[test]
    fn select_candidate_skips_a_shim_and_picks_the_native_binary() {
        let shim_dir = tempfile::tempdir().expect("shim directory");
        let native_dir = tempfile::tempdir().expect("native directory");
        fs::write(shim_dir.path().join("leg"), b"#!/bin/sh\nexec leg \"$@\"\n")
            .expect("write the shim");
        fs::write(native_dir.path().join("leg"), b"native").expect("write the native binary");
        let search = search(
            &[
                shim_dir.path().to_path_buf(),
                native_dir.path().to_path_buf(),
            ],
            &[],
        );

        let selected = select_candidate(&search).expect("a candidate is selected");
        assert_eq!(
            selected,
            Selected::Native(plain(&native_dir.path().join("leg")))
        );
    }

    #[test]
    fn select_candidate_skips_a_windows_command_wrapper() {
        let shim_dir = tempfile::tempdir().expect("shim directory");
        let native_dir = tempfile::tempdir().expect("native directory");
        fs::write(shim_dir.path().join("leg.cmd"), b"@ECHO off\r\n").expect("write the shim");
        fs::write(native_dir.path().join("leg.exe"), b"native").expect("write the native binary");
        let search = search(
            &[
                shim_dir.path().to_path_buf(),
                native_dir.path().to_path_buf(),
            ],
            &[".exe", ".cmd"],
        );

        let selected = select_candidate(&search).expect("a candidate is selected");
        assert_eq!(
            selected,
            Selected::Native(plain(&native_dir.path().join("leg.exe")))
        );
    }

    #[test]
    fn select_candidate_picks_the_published_launcher() {
        let directory = tempfile::tempdir().expect("launcher directory");
        let path = directory.path().join("leg.js");
        fs::copy(published_launcher(), &path).expect("copy the published launcher");
        let search = search(&[directory.path().to_path_buf()], &[".js"]);

        let selected = select_candidate(&search).expect("a candidate is selected");
        assert_eq!(selected, Selected::Launcher(plain(&path)));
    }

    #[test]
    fn select_candidate_reports_a_missing_binary() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let search = search(&[directory.path().to_path_buf()], &[".exe"]);

        let error = select_candidate(&search).expect_err("no candidate exists");
        assert!(matches!(error, ResolveError::NotFound));
    }

    #[test]
    fn find_node_honours_the_executable_extensions() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let path = directory.path().join("node.exe");
        fs::write(&path, b"binary").expect("write the candidate");
        let search = search(&[directory.path().to_path_buf()], &[".exe"]);

        assert_eq!(find_node(&search).expect("node is found"), path);
    }

    #[test]
    fn find_node_skips_a_command_wrapper() {
        let shim_dir = tempfile::tempdir().expect("shim directory");
        let node_dir = tempfile::tempdir().expect("node directory");
        fs::write(shim_dir.path().join("node.cmd"), b"@ECHO off\r\n").expect("write the shim");
        fs::write(node_dir.path().join("node.exe"), b"binary").expect("write the candidate");
        let search = search(
            &[shim_dir.path().to_path_buf(), node_dir.path().to_path_buf()],
            &[".exe", ".cmd"],
        );

        assert_eq!(
            find_node(&search).expect("node is found"),
            node_dir.path().join("node.exe")
        );
    }

    #[test]
    fn an_explicit_wrapper_path_is_rejected() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let path = directory.path().join("leg");
        fs::write(&path, b"#!/bin/sh\nexec leg \"$@\"\n").expect("write the wrapper");
        let search = search(&[directory.path().to_path_buf()], &[]);

        let error = resolve_leg_in(Some(&path), &search).expect_err("a wrapper is rejected");
        assert!(matches!(error, ResolveError::UnsupportedWrapper(_)));
    }
}
