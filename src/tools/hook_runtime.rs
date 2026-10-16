//! Bounded execution for configured lifecycle hooks.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::events::ToolStatus;
use crate::hooks::{HookEvent, HookSet};
use crate::interrupt;

use super::pretool::{GENERIC_DENIAL, HOOK_TIMEOUT, PreToolDecision, parse_decision_kind};
use super::process;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HookContext {
    pub(crate) session_id: Option<String>,
    pub(crate) turn_index: Option<u64>,
}

impl HookContext {
    pub(crate) fn new(session_id: Option<&str>, turn_index: Option<u64>) -> Self {
        Self {
            session_id: session_id.map(str::to_owned),
            turn_index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookOutcomeKind {
    Allowed,
    Denied(String),
    Succeeded { stdout: String, stderr: String },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookOutcome {
    pub(crate) event: HookEvent,
    pub(crate) hook_id: Option<String>,
    pub(crate) command: PathBuf,
    pub(crate) kind: HookOutcomeKind,
}

#[derive(Debug)]
pub(super) struct HookRuntime {
    hooks: HookSet,
    legacy_pre_tool: Option<PathBuf>,
    timeout: Duration,
}

impl Default for HookRuntime {
    fn default() -> Self {
        Self::new(HookSet::default(), None)
    }
}

impl HookRuntime {
    pub(super) fn new(hooks: HookSet, legacy_pre_tool: Option<PathBuf>) -> Self {
        Self {
            hooks,
            legacy_pre_tool,
            timeout: HOOK_TIMEOUT,
        }
    }

    #[cfg(all(test, unix))]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(super) fn authorize_tool(
        &self,
        tool_name: &str,
        tool_input: &Value,
        context: &HookContext,
    ) -> std::result::Result<Vec<HookOutcome>, PreToolDenial> {
        let mut outcomes = Vec::new();
        let mut candidates = Vec::new();
        if let Some(path) = self.legacy_pre_tool.as_ref() {
            candidates.push((None, path.clone()));
        }
        candidates.extend(
            self.hooks
                .select(HookEvent::PreToolUse, Some(tool_name))
                .into_iter()
                .map(|hook| (Some(hook.id.clone()), hook.command.clone())),
        );

        for (hook_id, command) in candidates {
            let payload = match serialize_payload(
                HookEvent::PreToolUse,
                context,
                serde_json::json!({
                    "tool_name": tool_name,
                    "tool_input": tool_input,
                }),
            ) {
                Ok(payload) => payload,
                Err(reason) => {
                    return Err(pre_tool_denial(
                        GENERIC_DENIAL,
                        &mut outcomes,
                        HookOutcome {
                            event: HookEvent::PreToolUse,
                            hook_id,
                            command,
                            kind: HookOutcomeKind::Failed(reason),
                        },
                    ));
                }
            };
            let output = match process::run_executable(command.as_os_str(), payload, self.timeout) {
                Ok(output) => output,
                Err(error) => {
                    return Err(pre_tool_denial(
                        GENERIC_DENIAL,
                        &mut outcomes,
                        HookOutcome {
                            event: HookEvent::PreToolUse,
                            hook_id,
                            command,
                            kind: HookOutcomeKind::Failed(format!("could not start hook: {error}")),
                        },
                    ));
                }
            };

            if interrupt::is_signaled() {
                return Err(pre_tool_denial(
                    GENERIC_DENIAL,
                    &mut outcomes,
                    HookOutcome {
                        event: HookEvent::PreToolUse,
                        hook_id,
                        command,
                        kind: HookOutcomeKind::Failed("hook cancelled".to_string()),
                    },
                ));
            }
            if output.status != "exited"
                || output.exit_code != 0
                || output.stdout.omitted_bytes != 0
                || !output.stdout.valid_utf8
            {
                let reason = if output.status != "exited" || output.exit_code != 0 {
                    process_failure_reason(
                        output.status,
                        output.exit_code,
                        &output.stderr.text,
                        self.timeout,
                    )
                } else {
                    "hook returned truncated or invalid UTF-8 stdout".to_string()
                };
                return Err(pre_tool_denial(
                    GENERIC_DENIAL,
                    &mut outcomes,
                    HookOutcome {
                        event: HookEvent::PreToolUse,
                        hook_id,
                        command,
                        kind: HookOutcomeKind::Failed(reason),
                    },
                ));
            }

            match parse_decision_kind(&output.stdout.text) {
                PreToolDecision::Allow => outcomes.push(HookOutcome {
                    event: HookEvent::PreToolUse,
                    hook_id,
                    command,
                    kind: HookOutcomeKind::Allowed,
                }),
                PreToolDecision::Fail => {
                    return Err(pre_tool_denial(
                        GENERIC_DENIAL,
                        &mut outcomes,
                        HookOutcome {
                            event: HookEvent::PreToolUse,
                            hook_id,
                            command,
                            kind: HookOutcomeKind::Failed(
                                "hook returned an invalid decision".to_string(),
                            ),
                        },
                    ));
                }
                PreToolDecision::Deny(reason) => {
                    let message = format!("denied by pre-tool hook: {reason}");
                    return Err(pre_tool_denial(
                        message,
                        &mut outcomes,
                        HookOutcome {
                            event: HookEvent::PreToolUse,
                            hook_id,
                            command,
                            kind: HookOutcomeKind::Denied(reason),
                        },
                    ));
                }
            }
        }

        Ok(outcomes)
    }

    pub(super) fn run_observational(
        &self,
        event: HookEvent,
        context: &HookContext,
        fields: Value,
    ) -> Vec<HookOutcome> {
        debug_assert_ne!(event, HookEvent::PreToolUse);
        let tool_name = fields.get("tool_name").and_then(Value::as_str);
        let selected = self.hooks.select(event, tool_name);
        if selected.is_empty() {
            return Vec::new();
        }
        let payload = serialize_payload(event, context, fields);
        selected
            .into_iter()
            .map(|hook| {
                let payload = match &payload {
                    Ok(payload) => payload.clone(),
                    Err(reason) => {
                        return HookOutcome {
                            event,
                            hook_id: Some(hook.id.clone()),
                            command: hook.command.clone(),
                            kind: HookOutcomeKind::Failed(reason.clone()),
                        };
                    }
                };
                let outcome = match process::run_executable(
                    hook.command.as_os_str(),
                    payload,
                    self.timeout,
                ) {
                    Err(error) => HookOutcomeKind::Failed(format!("could not start hook: {error}")),
                    Ok(_output) if interrupt::is_signaled() => {
                        HookOutcomeKind::Failed("hook cancelled".to_string())
                    }
                    Ok(output) if output.status != "exited" || output.exit_code != 0 => {
                        HookOutcomeKind::Failed(process_failure_reason(
                            output.status,
                            output.exit_code,
                            &output.stderr.text,
                            self.timeout,
                        ))
                    }
                    Ok(output) => HookOutcomeKind::Succeeded {
                        stdout: output.stdout.text,
                        stderr: output.stderr.text,
                    },
                };
                HookOutcome {
                    event,
                    hook_id: Some(hook.id.clone()),
                    command: hook.command.clone(),
                    kind: outcome,
                }
            })
            .collect()
    }
}

#[derive(Debug)]
pub(super) struct PreToolDenial {
    pub(super) message: String,
    pub(super) outcomes: Vec<HookOutcome>,
}

fn pre_tool_denial(
    message: impl Into<String>,
    outcomes: &mut Vec<HookOutcome>,
    outcome: HookOutcome,
) -> PreToolDenial {
    outcomes.push(outcome);
    PreToolDenial {
        message: message.into(),
        outcomes: std::mem::take(outcomes),
    }
}

#[derive(Serialize)]
struct Payload<'a> {
    hook_event_name: &'a str,
    cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_index: Option<u64>,
    #[serde(flatten)]
    fields: &'a Map<String, Value>,
}

fn serialize_payload(
    event: HookEvent,
    context: &HookContext,
    fields: Value,
) -> std::result::Result<Vec<u8>, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("could not determine hook working directory: {error}"))?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| "hook working directory is not valid UTF-8".to_string())?;
    serialize_payload_with_cwd(event, context, fields, cwd)
}

