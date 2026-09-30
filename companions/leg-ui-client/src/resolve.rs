use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CHECK_OUTPUT: usize = 1024 * 1024;

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
        "leg binary {path:?} does not support --stream-json; install a version that provides leg.exchange.stream/v1"
    )]
    MissingStreamCapability { path: PathBuf },
    #[error("could not run leg binary {path:?}: {message}")]
    Run { path: PathBuf, message: String },
}

pub(crate) fn resolve_leg(override_path: Option<&Path>) -> Result<ResolvedLeg, ResolveError> {
    let candidate = match override_path {
        Some(path) => path.to_path_buf(),
        None => find_on_path(OsStr::new("leg"))?,
    };
    let canonical = fs::canonicalize(&candidate).map_err(|source| ResolveError::Inspect {
        path: candidate.clone(),
        source,
    })?;

    let path = if is_published_npm_launcher(&canonical)? {
        resolve_npm_binary(&canonical)?
    } else {
        if is_script_wrapper(&canonical)? {
            return Err(ResolveError::UnsupportedWrapper(format!(
                "{} is a script wrapper. Select a native leg binary, or use the published @shukelabs/leg launcher",
                canonical.display()
            )));
        }
        canonical
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
    if !help.contains("--stream-json") {
        return Err(ResolveError::MissingStreamCapability { path });
    }

    Ok(ResolvedLeg { path })
}

fn find_on_path(name: &OsStr) -> Result<PathBuf, ResolveError> {
    let Some(path) = std::env::var_os("PATH") else {
        return Err(ResolveError::NotFound);
    };
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(ResolveError::NotFound)
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

fn is_published_npm_launcher(path: &Path) -> Result<bool, ResolveError> {
    let content = fs::read(path).map_err(|source| ResolveError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    let content = String::from_utf8_lossy(&content);
    Ok(content.contains("const platformPackages = {")
        && content.contains("function resolvePlatformBinary(")
        && content.contains("module.exports = { platformPackages, resolvePlatformBinary }")
        && content.contains("require.resolve(`${packageName}/bin/${binaryName}`)"))
}

fn is_script_wrapper(path: &Path) -> Result<bool, ResolveError> {
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
    Ok(count >= 2 && &prefix == b"#!")
}

fn resolve_npm_binary(launcher: &Path) -> Result<PathBuf, ResolveError> {
    let node = find_on_path(OsStr::new("node")).map_err(|_| {
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
    fs::canonicalize(&path).map_err(|error| {
        ResolveError::NpmResolution(format!(
            "platform package binary {} cannot be resolved: {error}",
            path.display()
        ))
    })
}

fn run_node(node: &Path, launcher: &Path, script: &str) -> Result<BoundedOutput, String> {
    let mut command = Command::new(node);
    command.arg("-e").arg(script).arg(launcher).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    run_command_bounded(command)
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
