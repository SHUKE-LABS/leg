//! Error types shared across leg's runtime surfaces.
//!
//! Configuration failures and the provider transport's failure modes are
//! modelled as distinct variants so callers can react to them explicitly. The
//! Messages client maps HTTP and decode failures onto these variants rather
//! than collapsing everything into a single opaque error.

use std::fmt;

/// Convenience alias for results produced by leg's runtime.
pub type Result<T> = std::result::Result<T, LegError>;

/// A Unix signal that interrupts an active turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptSignal {
    /// `SIGINT`.
    Interrupt,
    /// `SIGTERM`.
    Terminate,
}

impl InterruptSignal {
    /// The conventional shell exit status for this signal.
    pub const fn exit_code(self) -> u8 {
        match self {
            InterruptSignal::Interrupt => 130,
            InterruptSignal::Terminate => 143,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            InterruptSignal::Interrupt => "SIGINT",
            InterruptSignal::Terminate => "SIGTERM",
        }
    }
}

/// A guest inspection or provisioning failure.
///
/// The reason is a stable machine-readable string, the message says what
/// went wrong, and the remediation says what the host can do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFailure {
    /// The stable, machine-readable failure reason.
    pub reason: &'static str,
    /// The human-readable failure detail.
    pub message: String,
    /// The remediation hint for the host.
    pub remediation: String,
}

/// Top-level error type for leg.
#[derive(Debug)]
pub enum LegError {
    /// A command-line argument was missing, unrecognised, or malformed. Carries
    /// a human-readable explanation plus the one-line usage summary.
    Usage(String),
    /// Configuration could not be loaded or was invalid (e.g. a missing or
    /// malformed environment variable).
    Config(String),
    /// A transient connection-level failure before an HTTP response arrived.
    Transport(String),
    /// A non-retryable HTTP setup or protocol failure before an HTTP response.
    NonRetryableTransport(String),
    /// A failure while reading a response body; retrying could duplicate a
    /// request after the provider has already started replying.
    ResponseRead(String),
    /// The provider returned a stream-level error event after the HTTP
    /// response had started.
    ProviderStream {
        /// The optional provider error type.
        error_type: Option<String>,
        /// The provider's error message.
        message: String,
    },
    /// The provider rejected the credentials (HTTP 401). The message includes
    /// the provider error type when supplied.
    Auth(String),
    /// The provider rate-limited the request (HTTP 429 or `rate_limit_error`).
    /// The message includes the provider error type when supplied.
    RateLimited(String),
    /// The provider returned a server-side failure (HTTP 5xx).
    Server {
        /// The HTTP status code.
        status: u16,
        /// The optional provider error type from `error.type`.
        error_type: Option<String>,
        /// The provider's error message, or the raw body when it could not be
        /// parsed.
        message: String,
    },
    /// The provider returned some other non-success status (e.g. 400 Bad
    /// Request) that does not map to a more specific variant.
    Api {
        /// The HTTP status code.
        status: u16,
        /// The optional provider error type from `error.type`.
        error_type: Option<String>,
        /// The provider's error message, or the raw body when it could not be
        /// parsed.
        message: String,
    },
    /// A 2xx response could not be decoded into an [`AssistantReply`], because
    /// the body was malformed, partial, or carried no assistant text.
    ///
    /// [`AssistantReply`]: crate::model::AssistantReply
    Decode(String),
    /// A provider stopped at the output-token limit before producing usable
    /// assistant text or a tool call.
    TokenLimit {
        /// The configured maximum output-token count.
        max_tokens: u32,
        /// The provider's raw terminal reason.
        stop_reason: String,
    },
    /// The active conversation cannot fit in the configured provider context
    /// window without dropping an in-flight turn or tool result.
    ContextWindow(String),
    /// The selected provider/model does not accept image input. This is a
    /// stable, non-retryable failure; the caller may switch models or remove
    /// the image and deliberately resubmit the preserved turn.
    ImageInputUnsupported {
        /// Stable provider protocol name.
        provider: String,
        /// Configured model id.
        model: String,
    },
    /// A local I/O operation failed.
    Io(String),
    /// The `leg install` command could not materialize the native binaries.
    Install(String),
    /// A guest inspection or provisioning request failed. The failure carries a
    /// machine-readable reason and the remediation a host can act on.
    Guest(GuestFailure),
    /// The active turn was interrupted by a Unix signal.
    Interrupted {
        /// The signal that requested the interruption.
        signal: InterruptSignal,
    },
    /// A JSONL exchange trail (`LEG_EVENT_LOG` or a `--resume` file) could not
    /// be parsed: a malformed line, or a known event missing required fields.
    Log(String),
    /// A named `exchange` session has no stored trail for the requested id.
    SessionNotFound(String),
    /// A provider turn returned a delivered error response.
    TurnFailure {
        /// The `baton.message/v1` response kind.
        message_kind: String,
        /// The response's human-readable failure detail.
        message: String,
    },
}

