//! Provider-neutral reasoning configuration and model capability mapping.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::Provider;
use crate::error::{LegError, Result};

/// The minimum answer room retained when a provider shares its output ceiling
/// between thinking and the visible answer.
pub(crate) const MIN_ANSWER_TOKENS: u32 = 1024;

const ALLOWED_LEVELS: &str = "off, minimal, low, medium, high, xhigh, max";

/// The provider-neutral thinking levels accepted by `LEG_THINKING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let level = match raw {
            "off" => Self::Off,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => {
                return Err(LegError::Config(format!(
                    "LEG_THINKING must be one of {ALLOWED_LEVELS}, got {raw:?}"
                )));
            }
        };
        Ok(level)
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    const fn budget(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Minimal => 1024,
            Self::Low => 2048,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::XHigh => 32768,
            Self::Max => 65536,
        }
    }
}

/// How a provider accepts a resolved level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingEncoding {
    Effort,
    Budget,
}

/// The resolved thinking request to add to a provider body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThinkingRequest {
    Effort(String),
    Budget(u32),
}

/// Thinking settings resolved once while loading `LegConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThinkingConfig {
    requested: ThinkingLevel,
    encoding: Option<ThinkingEncoding>,
    supported: Vec<ThinkingLevel>,
    /// Explicit provider values from `LEG_THINKING_MAP`. `None` means that the
    /// corresponding level is deliberately unsupported.
    overrides: BTreeMap<ThinkingLevel, Option<String>>,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ThinkingConfig {
    pub(crate) fn disabled() -> Self {
        Self {
            requested: ThinkingLevel::Off,
            encoding: None,
            supported: Vec::new(),
            overrides: BTreeMap::new(),
        }
    }

    pub(crate) fn resolve(
        provider: Provider,
        model: &str,
        raw_level: Option<String>,
        raw_map: Option<String>,
    ) -> Result<Self> {
        let requested = ThinkingLevel::parse(raw_level.as_deref().unwrap_or("high"))?;
        let overrides = parse_map(raw_map)?;
        let capability = capability_for(provider, model);
        if capability.is_some_and(|capability| capability.encoding == ThinkingEncoding::Budget) {
            validate_budget_overrides(&overrides)?;
        }
        if !requested.is_enabled() {
            return Ok(Self {
                requested,
                encoding: None,
                supported: Vec::new(),
                overrides,
            });
        }

        let has_override_value = overrides.values().any(Option::is_some);
        let encoding = capability
            .map(|capability| capability.encoding)
            .or_else(|| has_override_value.then_some(ThinkingEncoding::Effort));

        let supported: Vec<ThinkingLevel> = if let Some(capability) = capability {
            capability
                .supported
                .iter()
                .copied()
                .filter(|level| !matches!(overrides.get(level), Some(None)))
                .collect()
        } else {
            overrides
                .iter()
                .filter_map(|(level, value)| {
                    level
                        .is_enabled()
                        .then_some(value.as_ref().map(|_| *level))
                        .flatten()
                })
                .collect()
        };

        if encoding.is_none() || supported.is_empty() {
            eprintln!("{}", unknown_model_warning(model));
        }

        Ok(Self {
            requested,
            encoding,
            supported,
            overrides,
        })
    }

    /// Resolves the request for a provider call with this output-token limit.
    pub(crate) fn request(&self, max_tokens: u32) -> Option<ThinkingRequest> {
        let level = self
            .supported
            .iter()
            .rev()
            .copied()
            .find(|level| *level <= self.requested)?;
        match self.encoding? {
            ThinkingEncoding::Effort => Some(ThinkingRequest::Effort(
                self.overrides
                    .get(&level)
                    .and_then(|value| value.clone())
                    .unwrap_or_else(|| level.as_str().to_string()),
            )),
            ThinkingEncoding::Budget => {
                let requested_budget = self
                    .overrides
                    .get(&level)
                    .and_then(|value| value.as_deref())
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or_else(|| level.budget());
                let answer_room = max_tokens.saturating_sub(MIN_ANSWER_TOKENS);
                let budget = requested_budget.min(answer_room);
                (budget >= MIN_ANSWER_TOKENS).then_some(ThinkingRequest::Budget(budget))
            }
        }
    }

    #[cfg(test)]
    fn requested(&self) -> ThinkingLevel {
        self.requested
    }
}

