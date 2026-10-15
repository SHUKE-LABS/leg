//! Environment-backed runtime configuration.
//!
//! [`LegConfig`] holds everything the single-turn first-reply path needs to
//! reach a provider. Loading is split into [`LegConfig::from_env`] (the real
//! entry point) and a pure [`LegConfig::from_lookup`] so parsing can be tested
//! deterministically without mutating the process environment.
//!
//! Authentication is modelled as a typed [`Credential`]. Anthropic keeps its
//! API-key/OAuth precedence, while both OpenAI wire protocols share
//! `OPENAI_API_KEY` and `OPENAI_BASE_URL`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::error::{LegError, Result};

/// Default base URL for the Claude-compatible Messages API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Default base URL for OpenAI-compatible APIs.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Default Anthropic model id when `LEG_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Default model id used by either OpenAI wire protocol when `LEG_MODEL` is unset.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4.1-mini";

/// Default request-phase and buffered-body timeout in seconds when
/// `LEG_TIMEOUT_SECS` is unset.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Default idle timeout in seconds for streaming responses.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 120;

/// Default `bash` command timeout in seconds when `LEG_BASH_TIMEOUT_SECS` is unset.
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;

/// Default `max_tokens` requested per reply when `LEG_MAX_TOKENS` is unset.
pub const DEFAULT_MAX_TOKENS: u32 = 32000;

/// Default number of retries for transient provider failures.
pub const DEFAULT_MAX_RETRIES: usize = 2;

/// Default base delay for provider retry backoff, in milliseconds.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 250;

/// The provider and wire protocol selected for the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic Messages API (`POST /v1/messages`).
    Anthropic,
    /// OpenAI Chat Completions API (`POST /v1/chat/completions`).
    OpenAiChatCompletions,
    /// OpenAI Responses API (`POST /v1/responses`).
    OpenAiResponses,
}

/// Authentication credential for the selected provider.
///
/// Variants map 1:1 onto the wire-format header the transport emits, including
/// OpenAI API keys sent with bearer authorization:
/// `ApiKey` -> `x-api-key`, `OAuth` -> `Authorization: Bearer <token>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// An Anthropic API key, sent as the `x-api-key` header.
    ApiKey(String),
    /// An OAuth bearer token, sent as the `Authorization: Bearer <token>`
    /// header.
    OAuth(String),
    /// An OpenAI-compatible API key, sent as an `Authorization: Bearer` token.
    Bearer(String),
}

/// Runtime configuration for leg's first-reply path.
#[derive(Debug, Clone)]
pub struct LegConfig {
    /// Selected provider and wire protocol, from `LEG_PROVIDER`.
    pub provider: Provider,
    /// Resolved provider credential.
    pub credential: Credential,
    /// Provider base URL, from `ANTHROPIC_BASE_URL` or `OPENAI_BASE_URL`.
    pub base_url: String,
    /// Model id to request. From `LEG_MODEL`, defaulting to a provider-specific
    /// model.
    pub model: String,
    /// Timeout for request setup phases and total buffered-body reads. Derived
    /// from `LEG_TIMEOUT_SECS`, defaulting to [`DEFAULT_TIMEOUT_SECS`]. Must be
    /// a positive integer; zero would fail request phases immediately.
    pub timeout: Duration,
    /// Maximum silence while reading a streamed response. Derived from
    /// `LEG_STREAM_IDLE_TIMEOUT_SECS`, defaulting to
    /// [`DEFAULT_STREAM_IDLE_TIMEOUT_SECS`]. Must be a positive integer.
    pub stream_idle_timeout: Duration,
    /// Default `bash` command timeout in seconds. Derived from
    /// `LEG_BASH_TIMEOUT_SECS`, defaulting to [`DEFAULT_BASH_TIMEOUT_SECS`].
    /// Must be a positive integer.
    pub bash_timeout_secs: u64,
    /// Maximum output tokens to request per reply. From `LEG_MAX_TOKENS`,
    /// defaulting to [`DEFAULT_MAX_TOKENS`]. Must be a positive integer; zero is
    /// rejected because the API rejects it.
    pub max_tokens: u32,
    /// Maximum retries per provider request. From `LEG_MAX_RETRIES`,
    /// defaulting to [`DEFAULT_MAX_RETRIES`]; zero disables retries.
    pub max_retries: usize,
    /// Base delay for retry backoff. From `LEG_RETRY_BASE_DELAY_MS`,
    /// defaulting to [`DEFAULT_RETRY_BASE_DELAY_MS`].
    pub retry_base_delay: Duration,
    /// Optional maximum tool-use rounds per turn. From `LEG_MAX_TOOL_ROUNDS`;
    /// unset or blank means unbounded, while a configured value must be a
    /// positive integer.
    pub max_tool_rounds: Option<usize>,
    /// Optional caller-provided system prompt. When `LEG_SYSTEM_PROMPT` names a
    /// readable file, this holds its content; the CLI appends the current
    /// project instructions before sending each provider turn. Unset or blank
    /// leaves this `None` and omits the field when no project instructions are
    /// present.
    pub system_prompt: Option<String>,
    /// Optional executable run before every tool dispatch. From
    /// `LEG_PRETOOL_HOOK`; unset or blank disables the hook.
    pub pre_tool_hook: Option<PathBuf>,
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
        let provider = parse_provider(non_empty(lookup("LEG_PROVIDER")))?;
        let (credential, base_url, default_model) = match provider {
            Provider::Anthropic => (
                resolve_credential(&lookup)?,
                non_empty(lookup("ANTHROPIC_BASE_URL"))
                    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
                DEFAULT_MODEL,
            ),
            Provider::OpenAiChatCompletions | Provider::OpenAiResponses => (
                resolve_openai_credential(&lookup)?,
                non_empty(lookup("OPENAI_BASE_URL"))
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
                DEFAULT_OPENAI_MODEL,
            ),
        };
        let model = non_empty(lookup("LEG_MODEL")).unwrap_or_else(|| default_model.to_string());

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