fn serialize_payload_with_cwd(
    event: HookEvent,
    context: &HookContext,
    fields: Value,
    cwd: &str,
) -> std::result::Result<Vec<u8>, String> {
    let fields = fields
        .as_object()
        .ok_or_else(|| "hook payload fields must be a JSON object".to_string())?;
    serde_json::to_vec(&Payload {
        hook_event_name: event.as_str(),
        cwd,
        session_id: context.session_id.as_deref(),
        turn_index: context.turn_index,
        fields,
    })
    .map_err(|error| format!("could not serialize hook payload: {error}"))
}

pub(crate) fn report_hook_failures(outcomes: &[HookOutcome]) {
    for outcome in outcomes {
        let HookOutcomeKind::Failed(reason) = &outcome.kind else {
            continue;
        };
        let source = outcome
            .hook_id
            .as_deref()
            .map(|id| format!("{id:?}"))
            .unwrap_or_else(|| format!("{}", outcome.command.display()));
        eprintln!(
            "warning: {} hook {source} failed: {reason}",
            outcome.event.as_str()
        );
    }
}

fn process_failure_reason(status: &str, exit_code: i32, stderr: &str, timeout: Duration) -> String {
    if status == "timed_out" {
        return format!("hook timed out after {} seconds", timeout.as_secs_f64());
    }
    let detail: String = stderr
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(240)
        .collect();
    if detail.is_empty() {
        format!("hook exited with status {exit_code}")
    } else {
        format!("hook exited with status {exit_code}: {detail}")
    }
}

