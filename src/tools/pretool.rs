//! Shared pre-tool hook decision types and limits.

use std::time::Duration;

pub(super) const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const GENERIC_DENIAL: &str = "denied by pre-tool hook: hook failed";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PreToolDecision {
    Allow,
    Deny(String),
    Fail,
}

pub(super) fn parse_decision_kind(stdout: &str) -> PreToolDecision {
    if stdout.trim().is_empty() {
        return PreToolDecision::Allow;
    }

    let Ok(decision) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return PreToolDecision::Fail;
    };
    match decision.get("decision").and_then(serde_json::Value::as_str) {
        Some("allow") => PreToolDecision::Allow,
        Some("deny") => decision
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .map(|reason| PreToolDecision::Deny(reason.to_string()))
            .unwrap_or(PreToolDecision::Fail),
        _ => PreToolDecision::Fail,
    }
}