        let stream_idle_timeout_secs = match non_empty(lookup("LEG_STREAM_IDLE_TIMEOUT_SECS")) {
            Some(raw) => {
                let parsed = raw.parse::<u64>().map_err(|_| {
                    LegError::Config(format!(
                        "LEG_STREAM_IDLE_TIMEOUT_SECS must be a positive integer, got {raw:?}"
                    ))
                })?;
                if parsed == 0 {
                    return Err(LegError::Config(
                        "LEG_STREAM_IDLE_TIMEOUT_SECS must be greater than zero".to_string(),
                    ));
                }
                parsed
            }
            None => DEFAULT_STREAM_IDLE_TIMEOUT_SECS,
        };

        let bash_timeout_secs = match non_empty(lookup("LEG_BASH_TIMEOUT_SECS")) {
            Some(raw) => {
                let parsed = raw.parse::<u64>().map_err(|_| {
                    LegError::Config(format!(
                        "LEG_BASH_TIMEOUT_SECS must be a positive integer, got {raw:?}"
                    ))
                })?;
                if parsed == 0 {
                    return Err(LegError::Config(
                        "LEG_BASH_TIMEOUT_SECS must be greater than zero".to_string(),
                    ));
                }
                parsed
            }
            None => DEFAULT_BASH_TIMEOUT_SECS,
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

        let max_retries = match non_empty(lookup("LEG_MAX_RETRIES")) {
            Some(raw) => raw.parse::<usize>().map_err(|_| {
                LegError::Config(format!(
                    "LEG_MAX_RETRIES must be a non-negative integer, got {raw:?}"
                ))
            })?,
            None => DEFAULT_MAX_RETRIES,
        };

        let retry_base_delay_ms = match non_empty(lookup("LEG_RETRY_BASE_DELAY_MS")) {
            Some(raw) => raw.parse::<u64>().map_err(|_| {
                LegError::Config(format!(
                    "LEG_RETRY_BASE_DELAY_MS must be a non-negative integer, got {raw:?}"
                ))
            })?,
            None => DEFAULT_RETRY_BASE_DELAY_MS,
        };

        let max_tool_rounds = match non_empty(lookup("LEG_MAX_TOOL_ROUNDS")) {
            Some(raw) => {
                let parsed = raw.parse::<usize>().map_err(|_| {
                    LegError::Config(format!(
                        "LEG_MAX_TOOL_ROUNDS must be a positive integer, got {raw:?}"
                    ))
                })?;
                if parsed == 0 {
                    return Err(LegError::Config(
                        "LEG_MAX_TOOL_ROUNDS must be greater than zero".to_string(),
                    ));
                }
                Some(parsed)
            }
            None => None,
        };

        let system_prompt = resolve_system_prompt(non_empty(lookup("LEG_SYSTEM_PROMPT")))?;
        let pre_tool_hook = non_empty(lookup("LEG_PRETOOL_HOOK")).map(PathBuf::from);

        Ok(Self {
            provider,
            credential,
            base_url,
            model,
            timeout: Duration::from_secs(timeout_secs),
            stream_idle_timeout: Duration::from_secs(stream_idle_timeout_secs),
            bash_timeout_secs,
            max_tokens,
            max_retries,
            retry_base_delay: Duration::from_millis(retry_base_delay_ms),
            max_tool_rounds,
            system_prompt,
            pre_tool_hook,
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

/// Builds the system prompt for one provider turn.
///
/// The configured `LEG_SYSTEM_PROMPT` content is followed by repository
/// instructions discovered from the git root down to the current directory.
/// Discovery is deliberately performed for every turn so a resumed session
/// sees the files that exist now rather than a copy from its earlier run.
pub(crate) fn system_prompt_for_turn(base: Option<&str>) -> Result<Option<String>> {
    let cwd = std::env::current_dir().map_err(|err| {
        LegError::Config(format!(
            "failed to resolve the current working directory: {err}"
        ))
    })?;
    compose_system_prompt(base, instruction_directories(&cwd))
}

/// Composes a base prompt with the contents of the supplied instruction
/// directories. Keeping this separate makes ordering and empty-input behavior
/// testable without changing the process working directory.
fn compose_system_prompt(
    base: Option<&str>,
    directories: impl IntoIterator<Item = PathBuf>,
) -> Result<Option<String>> {
    let mut parts = Vec::new();
    if let Some(base) = base {
        parts.push(base.to_string());
    }

    for directory in directories {
        let path = directory.join("AGENTS.md");
        if !path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&path).map_err(|err| {
            LegError::Config(format!(
                "failed to read project instructions {path:?}: {err}"
            ))
        })?;
        parts.push(content);
    }

    Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
}

/// Returns the directories whose `AGENTS.md` files apply to `cwd`, ordered
/// from the repository root to the nearest directory. A non-repository
/// working directory contributes only itself.
fn instruction_directories(cwd: &Path) -> Vec<PathBuf> {
    // Git for Windows may print a root with different separators, casing, or
    // a path-prefix form than `current_dir`. Canonicalizing both sides makes
    // the ancestor comparison semantic instead of dependent on that spelling.
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Some(root) = git_root(&cwd) else {
        return vec![cwd];
    };
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let mut directories = Vec::new();
    let mut current = cwd.as_path();
    loop {
        directories.push(current.to_path_buf());
        if current == root {
            directories.reverse();
            return directories;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }

    // A defensive fallback for an unexpected path representation from git:
    // never inspect directories outside the caller's working tree.
    vec![cwd.to_path_buf()]
}

/// Resolves the git root for `cwd`. A failed git probe means that `cwd` is
/// outside a repository, which is the normal fallback for temporary workspaces
/// and callers that do not have git installed.
fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let root = PathBuf::from(raw.trim());
    if root.is_absolute() {
        Some(root)
    } else {
        Some(cwd.join(root))
    }
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

static ANTHROPIC_CREDENTIAL_CANDIDATES: [(&str, CredentialBuilder); 3] = [
    ("ANTHROPIC_API_KEY", Credential::ApiKey),
    ("ANTHROPIC_AUTH_TOKEN", Credential::OAuth),
    ("CLAUDE_CODE_OAUTH_TOKEN", Credential::OAuth),
];

static CREDENTIAL_ENV_VARS: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
];

pub(crate) fn credential_env_vars() -> impl Iterator<Item = &'static str> {
    CREDENTIAL_ENV_VARS.iter().copied()
}

fn parse_provider(value: Option<String>) -> Result<Provider> {
    match value.as_deref() {
        None | Some("anthropic") => Ok(Provider::Anthropic),
        Some("openai-chat-completions") => Ok(Provider::OpenAiChatCompletions),
        Some("openai-responses") => Ok(Provider::OpenAiResponses),
        Some(value) => Err(LegError::Config(format!(
            "LEG_PROVIDER must be one of anthropic, openai-chat-completions, or openai-responses, got {value:?}"
        ))),
    }
}

fn resolve_credential(lookup: &impl Fn(&str) -> Option<String>) -> Result<Credential> {
    for &(var, make) in &ANTHROPIC_CREDENTIAL_CANDIDATES {
        let Some(raw) = lookup(var) else {
            continue;
        };
        if raw.trim().is_empty() {
            return Err(LegError::Config(format!("{var} is set but empty")));
        }
        let credential = make(raw);
        if matches!(
            &credential,
            Credential::OAuth(token) if token.trim_start().starts_with("sk-ant-oat")
        ) {
            return Err(LegError::Config(format!(
                "{var} contains a Claude subscription OAuth token (from claude setup-token); these tokens are unsupported outside Claude Code. Set ANTHROPIC_API_KEY to an Anthropic Console API key instead."
            )));
        }
        return Ok(credential);
    }

    let candidates = ANTHROPIC_CREDENTIAL_CANDIDATES
        .iter()
        .map(|(var, _)| *var)
        .collect::<Vec<_>>()
        .join(", ");
    Err(LegError::Config(format!(
        "no Anthropic credential set: set one of {candidates}"
    )))
}

fn resolve_openai_credential(lookup: &impl Fn(&str) -> Option<String>) -> Result<Credential> {
    let Some(raw) = lookup("OPENAI_API_KEY") else {
        return Err(LegError::Config(
            "no OpenAI credential set: set OPENAI_API_KEY".to_string(),
        ));
    };
    if raw.trim().is_empty() {
        return Err(LegError::Config(
            "OPENAI_API_KEY is set but empty".to_string(),
        ));
    }
    Ok(Credential::Bearer(raw))
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
    fn composes_system_prompt_in_root_to_nearest_order() {
        let root = std::env::temp_dir().join(format!(
            "leg-config-agents-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("create instruction directories");
        std::fs::write(root.join("AGENTS.md"), "root rules").expect("write root instructions");
        std::fs::write(nested.join("AGENTS.md"), "nested rules")
            .expect("write nested instructions");

        let prompt = compose_system_prompt(Some("caller rules"), [root.clone(), nested.clone()])
            .expect("compose system prompt")
            .expect("prompt exists");

        assert_eq!(prompt, "caller rules\n\nroot rules\n\nnested rules");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_repository_discovery_reads_only_the_working_directory() {
        let parent = std::env::temp_dir().join(format!(
            "leg-config-agents-nongit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let cwd = parent.join("workspace");
        std::fs::create_dir_all(&cwd).expect("create non-repository workspace");
        std::fs::write(parent.join("AGENTS.md"), "parent rules")
            .expect("write parent instructions");
        std::fs::write(cwd.join("AGENTS.md"), "working directory rules")
            .expect("write working-directory instructions");

        assert!(
            git_root(&cwd).is_none(),
            "temporary workspace must be non-git"
        );
        let prompt = compose_system_prompt(None, instruction_directories(&cwd))
            .expect("compose system prompt")
            .expect("working-directory prompt exists");

        assert_eq!(prompt, "working directory rules");
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn repository_discovery_reaches_the_git_root() {
        let root = std::env::temp_dir().join(format!(
            "leg-config-agents-git-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("create repository workspace");
        let git_status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .expect("run git init");
        assert!(git_status.success(), "git init failed");
        std::fs::write(root.join("AGENTS.md"), "root rules").expect("write root instructions");
        std::fs::write(nested.join("AGENTS.md"), "nested rules")
            .expect("write nested instructions");

        let prompt = compose_system_prompt(None, instruction_directories(&nested))
            .expect("compose system prompt")
            .expect("repository prompt exists");

        assert_eq!(prompt, "root rules\n\nnested rules");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn no_agents_preserves_base_or_omits_system_prompt() {
        assert_eq!(
            compose_system_prompt(None, Vec::<PathBuf>::new()).unwrap(),
            None
        );
        assert_eq!(
            compose_system_prompt(Some("caller rules"), Vec::<PathBuf>::new()).unwrap(),
            Some("caller rules".to_string())
        );
    }

    #[test]
    fn applies_defaults_when_only_api_key_present() {
        let cfg = LegConfig::from_lookup(lookup_from(&[("ANTHROPIC_API_KEY", "secret")]))
            .expect("config should load");
        assert_eq!(cfg.provider, Provider::Anthropic);
        assert_eq!(cfg.credential, Credential::ApiKey("secret".to_string()));
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(
            cfg.stream_idle_timeout,
            Duration::from_secs(DEFAULT_STREAM_IDLE_TIMEOUT_SECS)
        );
        assert_eq!(cfg.bash_timeout_secs, DEFAULT_BASH_TIMEOUT_SECS);
        assert_eq!(cfg.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(cfg.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(
            cfg.retry_base_delay,
            Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS)
        );
        assert_eq!(cfg.max_tool_rounds, None);
        assert_eq!(cfg.system_prompt, None);
        assert_eq!(cfg.pre_tool_hook, None);
    }

    #[test]
    fn default_max_tokens_is_32000() {
        assert_eq!(DEFAULT_MAX_TOKENS, 32000);
    }

    #[test]
    fn openai_protocols_share_credentials_base_url_and_model_settings() {
        for (provider, expected) in [
            ("openai-chat-completions", Provider::OpenAiChatCompletions),
            ("openai-responses", Provider::OpenAiResponses),
        ] {
            let cfg = LegConfig::from_lookup(lookup_from(&[
                ("LEG_PROVIDER", provider),
                ("OPENAI_API_KEY", "openai-secret"),
                ("OPENAI_BASE_URL", "https://proxy.example/v1"),
                ("LEG_MODEL", "custom-model"),
            ]))
            .expect("OpenAI config should load");

            assert_eq!(cfg.provider, expected);
            assert_eq!(
                cfg.credential,
                Credential::Bearer("openai-secret".to_string())
            );
            assert_eq!(cfg.base_url, "https://proxy.example/v1");
            assert_eq!(cfg.model, "custom-model");
        }
    }

    #[test]
    fn openai_protocols_use_their_shared_defaults() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("LEG_PROVIDER", "openai-responses"),
            ("OPENAI_API_KEY", "openai-secret"),
        ]))
        .expect("OpenAI config should load");

        assert_eq!(cfg.provider, Provider::OpenAiResponses);
        assert_eq!(cfg.base_url, DEFAULT_OPENAI_BASE_URL);
        assert_eq!(cfg.model, DEFAULT_OPENAI_MODEL);
    }

    #[test]
    fn rejects_unknown_provider_and_missing_or_blank_openai_credentials() {
        let invalid_provider = LegConfig::from_lookup(lookup_from(&[
            ("LEG_PROVIDER", "other"),
            ("ANTHROPIC_API_KEY", "secret"),
        ]))
        .unwrap_err();
        assert!(matches!(invalid_provider, LegError::Config(_)));
        assert!(invalid_provider.to_string().contains("LEG_PROVIDER"));

        for pairs in [
            vec![("LEG_PROVIDER", "openai-responses")],
            vec![
                ("LEG_PROVIDER", "openai-chat-completions"),
                ("OPENAI_API_KEY", "  "),
            ],
        ] {
            let err = LegConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
            assert!(matches!(err, LegError::Config(_)));
            assert!(err.to_string().contains("OPENAI_API_KEY"));
        }
    }

    #[test]
    fn overrides_are_honored() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("ANTHROPIC_BASE_URL", "https://proxy.example"),
            ("LEG_MODEL", "claude-opus-4-8"),
            ("LEG_TIMEOUT_SECS", "5"),
            ("LEG_STREAM_IDLE_TIMEOUT_SECS", "8"),
            ("LEG_BASH_TIMEOUT_SECS", "30"),
            ("LEG_MAX_TOKENS", "42"),
            ("LEG_MAX_RETRIES", "5"),
            ("LEG_RETRY_BASE_DELAY_MS", "300"),
            ("LEG_MAX_TOOL_ROUNDS", "3"),
            ("LEG_PRETOOL_HOOK", "/tmp/pre-tool-hook"),
        ]))
        .expect("config should load");
        assert_eq!(cfg.base_url, "https://proxy.example");
        assert_eq!(cfg.model, "claude-opus-4-8");
        assert_eq!(cfg.timeout, Duration::from_secs(5));
        assert_eq!(cfg.stream_idle_timeout, Duration::from_secs(8));
        assert_eq!(cfg.bash_timeout_secs, 30);
        assert_eq!(cfg.max_tokens, 42);
        assert_eq!(cfg.max_retries, 5);
        assert_eq!(cfg.retry_base_delay, Duration::from_millis(300));
        assert_eq!(cfg.max_tool_rounds, Some(3));
        assert_eq!(
            cfg.pre_tool_hook.as_deref(),
            Some(std::path::Path::new("/tmp/pre-tool-hook"))
        );
    }

    #[test]
    fn oauth_token_precedence_over_missing_api_key() {
        let cfg = LegConfig::from_lookup(lookup_from(&[("ANTHROPIC_AUTH_TOKEN", "tok")]))
            .expect("config should load");
        assert_eq!(cfg.credential, Credential::OAuth("tok".to_string()));
    }

    #[test]
    fn rejects_subscription_oauth_tokens() {
        for (var, token) in [
            ("ANTHROPIC_AUTH_TOKEN", "sk-ant-oat01-x"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-x"),
            ("ANTHROPIC_AUTH_TOKEN", " \tsk-ant-oat01-x"),
        ] {
            let err = LegConfig::from_lookup(lookup_from(&[(var, token)])).unwrap_err();
            let message = match err {
                LegError::Config(message) => message,
                other => panic!("expected configuration error, got {other:?}"),
            };
            assert!(message.contains(var));
            assert!(message.contains("ANTHROPIC_API_KEY"));
            assert!(message.contains("Claude subscription OAuth token"));
            assert!(message.contains("claude setup-token"));
            assert!(message.contains("unsupported outside Claude Code"));
        }
    }

    #[test]
    fn api_key_takes_precedence_over_subscription_oauth_token() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("ANTHROPIC_AUTH_TOKEN", "sk-ant-oat01-x"),
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
    fn zero_or_non_integer_stream_idle_timeout_is_rejected() {
        for raw in ["0", "abc"] {
            let err = LegConfig::from_lookup(lookup_from(&[
                ("ANTHROPIC_API_KEY", "secret"),
                ("LEG_STREAM_IDLE_TIMEOUT_SECS", raw),
            ]))
            .unwrap_err();
            assert!(matches!(err, LegError::Config(_)), "{raw}");
            assert!(err.to_string().contains("LEG_STREAM_IDLE_TIMEOUT_SECS"));
        }
    }

    #[test]
    fn zero_or_non_integer_bash_timeout_is_rejected() {
        for raw in ["0", "abc"] {
            let err = LegConfig::from_lookup(lookup_from(&[
                ("ANTHROPIC_API_KEY", "secret"),
                ("LEG_BASH_TIMEOUT_SECS", raw),
            ]))
            .unwrap_err();
            assert!(matches!(err, LegError::Config(_)), "{raw}");
        }
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
    fn retry_count_zero_is_valid_and_invalid_values_are_rejected() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("LEG_MAX_RETRIES", "0"),
            ("LEG_RETRY_BASE_DELAY_MS", "0"),
        ]))
        .expect("zero retries and delay should be valid");
        assert_eq!(cfg.max_retries, 0);
        assert_eq!(cfg.retry_base_delay, Duration::ZERO);

        for (var, value) in [
            ("LEG_MAX_RETRIES", "-1"),
            ("LEG_MAX_RETRIES", "many"),
            ("LEG_RETRY_BASE_DELAY_MS", "-1"),
            ("LEG_RETRY_BASE_DELAY_MS", "slow"),
        ] {
            let err = LegConfig::from_lookup(lookup_from(&[
                ("ANTHROPIC_API_KEY", "secret"),
                (var, value),
            ]))
            .unwrap_err();
            assert!(matches!(err, LegError::Config(_)), "{var}={value}");
        }
    }

    #[test]
    fn zero_or_non_integer_max_tool_rounds_are_rejected() {
        for raw in ["0", "abc"] {
            let err = LegConfig::from_lookup(lookup_from(&[
                ("ANTHROPIC_API_KEY", "secret"),
                ("LEG_MAX_TOOL_ROUNDS", raw),
            ]))
            .unwrap_err();
            assert!(matches!(err, LegError::Config(_)), "{raw}");
        }
    }

    #[test]
    fn blank_optional_vars_fall_back_to_defaults() {
        let cfg = LegConfig::from_lookup(lookup_from(&[
            ("ANTHROPIC_API_KEY", "secret"),
            ("LEG_MODEL", "  "),
            ("ANTHROPIC_BASE_URL", ""),
            ("LEG_BASH_TIMEOUT_SECS", "  "),
            ("LEG_MAX_TOOL_ROUNDS", "  "),
            ("LEG_MAX_RETRIES", "  "),
            ("LEG_RETRY_BASE_DELAY_MS", "  "),
            ("LEG_PRETOOL_HOOK", "  "),
        ]))
        .expect("config should load");
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
        assert_eq!(cfg.bash_timeout_secs, DEFAULT_BASH_TIMEOUT_SECS);
        assert_eq!(cfg.max_tool_rounds, None);
        assert_eq!(cfg.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(
            cfg.retry_base_delay,
            Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS)
        );
        assert_eq!(cfg.pre_tool_hook, None);
    }
}