impl LegError {
    /// Builds the decode failure used when a provider exhausts the configured
    /// output-token limit before returning usable assistant content.
    pub(crate) fn token_limit_reply(max_tokens: u32, stop_reason: &str) -> Self {
        LegError::TokenLimit {
            max_tokens,
            stop_reason: stop_reason.to_string(),
        }
    }

    /// A stable, machine-readable class for this error.
    ///
    /// Used by the delivered-error envelope so consumers can branch on the
    /// failure kind without parsing the human-readable message.
    pub fn kind(&self) -> &'static str {
        match self {
            LegError::Usage(_) => "usage",
            LegError::Config(_) => "config",
            LegError::Transport(_) => "transport",
            LegError::NonRetryableTransport(_) => "transport",
            LegError::ResponseRead(_) => "transport",
            LegError::ProviderStream { .. } => "provider_stream",
            LegError::Auth(_) => "auth",
            LegError::RateLimited(_) => "rate_limited",
            LegError::Server { .. } => "server",
            LegError::Api { .. } => "api",
            LegError::Decode(_) | LegError::TokenLimit { .. } => "decode",
            LegError::ContextWindow(_) => "context_window",
            LegError::ImageInputUnsupported { .. } => "image_input_unsupported",
            LegError::Io(_) => "io",
            LegError::Install(_) => "install",
            LegError::Guest(_) => "guest",
            LegError::Interrupted { .. } => "interrupted",
            LegError::Log(_) => "log",
            LegError::SessionNotFound(_) => "session_not_found",
            LegError::TurnFailure { .. } => "turn_failure",
        }
    }

    /// Returns a provider stop reason when the error was caused by a
    /// token-limited reply.
    pub(crate) fn stop_reason(&self) -> Option<&str> {
        match self {
            LegError::TokenLimit { stop_reason, .. } => Some(stop_reason),
            _ => None,
        }
    }

    /// Whether this provider rejection identifies an input context overflow.
    pub(crate) fn is_context_window_rejection(&self) -> bool {
        let message = match self {
            LegError::Api {
                error_type,
                message,
                ..
            }
            | LegError::Server {
                error_type,
                message,
                ..
            } => format!("{} {message}", error_type.as_deref().unwrap_or_default()),
            LegError::ProviderStream {
                error_type,
                message,
            } => {
                format!("{} {message}", error_type.as_deref().unwrap_or_default())
            }
            _ => return false,
        };
        let message = message.to_ascii_lowercase();
        message.contains("context window")
            || message.contains("context length")
            || message.contains("prompt is too long")
            || message.contains("input is too long")
            || message.contains("maximum context")
            || message.contains("context_length_exceeded")
            || message.contains("reduce the length of the input")
    }

    /// Reclassifies an explicit provider modality rejection without retaining
    /// provider text that could contain a credential, path, or uploaded data.
    pub(crate) fn classify_image_input_rejection(
        self,
        provider: &str,
        model: &str,
        has_image: bool,
    ) -> Self {
        if !has_image {
            return self;
        }
        let LegError::Api {
            status,
            error_type,
            message,
        } = &self
        else {
            return self;
        };
        if !(400..500).contains(status) || !is_image_input_rejection(error_type.as_deref(), message)
        {
            return self;
        }
        LegError::ImageInputUnsupported {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    /// Reclassifies a provider response before a retry wrapper can discard the
    /// response body. This also covers a provider that uses a retryable HTTP
    /// status for a deterministic modality rejection.
    pub(crate) fn classify_image_input_response(
        self,
        provider: &str,
        model: &str,
        has_image: bool,
        status: u16,
        body: &str,
    ) -> Self {
        if has_image && is_image_input_rejection_response(status, body) {
            return LegError::ImageInputUnsupported {
                provider: provider.to_string(),
                model: model.to_string(),
            };
        }
        self.classify_image_input_rejection(provider, model, has_image)
    }
}

fn is_image_input_rejection(error_type: Option<&str>, message: &str) -> bool {
    let error_type = error_type.unwrap_or_default().to_ascii_lowercase();
    let message = message.to_ascii_lowercase();
    let combined = format!("{error_type} {message}");
    let image_term = [
        "image",
        "vision",
        "visual",
        "multimodal",
        "modality",
        "image_url",
    ]
    .iter()
    .any(|term| combined.contains(term));
    let rejection_term = [
        "unsupported",
        "not support",
        "does not support",
        "not accept",
        "cannot accept",
        "invalid modality",
        "invalid input type",
    ]
    .iter()
    .any(|term| combined.contains(term));
    image_term && rejection_term
}

/// Detects a deterministic image-modality rejection from a provider's raw
/// error response without exposing the response text to callers.
pub(crate) fn is_image_input_rejection_response(status: u16, body: &str) -> bool {
    if !(400..500).contains(&status) {
        return false;
    }

    let body_match = is_image_input_rejection(None, body);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body_match;
    };
    let error = value
        .pointer("/response/error")
        .filter(|value| value.is_object())
        .or_else(|| value.get("error").filter(|value| value.is_object()))
        .unwrap_or(&value);
    let error_type = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.get("type").and_then(serde_json::Value::as_str));
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(body);
    body_match || is_image_input_rejection(error_type, message)
}

