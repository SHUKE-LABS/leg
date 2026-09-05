//! Error types shared across leg's runtime surfaces.
//!
//! Configuration failures and the provider transport's failure modes are
//! modelled as distinct variants so callers can react to them explicitly. The
//! Messages client maps HTTP and decode failures onto these variants rather
//! than collapsing everything into a single opaque error.

use std::fmt;

/// Convenience alias for results produced by leg's runtime.
pub type Result<T> = std::result::Result<T, LegError>;

/// Top-level error type for leg.
#[derive(Debug)]
pub enum LegError {
    /// A command-line argument was missing, unrecognised, or malformed. Carries
    /// a human-readable explanation plus the one-line usage summary.
    Usage(String),
    /// Configuration could not be loaded or was invalid (e.g. a missing or
    /// malformed environment variable).
    Config(String),
    /// A transport-level failure with no HTTP response: connection refused,
    /// DNS failure, TLS error, timeout, etc.
    Transport(String),
    /// The provider rejected the credentials (HTTP 401).
    Auth(String),
    /// The provider rate-limited the request (HTTP 429).
    RateLimited(String),
    /// The provider returned a server-side failure (HTTP 5xx).
    Server {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message, or the raw body when it could not be
        /// parsed.
        message: String,
    },
    /// The provider returned some other non-success status (e.g. 400 Bad
    /// Request) that does not map to a more specific variant.
    Api {
        /// The HTTP status code.
        status: u16,
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
    /// A JSONL exchange trail (`LEG_EVENT_LOG` or a `--resume` file) could not
    /// be parsed: a malformed line, or a known event missing required fields.
    Log(String),
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
            LegError::Auth(_) => "auth",
            LegError::RateLimited(_) => "rate_limited",
            LegError::Server { .. } => "server",
            LegError::Api { .. } => "api",
            LegError::Decode(_) => "decode",
            LegError::Io(_) => "io",
            LegError::Log(_) => "log",
        }
    }
}

impl fmt::Display for LegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LegError::Usage(msg) => write!(f, "usage error: {msg}"),
            LegError::Config(msg) => write!(f, "configuration error: {msg}"),
            LegError::Transport(msg) => write!(f, "transport error: {msg}"),
            LegError::Auth(msg) => write!(f, "authentication error: {msg}"),
            LegError::RateLimited(msg) => write!(f, "rate limited: {msg}"),
            LegError::Server { status, message } => {
                write!(f, "provider server error ({status}): {message}")
            }
            LegError::Api { status, message } => {
                write!(f, "provider error ({status}): {message}")
            }
            LegError::Decode(msg) => write!(f, "response decode error: {msg}"),
            LegError::Io(msg) => write!(f, "io error: {msg}"),
            LegError::Log(msg) => write!(f, "log error: {msg}"),
        }
    }
}

impl std::error::Error for LegError {}
