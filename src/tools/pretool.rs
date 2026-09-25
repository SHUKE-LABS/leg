//! Optional fail-closed pre-tool hook execution.

use std::path::PathBuf;
use std::time::Duration;

use crate::tools::process;

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
const GENERIC_DENIAL: &str = "denied by pre-tool hook: hook failed";

pub(super) struct PreToolHook {
    executable: PathBuf,
    timeout: Duration,
}

impl PreToolHook {
    pub(super) fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            timeout: HOOK_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(super) fn authorize(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|path| path.into_os_string().into_string().ok())
            .ok_or_else(generic_denial)?;
        let payload = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool_name,
            "tool_input": tool_input,
            "cwd": cwd,
        }))
        .map_err(|_| generic_denial())?;
        let output = process::run_executable(self.executable.as_os_str(), payload, self.timeout)
            .map_err(|_| generic_denial())?;
        if output.status != "exited"
            || output.exit_code != 0
            || output.stdout.omitted_bytes != 0
            || !output.stdout.valid_utf8
        {
            return Err(generic_denial());
        }
        parse_decision(&output.stdout.text)
    }
}

fn generic_denial() -> String {
    GENERIC_DENIAL.to_string()
}

fn parse_decision(stdout: &str) -> std::result::Result<(), String> {
    if stdout.trim().is_empty() {
        return Ok(());
    }

    let decision: serde_json::Value = serde_json::from_str(stdout).map_err(|_| generic_denial())?;
    match decision.get("decision").and_then(serde_json::Value::as_str) {
        Some("allow") => Ok(()),
        Some("deny") => {
            let reason = decision
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .ok_or_else(generic_denial)?;
            Err(format!("denied by pre-tool hook: {reason}"))
        }
        _ => Err(generic_denial()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_output_and_allow_decision_allow_dispatch() {
        assert_eq!(HOOK_TIMEOUT, Duration::from_secs(30));
        assert_eq!(parse_decision(" \n"), Ok(()));
        assert_eq!(parse_decision(r#"{"decision":"allow"}"#), Ok(()));
    }

    #[test]
    fn deny_decision_preserves_its_reason() {
        assert_eq!(
            parse_decision(r#"{"decision":"deny","reason":"role policy"}"#),
            Err("denied by pre-tool hook: role policy".to_string())
        );
    }

    #[test]
    fn malformed_or_incomplete_decisions_fail_closed() {
        for stdout in [
            "not json",
            r#"{"decision":"maybe"}"#,
            r#"{"decision":"deny"}"#,
            r#"{"decision":"deny","reason":""}"#,
        ] {
            assert_eq!(
                parse_decision(stdout),
                Err(GENERIC_DENIAL.to_string()),
                "{stdout}"
            );
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct TempDir(PathBuf);

        impl TempDir {
            fn new() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let path =
                    std::env::temp_dir().join(format!("leg-pretool-{}-{n}", std::process::id()));
                std::fs::create_dir(&path).expect("create temp directory");
                Self(path)
            }

            fn executable(&self, name: &str, body: &str) -> PathBuf {
                let path = self.0.join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write hook");
                let mut permissions = std::fs::metadata(&path)
                    .expect("hook metadata")
                    .permissions();
                permissions.set_mode(0o700);
                std::fs::set_permissions(&path, permissions).expect("make hook executable");
                path
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn sends_the_pretool_event_and_tool_context_on_stdin() {
            let dir = TempDir::new();
            let hook = dir.executable(
                "check-input",
                r#"payload="$(cat)"
printf '%s' "$payload" | grep -Fq '"hook_event_name":"PreToolUse"' || exit 2
printf '%s' "$payload" | grep -Fq '"tool_name":"bash"' || exit 3
printf '%s' "$payload" | grep -Fq '"tool_input":{"command":"echo hi"}' || exit 4
current_dir="$(pwd)"
printf '%s' "$payload" | grep -Fq "\"cwd\":\"$current_dir\"" || exit 5
printf '{"decision":"allow"}'
"#,
            );
            let hook = PreToolHook::new(hook);

            assert_eq!(
                hook.authorize("bash", &serde_json::json!({"command": "echo hi"})),
                Ok(())
            );
        }

        #[test]
        fn hook_process_deny_and_allow_decisions_are_enforced() {
            let dir = TempDir::new();
            let deny = dir.executable(
                "deny",
                r#"cat >/dev/null
printf '{"decision":"deny","reason":"bash is forbidden"}'
"#,
            );
            let allow = dir.executable(
                "allow",
                r#"cat >/dev/null
printf '{"decision":"allow"}'
"#,
            );

            assert_eq!(
                PreToolHook::new(deny)
                    .authorize("bash", &serde_json::json!({}))
                    .unwrap_err(),
                "denied by pre-tool hook: bash is forbidden"
            );
            assert_eq!(
                PreToolHook::new(allow).authorize("bash", &serde_json::json!({})),
                Ok(())
            );
        }

        #[test]
        fn hook_spawn_nonzero_and_timeout_fail_closed() {
            let dir = TempDir::new();
            let nonzero = dir.executable(
                "nonzero",
                r#"cat >/dev/null
printf '{"decision":"allow"}'
exit 1
"#,
            );
            let malformed = dir.executable(
                "malformed",
                r#"cat >/dev/null
printf 'garbage'
"#,
            );
            let invalid_utf8 = dir.executable(
                "invalid-utf8",
                r#"cat >/dev/null
printf '{"decision":"allow","extra":"\377"}'
"#,
            );
            let hanging = dir.executable(
                "hanging",
                r#"cat >/dev/null
sleep 5
"#,
            );

            for path in [nonzero, malformed, invalid_utf8, dir.0.join("missing")] {
                assert_eq!(
                    PreToolHook::new(path).authorize("bash", &serde_json::json!({})),
                    Err(GENERIC_DENIAL.to_string())
                );
            }

            assert_eq!(
                PreToolHook::new(hanging)
                    .with_timeout(Duration::from_millis(20))
                    .authorize("bash", &serde_json::json!({})),
                Err(GENERIC_DENIAL.to_string())
            );
        }
    }
}