impl fmt::Display for LegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LegError::Usage(msg) => write!(f, "usage error: {msg}"),
            LegError::Config(msg) => write!(f, "configuration error: {msg}"),
            LegError::Transport(msg) => write!(f, "transport error: {msg}"),
            LegError::NonRetryableTransport(msg) | LegError::ResponseRead(msg) => {
                write!(f, "transport error: {msg}")
            }
            LegError::ProviderStream {
                error_type,
                message,
            } => {
                write!(f, "provider stream error")?;
                if let Some(error_type) = error_type {
                    write!(f, " ({error_type})")?;
                }
                write!(f, ": {message}")
            }
            LegError::Auth(msg) => write!(f, "authentication error: {msg}"),
            LegError::RateLimited(msg) => write!(f, "rate limited: {msg}"),
            LegError::Server {
                status,
                error_type,
                message,
            } => {
                write!(f, "provider server error ({status}")?;
                if let Some(error_type) = error_type {
                    write!(f, ", {error_type}")?;
                }
                write!(f, "): {message}")
            }
            LegError::Api {
                status,
                error_type,
                message,
            } => {
                write!(f, "provider error ({status}")?;
                if let Some(error_type) = error_type {
                    write!(f, ", {error_type}")?;
                }
                write!(f, "): {message}")
            }
            LegError::Decode(msg) => write!(f, "response decode error: {msg}"),
            LegError::TokenLimit { max_tokens, .. } => write!(
                f,
                "response decode error: reply hit max_tokens ({max_tokens}) before producing text or a tool call; raise LEG_MAX_TOKENS"
            ),
            LegError::ContextWindow(msg) => write!(f, "context window error: {msg}"),
            LegError::ImageInputUnsupported { provider, model } => write!(
                f,
                "image_input_unsupported: provider {provider} model {model} does not accept image input"
            ),
            LegError::Io(msg) => write!(f, "io error: {msg}"),
            LegError::Install(msg) => write!(f, "install error: {msg}"),
            LegError::Guest(failure) => {
                write!(f, "guest error ({}): {}", failure.reason, failure.message)
            }
            LegError::Interrupted { signal } => {
                write!(f, "interrupted by {}", signal.name())
            }
            LegError::Log(msg) => write!(f, "log error: {msg}"),
            LegError::SessionNotFound(session_id) => {
                write!(f, "no session found: {session_id}")
            }
            LegError::TurnFailure {
                message_kind,
                message,
            } => write!(f, "turn failed (kind: {message_kind}): {message}"),
        }
    }
}

impl std::error::Error for LegError {}
