//! The synchronous `bash` tool: run one bounded command in the caller's cwd.

use std::ffi::OsString;
use std::time::Duration;

use super::ToolHandler;
use crate::config::DEFAULT_BASH_TIMEOUT_SECS;
use crate::model::ToolSpec;

mod process;

const DESCRIPTION: &str = "Run a shell command with Bash in the current working directory. \
The command runs as the current OS user. Leg's provider credential variables are removed from \
the command environment; login-shell startup files may re-export them. Each output stream is \
capped at 2000 lines or 50 KB.";

/// The `bash` tool handler.
pub struct BashTool {
    shell: OsString,
    default_timeout_secs: u64,
    #[cfg(test)]
    env: Vec<(OsString, OsString)>,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            shell: OsString::from("bash"),
            default_timeout_secs: DEFAULT_BASH_TIMEOUT_SECS,
            #[cfg(test)]
            env: Vec::new(),
        }
    }
}

impl BashTool {
    /// Creates a handler that looks up `bash` on `PATH` for each call.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a handler with the configured default command timeout.
    pub fn with_default_timeout_secs(default_timeout_secs: u64) -> Self {
        Self {
            default_timeout_secs,
            ..Self::default()
        }
    }

    /// The `bash` declaration advertised to the model.
    pub fn spec() -> ToolSpec {
        Self::spec_with_default_timeout_secs(DEFAULT_BASH_TIMEOUT_SECS)
    }

    /// The `bash` declaration with a configured default command timeout.
    pub fn spec_with_default_timeout_secs(default_timeout_secs: u64) -> ToolSpec {
        let timeout_unit = if default_timeout_secs == 1 {
            "second"
        } else {
            "seconds"
        };
        let description = format!(
            "{DESCRIPTION} Commands time out after {default_timeout_secs} {timeout_unit} by default; set timeout to change the limit."
        );
        let timeout_description =
            format!("Maximum run time in seconds (default {default_timeout_secs})");
        ToolSpec::new(
            "bash",
            description,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to run with bash -lc"
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional short explanation of the command"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 0,
                        "default": default_timeout_secs,
                        "description": timeout_description
                    }
                },
                "required": ["command"]
            }),
        )
    }

    #[cfg(test)]
    fn with_shell(shell: impl Into<OsString>) -> Self {
        Self {
            shell: shell.into(),
            default_timeout_secs: DEFAULT_BASH_TIMEOUT_SECS,
            #[cfg(test)]
            env: Vec::new(),
        }
    }

    #[cfg(test)]
    fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    fn process_env(&self) -> &[(OsString, OsString)] {
        #[cfg(test)]
        {
            &self.env
        }
        #[cfg(not(test))]
        {
            &[]
        }
    }
}