pub(crate) fn tool_status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied => "denied",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_uses_the_fixed_production_timeout_by_default() {
        assert_eq!(HOOK_TIMEOUT, Duration::from_secs(30));
        assert_eq!(HookRuntime::default().timeout, HOOK_TIMEOUT);
        assert_eq!(
            HookRuntime::new(HookSet::default(), None).timeout,
            HOOK_TIMEOUT
        );
    }

    #[test]
    fn payload_has_common_identity_and_event_specific_fields() {
        let context = HookContext::new(Some("sess-7"), Some(3));
        let payload = serialize_payload_with_cwd(
            HookEvent::PostToolUse,
            &context,
            serde_json::json!({
                "tool_name": "bash",
                "tool_input": { "command": "echo hi" },
                "tool_output": "hi",
                "tool_status": "completed",
            }),
            "C:/workspace",
        )
        .unwrap();
        let payload: Value = serde_json::from_slice(&payload).unwrap();

        assert_eq!(payload["hook_event_name"], "PostToolUse");
        assert_eq!(payload["cwd"], "C:/workspace");
        assert_eq!(payload["session_id"], "sess-7");
        assert_eq!(payload["turn_index"], 3);
        assert_eq!(payload["tool_name"], "bash");
        assert_eq!(payload["tool_input"]["command"], "echo hi");
        assert_eq!(payload["tool_output"], "hi");
        assert_eq!(payload["tool_status"], "completed");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        struct TempDir(PathBuf);

        impl TempDir {
            fn new() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("leg-hook-runtime-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&path).expect("create temporary directory");
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

        fn load_hooks(config_path: &Path, entries: Vec<Value>) -> HookSet {
            let contents = serde_json::json!({ "version": 1, "hooks": entries }).to_string();
            crate::hooks::load_hook_set(
                &crate::hooks::HookConfigPaths {
                    explicit: Some(config_path.to_path_buf()),
                    ..Default::default()
                },
                |path| {
                    if path == config_path {
                        Ok(contents.clone())
                    } else {
                        Err(std::io::ErrorKind::NotFound.into())
                    }
                },
            )
            .expect("load hook set")
        }

        fn quote(path: &Path) -> String {
            format!("'{}'", path.display())
        }

        #[test]
        fn legacy_pretool_payload_and_allow_use_the_runtime_path() {
            let dir = TempDir::new();
            let payload_path = dir.0.join("legacy-payload.json");
            let legacy = dir.executable(
                "legacy-hook",
                &format!(
                    "payload=\"$(cat)\"\nprintf '%s' \"$payload\" > {}\nprintf '%s' '{{\"decision\":\"allow\"}}'",
                    quote(&payload_path)
                ),
            );
            let runtime = HookRuntime::new(HookSet::default(), Some(legacy));
            let context = HookContext::new(Some("sess-9"), Some(4));
            let input = serde_json::json!({ "command": "echo hi" });

            let outcomes = runtime
                .authorize_tool("bash", &input, &context)
                .expect("legacy allow permits tool dispatch");

            assert_eq!(outcomes.len(), 1);
            assert_eq!(outcomes[0].hook_id, None);
            assert_eq!(outcomes[0].kind, HookOutcomeKind::Allowed);
            let payload: Value =
                serde_json::from_slice(&std::fs::read(payload_path).unwrap()).unwrap();
            assert_eq!(payload["hook_event_name"], "PreToolUse");
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(payload["cwd"], cwd);
            assert_eq!(payload["session_id"], "sess-9");
            assert_eq!(payload["turn_index"], 4);
            assert_eq!(payload["tool_name"], "bash");
            assert_eq!(payload["tool_input"], input);

            let empty = dir.executable("legacy-empty", "cat >/dev/null");
            let outcomes = HookRuntime::new(HookSet::default(), Some(empty))
                .authorize_tool("bash", &Value::Null, &HookContext::default())
                .expect("legacy empty stdout remains an allow decision");
            assert_eq!(outcomes[0].kind, HookOutcomeKind::Allowed);
        }

        #[test]
        fn legacy_pretool_deny_preserves_its_reason_on_the_runtime_path() {
            let dir = TempDir::new();
            let legacy = dir.executable(
                "legacy-deny",
                "cat >/dev/null\nprintf '%s' '{\"decision\":\"deny\",\"reason\":\"bash is forbidden\"}'",
            );
            let runtime = HookRuntime::new(HookSet::default(), Some(legacy));

            let denial = runtime
                .authorize_tool("bash", &Value::Null, &HookContext::default())
                .expect_err("legacy deny prevents tool dispatch");

            assert_eq!(denial.message, "denied by pre-tool hook: bash is forbidden");
            assert_eq!(denial.outcomes.len(), 1);
            assert_eq!(
                denial.outcomes[0].kind,
                HookOutcomeKind::Denied("bash is forbidden".to_string())
            );
        }

        #[test]
        fn legacy_pretool_process_failures_fail_closed_on_the_runtime_path() {
            let dir = TempDir::new();
            let nonzero = dir.executable(
                "nonzero",
                "cat >/dev/null\nprintf '%s' '{\"decision\":\"allow\"}'\nexit 1",
            );
            let malformed = dir.executable("malformed", "cat >/dev/null\nprintf '%s' garbage");
            let invalid_utf8 = dir.executable(
                "invalid-utf8",
                r#"cat >/dev/null
printf '{"decision":"allow","extra":"\377"}'
"#,
            );
            let missing = dir.0.join("missing-hook");

            for path in [nonzero, malformed, invalid_utf8, missing] {
                let denial = HookRuntime::new(HookSet::default(), Some(path))
                    .authorize_tool("bash", &Value::Null, &HookContext::default())
                    .expect_err("legacy process failure fails closed");

                assert_eq!(denial.message, GENERIC_DENIAL);
                assert_eq!(denial.outcomes.len(), 1);
                assert!(matches!(
                    &denial.outcomes[0].kind,
                    HookOutcomeKind::Failed(_)
                ));
            }
        }

        #[test]
        fn legacy_pretool_timeout_stops_and_reaps_the_runtime_child() {
            let dir = TempDir::new();
            let child_pid = dir.0.join("hook-pid");
            let finished = dir.0.join("finished");
            let legacy = dir.executable(
                "hanging-hook",
                &format!(
                    "printf '%s' \"$$\" > {}\nsleep 30\nprintf '%s' finished > {}",
                    quote(&child_pid),
                    quote(&finished)
                ),
            );
            let runtime = HookRuntime::new(HookSet::default(), Some(legacy))
                .with_timeout(Duration::from_millis(250));

            let denial = runtime
                .authorize_tool("bash", &Value::Null, &HookContext::default())
                .expect_err("timed out legacy hook fails closed");

            assert_eq!(denial.message, GENERIC_DENIAL);
            assert!(matches!(
                &denial.outcomes[0].kind,
                HookOutcomeKind::Failed(reason) if reason.contains("timed out")
            ));
            assert!(!finished.exists(), "timed-out hook continued running");
            let pid = std::fs::read_to_string(child_pid)
                .expect("hook pid file")
                .parse::<libc::pid_t>()
                .expect("hook pid");
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "timed-out hook child was not reaped"
            );
        }

        #[test]
        fn pretool_runs_legacy_then_file_order_and_stops_after_deny() {
            let dir = TempDir::new();
            let trace = dir.0.join("trace.txt");
            let legacy_payload = dir.0.join("legacy-payload.json");
            let legacy = dir.executable(
                "legacy-hook",
                &format!(
                    "payload=\"$(cat)\"\nprintf '%s\\n' legacy >> {}\nprintf '%s' \"$payload\" > {}\nprintf '%s' '{{\"decision\":\"allow\"}}'",
                    quote(&trace),
                    quote(&legacy_payload)
                ),
            );
            let allow = dir.executable(
                "allow-hook",
                &format!(
                    "cat >/dev/null\nprintf '%s\\n' declarative-allow >> {}\nprintf '%s' '{{\"decision\":\"allow\"}}'",
                    quote(&trace)
                ),
            );
            let deny = dir.executable(
                "deny-hook",
                &format!(
                    "cat >/dev/null\nprintf '%s\\n' declarative-deny >> {}\nprintf '%s' '{{\"decision\":\"deny\",\"reason\":\"policy\"}}'",
                    quote(&trace)
                ),
            );
            let later = dir.executable(
                "later-hook",
                &format!(
                    "cat >/dev/null\nprintf '%s\\n' later >> {}\nprintf '%s' '{{\"decision\":\"allow\"}}'",
                    quote(&trace)
                ),
            );
            let hooks = load_hooks(
                &dir.0.join("hooks.json"),
                [("allow", allow), ("deny", deny), ("later", later)]
                    .into_iter()
                    .map(|(id, command)| {
                        serde_json::json!({
                            "id": id,
                            "event": "PreToolUse",
                            "matcher": "bash",
                            "command": command.to_string_lossy(),
                        })
                    })
                    .collect(),
            );
            let runtime = HookRuntime::new(hooks, Some(legacy));
            let context = HookContext::new(Some("sess-9"), Some(4));

            let denial = runtime
                .authorize_tool(
                    "bash",
                    &serde_json::json!({ "command": "echo hi" }),
                    &context,
                )
                .unwrap_err();

            assert_eq!(denial.message, "denied by pre-tool hook: policy");
            assert_eq!(
                denial
                    .outcomes
                    .iter()
                    .map(|outcome| outcome.hook_id.as_deref().unwrap_or("legacy"))
                    .collect::<Vec<_>>(),
                ["legacy", "allow", "deny"]
            );
            assert_eq!(
                std::fs::read_to_string(trace).unwrap(),
                "legacy\ndeclarative-allow\ndeclarative-deny\n"
            );
            let legacy_payload: Value =
                serde_json::from_slice(&std::fs::read(legacy_payload).unwrap()).unwrap();
            assert_eq!(legacy_payload["hook_event_name"], "PreToolUse");
            assert_eq!(legacy_payload["session_id"], "sess-9");
            assert_eq!(legacy_payload["turn_index"], 4);
            assert_eq!(legacy_payload["tool_input"]["command"], "echo hi");
        }

        #[test]
        fn fail_closed_pretool_error_stops_remaining_hooks() {
            let dir = TempDir::new();
            let later_marker = dir.0.join("later-ran");
            let later = dir.executable(
                "later-hook",
                &format!(
                    "cat >/dev/null\nprintf '%s' ran > {}\nprintf '%s' '{{\"decision\":\"allow\"}}'",
                    quote(&later_marker)
                ),
            );
            let hooks = load_hooks(
                &dir.0.join("hooks.json"),
                [("missing", dir.0.join("missing-hook")), ("later", later)]
                    .into_iter()
                    .map(|(id, command)| {
                        serde_json::json!({
                            "id": id,
                            "event": "PreToolUse",
                            "matcher": "bash",
                            "command": command.to_string_lossy(),
                        })
                    })
                    .collect(),
            );
            let runtime = HookRuntime::new(hooks, None);

            let denial = runtime
                .authorize_tool("bash", &Value::Null, &HookContext::default())
                .unwrap_err();

            assert_eq!(denial.message, GENERIC_DENIAL);
            assert_eq!(denial.outcomes.len(), 1);
            assert_eq!(denial.outcomes[0].hook_id.as_deref(), Some("missing"));
            assert!(matches!(
                &denial.outcomes[0].kind,
                HookOutcomeKind::Failed(_)
            ));
            assert!(!later_marker.exists(), "hook ran after fail-closed error");
        }

        #[test]
        fn observational_failure_is_structured_and_later_hooks_still_run() {
            let dir = TempDir::new();
            let second_payload = dir.0.join("second-payload.json");
            let fails = dir.executable(
                "fails",
                "cat >/dev/null\nprintf '%s' 'diagnostic from hook' >&2\nexit 7",
            );
            let succeeds = dir.executable(
                "succeeds",
                &format!(
                    "payload=\"$(cat)\"\nprintf '%s' \"$payload\" > {}\nprintf '%s' observer-output",
                    quote(&second_payload)
                ),
            );
            let hooks = load_hooks(
                &dir.0.join("hooks.json"),
                [("first", fails), ("second", succeeds)]
                    .into_iter()
                    .map(|(id, command)| {
                        serde_json::json!({
                            "id": id,
                            "event": "PostToolUse",
                            "matcher": "bash",
                            "command": command.to_string_lossy(),
                        })
                    })
                    .collect(),
            );
            let runtime = HookRuntime::new(hooks, None);
            let outcomes = runtime.run_observational(
                HookEvent::PostToolUse,
                &HookContext::new(Some("sess-2"), Some(8)),
                serde_json::json!({
                    "tool_name": "bash",
                    "tool_input": { "command": "echo hi" },
                    "tool_output": "hi",
                    "tool_status": "completed",
                }),
            );

            assert_eq!(outcomes.len(), 2);
            assert_eq!(outcomes[0].hook_id.as_deref(), Some("first"));
            assert!(matches!(
                &outcomes[0].kind,
                HookOutcomeKind::Failed(reason) if reason.contains("diagnostic from hook")
            ));
            assert_eq!(outcomes[1].hook_id.as_deref(), Some("second"));
            assert!(matches!(
                &outcomes[1].kind,
                HookOutcomeKind::Succeeded { stdout, .. } if stdout == "observer-output"
            ));
            let payload: Value =
                serde_json::from_slice(&std::fs::read(second_payload).unwrap()).unwrap();
            assert_eq!(payload["hook_event_name"], "PostToolUse");
            assert_eq!(payload["session_id"], "sess-2");
            assert_eq!(payload["turn_index"], 8);
            assert_eq!(payload["tool_status"], "completed");
        }

        const CANCEL_CHILD_ENV: &str = "LEG_HOOK_CANCEL_CHILD";

        #[test]
        fn cancellation_child_test() {
            if std::env::var_os(CANCEL_CHILD_ENV).is_none() {
                return;
            }
            crate::interrupt::install().expect("install interrupt handler");
            let hook = PathBuf::from(std::env::var_os("LEG_HOOK_CANCEL_PATH").unwrap());
            let finished = PathBuf::from(std::env::var_os("LEG_HOOK_CANCEL_FINISHED").unwrap());
            let runtime = HookRuntime::new(HookSet::default(), Some(hook));

            let denial = runtime
                .authorize_tool("bash", &Value::Null, &HookContext::default())
                .expect_err("cancelled pre-tool hook fails closed");

            assert_eq!(denial.message, GENERIC_DENIAL);
            assert!(crate::interrupt::is_signaled());
            assert!(!finished.exists(), "hook continued after cancellation");
        }

        #[test]
        fn cancellation_stops_and_reaps_the_hook_child() {
            let dir = TempDir::new();
            let started = dir.0.join("started");
            let finished = dir.0.join("finished");
            let child_pid = dir.0.join("hook-pid");
            let hook = dir.executable(
                "hanging-hook",
                &format!(
                    "printf '%s' started > {}\nprintf '%s' \"$$\" > {}\nsleep 30\nprintf '%s' finished > {}",
                    quote(&started),
                    quote(&child_pid),
                    quote(&finished)
                ),
            );
            let mut child = Command::new(std::env::current_exe().expect("test binary path"))
                .args([
                    "--exact",
                    "tools::hook_runtime::tests::unix::cancellation_child_test",
                    "--nocapture",
                ])
                .env(CANCEL_CHILD_ENV, "1")
                .env("LEG_HOOK_CANCEL_PATH", &hook)
                .env("LEG_HOOK_CANCEL_FINISHED", &finished)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start isolated cancellation test process");

            let deadline = Instant::now() + Duration::from_secs(5);
            while (!started.exists() || !child_pid.exists()) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !started.exists() || !child_pid.exists() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("hook child did not start");
            }
            let pid = std::fs::read_to_string(child_pid)
                .expect("hook pid file")
                .parse::<libc::pid_t>()
                .expect("hook pid");
            if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) } != 0 {
                let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                panic!("could not interrupt isolated hook test process");
            }

            let child_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < child_deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("cancelled hook test process did not exit promptly");
                    }
                    Err(error) => {
                        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("could not wait for cancellation test: {error}");
                    }
                }
            }

            let result = child.wait_with_output().expect("wait for isolated test");
            assert!(
                result.status.success(),
                "cancellation child failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(!finished.exists(), "hook child was not stopped");
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "hook child process was not reaped"
            );
        }
    }
}
