//! Environment-backed runtime configuration.
//!
//! [`LegConfig`] holds everything the single-turn first-reply path needs to
//! reach a provider. Loading is split into [`LegConfig::from_env`] (the real
//! entry point) and a pure [`LegConfig::from_lookup`] so parsing can be tested
//! deterministically without mutating the process environment.
//!
//! Authentication is modelled as a typed [`Credential`] so leg accepts either
//! an Anthropic API key (`ANTHROPIC_API_KEY`) or an OAuth bearer token
//! (`ANTHROPIC_AUTH_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN`). The first present
//! variable in that precedence order is the resolved credential; the
//! transport then picks the matching `x-api-key` or `Authorization: Bearer`
//! header from the variant.

use std::time::Duration;

use crate::error::{LegError, Result};

/// Default base URL for the Claude-compatible Messages API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Default model id used when `LEG_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Default request timeout in seconds when `LEG_TIMEOUT_SECS` is unset.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Default `max_tokens` requested per reply when `LEG_MAX_TOKENS` is unset.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// An authentication credential accepted by the provider transport.
///
/// Variants map 1:1 onto the wire-format header the transport emits:
/// `ApiKey` -> `x-api-key`, `OAuth` -> `Authorization: Bearer <token>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// An Anthropic API key, sent as the `x-api-key` header.
    ApiKey(String),
    /// An OAuth bearer token, sent as the `Authorization: Bearer <token>`
    /// header.
    OAuth(String),
}

/// Runtime configuration for leg's first-reply path.
#[derive(Debug, Clone)]
pub struct LegConfig {
    /// Resolved provider credential (API key or OAuth bearer token).
    pub credential: Credential,
    /// Base URL for the Messages API. From `ANTHROPIC_BASE_URL`, defaulting to
    /// [`DEFAULT_BASE_URL`].
    pub base_url: String,
    /// Model id to request. From `LEG_MODEL`, defaulting to [`DEFAULT_MODEL`].
    pub model: String,
    /// Per-request timeout. Derived from `LEG_TIMEOUT_SECS`, defaulting to
    /// [`DEFAULT_TIMEOUT_SECS`]. Must be a positive integer; zero is rejected
    /// because a zero deadline fails every request immediately.
    pub timeout: Duration,
    /// Maximum output tokens to request per reply. From `LEG_MAX_TOKENS`,
    /// defaulting to [`DEFAULT_MAX_TOKENS`]. Must be a positive integer; zero is
    /// rejected because the API rejects it.
    pub max_tokens: u32,
    /// Optional system prompt. When `LEG_SYSTEM_PROMPT` names a readable file,
    /// this holds its content; the transport then sends it as the request's
    /// `system` field. Unset or blank leaves this `None` and omits the field.
    pub system_prompt: Option<String>,
}

impl LegConfig {
    /// Loads configuration from the process environment.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Loads configuration from an arbitrary key lookup.
    ///
    /// `lookup` returns the value for a variable name, or `None` when it is
    /// unset. This is the testable core behind [`LegConfig::from_env`].
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let credential = resolve_credential(&lookup)?;

        let base_url =
            non_empty(lookup("ANTHROPIC_BASE_URL")).unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model = non_empty(lookup("LEG_MODEL")).unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let timeout_secs = match non_empty(lookup("LEG_TIMEOUT_SECS")) {
            Some(raw) => {
                let parsed = raw.parse::<u64>().map_err(|_| {
                    LegError::Config(format!(
                        "LEG_TIMEOUT_SECS must be a positive integer, got {raw:?}"
                    ))
                })?;
                if parsed == 0 {
                    return Err(LegError::Config(
                        "LEG_TIMEOUT_SECS must be greater than zero".to_string(),
                    ));
                }
                parsed
            }
            None => DEFAULT_TIMEOUT_SECS,
        };

        let max_tokens = match non_empty(lookup("LEG_MAX_TOKENS")) {
            Some(raw) => {
                let parsed = raw.parse::<u32>().map_err(|_| {
                    LegError::Config(format!(
                        "LEG_MAX_TOKENS must be a positive integer, got {raw:?}"
                    ))
                })?;
                if parsed == 0 {
                    return Err(LegError::Config(
                        "LEG_MAX_TOKENS must be greater than zero".to_string(),
                    ));
                }
                parsed
            }
            None => DEFAULT_MAX_TOKENS,
        };

        let system_prompt = resolve_system_prompt(non_empty(lookup("LEG_SYSTEM_PROMPT")))?;

        Ok(Self {
            credential,
            base_url,
            model,
            timeout: Duration::from_secs(timeout_secs),
            max_tokens,
            system_prompt,
        })
    }
}