impl ToolHandler for BashTool {
    fn call(&self, input: &serde_json::Value) -> Result<String, String> {
        let command = input["command"]
            .as_str()
            .ok_or("bash: `command` must be a string")?;
        if let Some(description) = input.get("description")
            && !description.is_string()
        {
            return Err("bash: `description` must be a string".to_string());
        }
        let timeout_secs = match input.get("timeout") {
            None => self.default_timeout_secs,
            Some(timeout) => timeout
                .as_u64()
                .ok_or("bash: `timeout` must be a non-negative integer")?,
        };

        let output = process::run(
            &self.shell,
            command,
            Duration::from_secs(timeout_secs),
            self.process_env(),
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("bash: executable was not found: {error}")
            } else {
                format!("bash: failed to start command: {error}")
            }
        })?;

        Ok(serde_json::json!({
            "wall_time_seconds": output.wall_time_seconds,
            "status": output.status,
            "exit_code": output.exit_code,
            "stdout": output.stdout.text,
            "stderr": output.stderr.text,
            "stdout_omitted_bytes": output.stdout.omitted_bytes,
            "stderr_omitted_bytes": output.stderr.omitted_bytes
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;

    fn call(command: &str, extra: Value) -> Result<Value, String> {
        call_with_tool(BashTool::new(), command, extra)
    }

    fn call_with_default_timeout(
        default_timeout_secs: u64,
        command: &str,
        extra: Value,
    ) -> Result<Value, String> {
        call_with_tool(
            BashTool::with_default_timeout_secs(default_timeout_secs),
            command,
            extra,
        )
    }

    fn call_with_tool(tool: BashTool, command: &str, extra: Value) -> Result<Value, String> {
        let mut input = serde_json::json!({"command": command});
        if let Some(fields) = extra.as_object() {
            for (key, value) in fields {
                input[key] = value.clone();
            }
        }
        let home = temp_dir("home");
        let tool = tool.with_env("HOME", home.as_os_str().to_os_string());
        let result = tool
            .call(&input)
            .map(|output| serde_json::from_str(&output).unwrap());
        std::fs::remove_dir_all(home).unwrap();
        result
    }

    fn bash_available() -> bool {
        let mut probe = Command::new("bash");
        // Resolve `bash` the way `process::run` does (see its Windows note).
        #[cfg(windows)]
        if let Some(path) = std::env::var_os("PATH") {
            probe.env("PATH", path);
        }
        probe
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn login_startup_budget_secs() -> u64 {
        if cfg!(windows) {
            let output = call("true", serde_json::json!({"timeout": 60})).unwrap();
            assert_eq!(output["status"], "exited");
            output["wall_time_seconds"].as_f64().unwrap().ceil() as u64
        } else {
            0
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "leg-bash-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn shell_path(path: &std::path::Path) -> String {
        let value = path.to_string_lossy();
        #[cfg(windows)]
        {
            let normalized = value.replace('\\', "/");
            if let Some((drive, rest)) = normalized.split_once(":/") {
                return format!("/{}/{}", drive.to_ascii_lowercase(), rest);
            }
            normalized
        }
        #[cfg(not(windows))]
        {
            value.into_owned()
        }
    }

    #[test]
    fn spec_declares_command_description_and_timeout() {
        let spec = BashTool::spec();
        assert_eq!(spec.name, "bash");
        assert_eq!(
            spec.input_schema["required"],
            serde_json::json!(["command"])
        );
        assert_eq!(
            spec.input_schema["properties"]["description"]["type"],
            "string"
        );
        assert_eq!(
            spec.input_schema["properties"]["timeout"]["default"],
            DEFAULT_BASH_TIMEOUT_SECS
        );
        assert!(spec.description.contains("120 seconds by default"));
        assert!(spec.description.contains("2000 lines or 50 KB"));
        assert!(
            spec.description
                .contains("provider credential variables are removed")
        );
        assert!(
            spec.description
                .contains("startup files may re-export them")
        );
        assert_eq!(
            BashTool::new().default_timeout_secs,
            DEFAULT_BASH_TIMEOUT_SECS
        );
    }

    #[test]
    fn configured_timeout_is_reflected_in_the_tool_spec() {
        let spec = BashTool::spec_with_default_timeout_secs(30);
        assert_eq!(spec.input_schema["properties"]["timeout"]["default"], 30);
        assert!(spec.description.contains("30 seconds by default"));
        assert_eq!(
            spec.input_schema["properties"]["timeout"]["description"],
            "Maximum run time in seconds (default 30)"
        );
    }

    #[test]
    fn removes_leg_credentials_but_preserves_other_environment_variables() {
        if !bash_available() {
            return;
        }
        let home = temp_dir("credentials");
        let tool = BashTool::new()
            .with_env("HOME", home.as_os_str().to_os_string())
            .with_env("ANTHROPIC_API_KEY", "test-api-key")
            .with_env("ANTHROPIC_AUTH_TOKEN", "test-auth-token")
            .with_env("CLAUDE_CODE_OAUTH_TOKEN", "test-oauth-token")
            .with_env("MAT_TEST_MARKER", "visible");
        let result = tool
            .call(&serde_json::json!({
                "command": "env | while IFS= read -r line; do case \"$line\" in ANTHROPIC_API_KEY=*|ANTHROPIC_AUTH_TOKEN=*|CLAUDE_CODE_OAUTH_TOKEN=*|MAT_TEST_MARKER=*) printf '%s\\n' \"$line\";; esac; done"
            }))
            .map(|output| serde_json::from_str::<Value>(&output).unwrap());
        std::fs::remove_dir_all(home).unwrap();

        let output = result.unwrap();
        assert_eq!(output["exit_code"], 0);
        let stdout = output["stdout"].as_str().unwrap();
        for var in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            assert!(
                !stdout
                    .lines()
                    .any(|line| line.starts_with(&format!("{var}="))),
                "{var} was present in the command environment: {stdout}"
            );
        }
        assert!(stdout.lines().any(|line| line == "MAT_TEST_MARKER=visible"));
    }

    #[test]
    fn runs_command_in_caller_directory_and_captures_streams_and_exit_code() {
        if !bash_available() {
            return;
        }
        let cwd = std::env::current_dir().unwrap();
        let command = "printf 'out'; printf 'err' >&2; printf '|%s' \"$PWD\"";
        let output = call(
            command,
            serde_json::json!({"description": "verify streams", "timeout": 30}),
        )
        .unwrap();
        assert_eq!(output["status"], "exited");
        assert_eq!(output["exit_code"], 0);
        let windows_path = cwd.to_string_lossy().replace('\\', "/");
        assert!(
            output["stdout"] == format!("out|{}", shell_path(&cwd))
                || output["stdout"] == format!("out|{windows_path}")
        );
        assert_eq!(output["stderr"], "err");
        assert_eq!(output["stdout_omitted_bytes"], 0);
        assert_eq!(output["stderr_omitted_bytes"], 0);

        let failure = call(
            "printf 'failed'; exit 7",
            serde_json::json!({"timeout": 30}),
        )
        .unwrap();
        assert_eq!(failure["status"], "exited");
        assert_eq!(failure["exit_code"], 7);
        assert_eq!(failure["stdout"], "failed");
    }

    #[test]
    fn rejects_invalid_arguments_and_reports_missing_bash() {
        assert_eq!(
            BashTool::new().call(&serde_json::json!({})).unwrap_err(),
            "bash: `command` must be a string"
        );
        assert_eq!(
            BashTool::new()
                .call(&serde_json::json!({"command": "true", "timeout": -1}))
                .unwrap_err(),
            "bash: `timeout` must be a non-negative integer"
        );
        assert!(
            BashTool::with_shell("leg-bash-command-that-does-not-exist")
                .call(&serde_json::json!({"command": "true"}))
                .unwrap_err()
                .contains("bash: executable was not found:")
        );
    }

    #[test]
    fn explicit_timeout_keeps_partial_output_and_returns_124() {
        if !bash_available() {
            return;
        }
        let timeout = if cfg!(windows) {
            login_startup_budget_secs() + 10
        } else {
            1
        };
        let output = call(
            "printf 'before-timeout'; sleep 30",
            serde_json::json!({"timeout": timeout}),
        )
        .unwrap();
        assert_eq!(output["status"], "timed_out");
        assert_eq!(output["exit_code"], 124);
        assert_eq!(output["stdout"], "before-timeout");
        assert!(output["wall_time_seconds"].as_f64().unwrap() < (timeout + 4) as f64);
    }

    #[cfg(unix)]
    #[test]
    fn reports_signal_termination_as_128_plus_signal() {
        if !bash_available() {
            return;
        }
        let output = call("kill -TERM $$", serde_json::json!({"timeout": 30})).unwrap();
        assert_eq!(output["status"], "exited");
        assert_eq!(output["exit_code"], 128 + libc::SIGTERM);
    }

    #[test]
    fn configured_default_timeout_is_used_when_timeout_is_omitted() {
        if !bash_available() {
            return;
        }
        let timeout = if cfg!(windows) {
            login_startup_budget_secs() + 3
        } else {
            1
        };
        let started = Instant::now();
        let output = call_with_default_timeout(timeout, "sleep 30", serde_json::json!({})).unwrap();
        assert_eq!(output["status"], "timed_out");
        assert_eq!(output["exit_code"], 124);
        assert!(started.elapsed() >= Duration::from_secs(timeout));
        assert!(started.elapsed() < Duration::from_secs(timeout + 4));
    }

    #[test]
    fn caps_each_stream_by_line_and_byte_limits_and_reports_omitted_bytes() {
        if !bash_available() {
            return;
        }
        let lines = call(
            "for ((i=0; i<2101; i++)); do printf 'out-%04d\\n' \"$i\"; printf 'err-%04d\\n' \"$i\" >&2; done",
            serde_json::json!({"timeout": 30}),
        )
        .unwrap();
        assert_eq!(lines["stdout_omitted_bytes"], 101 * 9);
        assert_eq!(lines["stderr_omitted_bytes"], 101 * 9);
        assert!(lines["stdout"].as_str().unwrap().starts_with("out-0000\n"));
        assert!(lines["stdout"].as_str().unwrap().contains("out-2100\n"));
        assert!(
            lines["stdout"]
                .as_str()
                .unwrap()
                .contains("909 bytes omitted")
        );
        assert!(lines["stderr"].as_str().unwrap().starts_with("err-0000\n"));
        assert!(lines["stderr"].as_str().unwrap().contains("err-2100\n"));

        let bytes = call(
            "printf '%60000s' x; printf '%60000s' y >&2",
            serde_json::json!({"timeout": 30}),
        )
        .unwrap();
        assert_eq!(bytes["stdout_omitted_bytes"], 8_800);
        assert_eq!(bytes["stderr_omitted_bytes"], 8_800);
        assert!(
            bytes["stdout"]
                .as_str()
                .unwrap()
                .contains("8800 bytes omitted")
        );
        assert!(
            bytes["stderr"]
                .as_str()
                .unwrap()
                .contains("8800 bytes omitted")
        );
    }

    #[test]
    fn timeout_terminates_background_descendants() {
        if !bash_available() {
            return;
        }
        let dir = temp_dir("descendants");
        let marker = dir.join("survived");
        let marker = shell_quote(&shell_path(&marker));
        let timeout = if cfg!(windows) {
            login_startup_budget_secs() + 10
        } else {
            1
        };
        let sleep = if cfg!(windows) { timeout + 2 } else { 2 };
        let command = format!("printf launched; ( sleep {sleep}; printf alive > {marker} ) & wait");
        let started = Instant::now();
        let output = call(&command, serde_json::json!({"timeout": timeout})).unwrap();
        assert_eq!(output["status"], "timed_out");
        assert_eq!(output["stdout"], "launched");
        thread::sleep(Duration::from_secs((sleep + 1) as u64));
        assert!(
            !dir.join("survived").exists(),
            "background process survived timeout"
        );
        assert!(started.elapsed() < Duration::from_secs((timeout + sleep + 4) as u64));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stops_waiting_after_the_pipe_drain_guard() {
        if !bash_available()
            || !Command::new("python3")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }

        let output = call(
            "python3 -c 'import os,time; os.setsid(); print(os.getpid(), flush=True); time.sleep(30)' & wait",
            serde_json::json!({"timeout": 1}),
        )
        .unwrap();
        let child_pid = output["stdout"]
            .as_str()
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .expect("the detached child printed its pid");
        // SAFETY: this test owns the detached helper process and sends it a
        // signal before asserting the output-drain timing.
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
        }
        assert_eq!(output["status"], "timed_out");
        assert_eq!(output["exit_code"], 124);
        let elapsed = output["wall_time_seconds"].as_f64().unwrap();
        assert!(elapsed >= 3.0, "drain guard was not observed: {elapsed}s");
        assert!(elapsed < 4.0, "drain guard exceeded its bound: {elapsed}s");
    }
}