#[derive(Debug)]
struct ModelCapability {
    provider: Provider,
    prefix: &'static str,
    encoding: ThinkingEncoding,
    supported: &'static [ThinkingLevel],
}

const CLAUDE_EFFORT_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const CLAUDE_4_6_EFFORT_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Max,
];
const OPENAI_EFFORT_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
];
const BUDGET_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
];

/// Built-in model capability table. Prefixes accept dated model aliases and
/// provider-compatible suffixes without attempting to infer capabilities from
/// arbitrary unknown ids.
const MODEL_CAPABILITIES: &[ModelCapability] = &[
    // Anthropic models with adaptive/effort-style thinking.
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-opus-5",
        encoding: ThinkingEncoding::Effort,
        supported: CLAUDE_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-sonnet-5",
        encoding: ThinkingEncoding::Effort,
        supported: CLAUDE_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-haiku-5",
        encoding: ThinkingEncoding::Effort,
        supported: CLAUDE_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-opus-4-6",
        encoding: ThinkingEncoding::Effort,
        supported: CLAUDE_4_6_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-sonnet-4-6",
        encoding: ThinkingEncoding::Effort,
        supported: CLAUDE_4_6_EFFORT_LEVELS,
    },
    // Older Claude models use the extended-thinking budget shape.
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-3-7-sonnet",
        encoding: ThinkingEncoding::Budget,
        supported: BUDGET_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-opus-4-5",
        encoding: ThinkingEncoding::Budget,
        supported: BUDGET_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-sonnet-4-5",
        encoding: ThinkingEncoding::Budget,
        supported: BUDGET_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-opus-4",
        encoding: ThinkingEncoding::Budget,
        supported: BUDGET_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-sonnet-4",
        encoding: ThinkingEncoding::Budget,
        supported: BUDGET_LEVELS,
    },
    ModelCapability {
        provider: Provider::Anthropic,
        prefix: "claude-haiku-4",
        encoding: ThinkingEncoding::Budget,
        supported: BUDGET_LEVELS,
    },
    // OpenAI reasoning models expose the effort string directly.
    ModelCapability {
        provider: Provider::OpenAiResponses,
        prefix: "o1",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiResponses,
        prefix: "o3",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiResponses,
        prefix: "o4",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiResponses,
        prefix: "gpt-5",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiChatCompletions,
        prefix: "o1",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiChatCompletions,
        prefix: "o3",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiChatCompletions,
        prefix: "o4",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
    ModelCapability {
        provider: Provider::OpenAiChatCompletions,
        prefix: "gpt-5",
        encoding: ThinkingEncoding::Effort,
        supported: OPENAI_EFFORT_LEVELS,
    },
];

fn capability_for(provider: Provider, model: &str) -> Option<&'static ModelCapability> {
    MODEL_CAPABILITIES
        .iter()
        .find(|capability| capability.provider == provider && model.starts_with(capability.prefix))
}

fn validate_budget_overrides(overrides: &BTreeMap<ThinkingLevel, Option<String>>) -> Result<()> {
    for (level, value) in overrides {
        let Some(value) = value else {
            continue;
        };
        let budget = value.parse::<u32>().map_err(|_| {
            LegError::Config(format!(
                "LEG_THINKING_MAP value for {:?} must be a decimal budget of at least {MIN_ANSWER_TOKENS} for budget-style models",
                level.as_str()
            ))
        })?;
        if budget < MIN_ANSWER_TOKENS {
            return Err(LegError::Config(format!(
                "LEG_THINKING_MAP value for {:?} must be a decimal budget of at least {MIN_ANSWER_TOKENS} for budget-style models",
                level.as_str()
            )));
        }
    }
    Ok(())
}

