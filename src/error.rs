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
    /// A local I/O operation failed.
    Io(String),
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
            LegError::Auth(_) => "auth",
            LegError::RateLimited(_) => "rate_limited",
            LegError::Server { .. } => "server",
            LegError::Api { .. } => "api",
            LegError::Decode(_) => "decode",
            LegError::Io(_) => "io",
            LegError::Interrupted { .. } => "interrupted",
            LegError::Log(_) => "log",
            LegError::SessionNotFound(_) => "session_not_found",
            LegError::TurnFailure { .. } => "turn_failure",
        }
    }
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
            LegError::Io(msg) => write!(f, "io error: {msg}"),
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