/// Resolves the optional system prompt from the `LEG_SYSTEM_PROMPT` path.
///
/// `path` is the already-trimmed value of the variable (or `None` when unset or
/// blank). When present, the file at that path is read and its content returned;
/// a missing or unreadable file is a [`LegError::Config`] naming both the
/// variable and the path, so the command fails at startup before any network
/// call. An unset or blank variable yields `None`, preserving the no-system
/// behaviour.
fn resolve_system_prompt(path: Option<String>) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    std::fs::read_to_string(&path).map(Some).map_err(|err| {
        LegError::Config(format!(
            "LEG_SYSTEM_PROMPT points to a file that could not be read ({path}): {err}"
        ))
    })
}

/// Resolves the provider credential with the documented precedence.
///
/// Iterates the candidate `(variable, variant)` pairs in order; the first
/// variable whose lookup returns `Some` is the resolved credential. A
/// present-but-blank value is an error (and does *not* fall through to a later
/// candidate), because exporting a credential variable empty is almost always
/// a misconfiguration rather than an explicit "skip me" signal. If no
/// variable is present at all, that is also an error.
type CredentialBuilder = fn(String) -> Credential;

fn resolve_credential(lookup: &impl Fn(&str) -> Option<String>) -> Result<Credential> {
    let candidates: [(&str, CredentialBuilder); 3] = [
        ("ANTHROPIC_API_KEY", Credential::ApiKey),
        ("ANTHROPIC_AUTH_TOKEN", Credential::OAuth),
        ("CLAUDE_CODE_OAUTH_TOKEN", Credential::OAuth),
    ];

    for (var, make) in candidates {
        let Some(raw) = lookup(var) else {
            continue;
        };
        if raw.trim().is_empty() {
            return Err(LegError::Config(format!("{var} is set but empty")));
        }
        return Ok(make(raw));
    }

    Err(LegError::Config(
        "no Anthropic credential set: set one of ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN, or CLAUDE_CODE_OAUTH_TOKEN".to_string(),
    ))
}

/// Treats a present-but-blank value as absent so a defaulted variable that is
/// exported empty still falls back to its default.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn applies_defaults_when_only_api_key_present() {
        let cfg = LegConfig::from_lookup(lookup_from(&[("ANTHROPIC_API_KEY", "secret")]))
            .expect("config should load");
        assert_eq!(cfg.credential, Credential::ApiKey("secret".to_string()));
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(cfg.system_prompt, None);
    }

    #[test]
    fn overrides_are_honored() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("ANTHROPIC_BASE_URL", "https://proxy.example"),
            ("LEG_MODEL", "claude-opus-4-8"),
            ("LEG_TIMEOUT_SECS", "5"),
            ("LEG_MAX_TOKENS", "42"),
        ]))
        .expect("config should load");
        assert_eq!(cfg.base_url, "https://proxy.example");
        assert_eq!(cfg.model, "claude-opus-4-8");
        assert_eq!(cfg.timeout, Duration::from_secs(5));
        assert_eq!(cfg.max_tokens, 42);
    }

    #[test]
    fn oauth_token_precedence_over_missing_api_key() {
        let cfg = LegConfig::from_lookup(lookup_from(&[("ANTHROPIC_AUTH_TOKEN", "tok")]))
            .expect("config should load");
        assert_eq!(cfg.credential, Credential::OAuth("tok".to_string()));
    }

    #[test]
    fn api_key_takes_precedence_over_oauth_vars() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("ANTHROPIC_AUTH_TOKEN", "tok"),
        ]))
        .expect("config should load");
        assert_eq!(cfg.credential, Credential::ApiKey("secret".to_string()));
    }

    #[test]
    fn missing_credential_is_config_error() {
        let err = LegConfig::from_lookup(lookup_from(&[])).unwrap_err();
        assert!(matches!(err, LegError::Config(_)));
    }

    #[test]
    fn blank_credential_is_config_error_without_fallthrough() {
        let err = LegConfig::from_lookup(lookup_from(&[("ANTHROPIC_API_KEY", "   ")])).unwrap_err();
        assert!(matches!(err, LegError::Config(_)));
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let err = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("LEG_TIMEOUT_SECS", "0"),
        ]))
        .unwrap_err();
        assert!(matches!(err, LegError::Config(_)));
    }

    #[test]
    fn zero_max_tokens_is_rejected() {
        let err = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("LEG_MAX_TOKENS", "0"),
        ]))
        .unwrap_err();
        assert!(matches!(err, LegError::Config(_)));
    }

    #[test]
    fn blank_optional_vars_fall_back_to_defaults() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("LEG_MODEL", "  "),
            ("ANTHROPIC_BASE_URL", ""),
        ]))
        .expect("config should load");
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
    }
}