fn parse_map(raw: Option<String>) -> Result<BTreeMap<ThinkingLevel, Option<String>>> {
    let Some(raw) = raw else {
        return Ok(BTreeMap::new());
    };
    let value: Value = serde_json::from_str(&raw).map_err(|error| {
        LegError::Config(format!("LEG_THINKING_MAP must be a JSON object: {error}"))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| LegError::Config("LEG_THINKING_MAP must be a JSON object".to_string()))?;
    let mut parsed = BTreeMap::new();
    for (key, value) in object {
        let level = ThinkingLevel::parse(key).map_err(|_| {
            LegError::Config(format!(
                "LEG_THINKING_MAP contains invalid level {key:?}; allowed levels are {ALLOWED_LEVELS}"
            ))
        })?;
        let mapped = match value {
            Value::Null => None,
            Value::String(value) => Some(value.clone()),
            _ => {
                return Err(LegError::Config(format!(
                    "LEG_THINKING_MAP value for {key:?} must be a string or null"
                )));
            }
        };
        parsed.insert(level, mapped);
    }
    Ok(parsed)
}

fn unknown_model_warning(model: &str) -> String {
    format!(
        "warning: model {model:?} has no known thinking capability; set LEG_THINKING_MAP to configure it"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_levels_and_rejects_unknown_values() {
        for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert!(ThinkingLevel::parse(level).is_ok(), "{level}");
        }
        let error = ThinkingLevel::parse("extreme").unwrap_err();
        assert!(error.to_string().contains(ALLOWED_LEVELS));
    }

    #[test]
    fn defaults_to_high_for_a_known_claude_model() {
        let config = ThinkingConfig::resolve(Provider::Anthropic, "claude-sonnet-4-6", None, None)
            .expect("thinking config");
        assert_eq!(config.requested(), ThinkingLevel::High);
        assert_eq!(
            config.request(32_000),
            Some(ThinkingRequest::Effort("high".to_string()))
        );
    }

    #[test]
    fn xhigh_clamps_to_high_for_claude_4_6_models() {
        for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            let config = ThinkingConfig::resolve(
                Provider::Anthropic,
                model,
                Some("xhigh".to_string()),
                None,
            )
            .expect("thinking config");
            assert_eq!(
                config.request(32_000),
                Some(ThinkingRequest::Effort("high".to_string())),
                "model: {model}"
            );
        }
    }

    #[test]
    fn clamps_max_to_high_for_openai_effort_models() {
        let config = ThinkingConfig::resolve(
            Provider::OpenAiResponses,
            "o3-mini",
            Some("max".to_string()),
            None,
        )
        .expect("thinking config");
        assert_eq!(
            config.request(32_000),
            Some(ThinkingRequest::Effort("high".to_string()))
        );
    }

    #[test]
    fn clamps_budget_to_answer_room() {
        let config = ThinkingConfig::resolve(
            Provider::Anthropic,
            "claude-3-7-sonnet-20250219",
            Some("high".to_string()),
            None,
        )
        .expect("thinking config");
        assert_eq!(config.request(4096), Some(ThinkingRequest::Budget(3072)));
    }

    #[test]
    fn unsupported_legacy_claude_model_does_not_enable_thinking() {
        let config = ThinkingConfig::resolve(
            Provider::Anthropic,
            "claude-3-5-sonnet-20241022",
            Some("high".to_string()),
            None,
        )
        .expect("thinking config");
        assert_eq!(config.request(32_000), None);
    }

    #[test]
    fn override_maps_unknown_model_to_effort_value() {
        let config = ThinkingConfig::resolve(
            Provider::OpenAiResponses,
            "vendor-reasoning-model",
            Some("high".to_string()),
            Some(r#"{"high":"medium"}"#.to_string()),
        )
        .expect("thinking config");
        assert_eq!(
            config.request(32_000),
            Some(ThinkingRequest::Effort("medium".to_string()))
        );
    }

    #[test]
    fn unknown_warning_names_model_and_override_variable_once() {
        let warning = unknown_model_warning("vendor-unknown");
        assert!(warning.contains("vendor-unknown"));
        assert!(warning.contains("LEG_THINKING_MAP"));
        assert_eq!(warning.matches("warning:").count(), 1);
    }

    #[test]
    fn parses_null_map_values_as_unsupported() {
        let config = ThinkingConfig::resolve(
            Provider::OpenAiResponses,
            "o3-mini",
            Some("high".to_string()),
            Some(r#"{"high":null,"medium":"medium"}"#.to_string()),
        )
        .expect("thinking config");
        assert_eq!(
            config.request(32_000),
            Some(ThinkingRequest::Effort("medium".to_string()))
        );
    }
}
