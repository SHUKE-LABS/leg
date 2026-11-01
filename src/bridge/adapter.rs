//! The host-adapter seam and the fixed host transport.
//!
//! The host owns the transport, the permission prompt, the dispatch, the
//! cancellation, and the user confirmation. The bridge only owns the
//! protocol, so it reaches the host through the [`HostAdapter`] trait: a
//! later mobile slice implements that trait for its platform, and this slice
//! ships the seam plus the one fixed transport the plan names — a Unix
//! socket endpoint in `LEG_BRIDGE_HOST_SOCKET`.
//!
//! The transport is a socket and nothing else: it never loads a
//! request-selected command, plugin, native library, or endpoint. The
//! endpoint comes from the environment, the request never names one.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
#[cfg(unix)]
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{BridgeError, BridgeStatus, PROTOCOL_VERSION};

/// The largest frame the bridge reads from the host.
pub const MAX_HOST_FRAME_BYTES: usize = 64 * 1024;

/// The longest the bridge waits for one read before it looks at the clock
/// again.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A request the bridge hands to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostRequest {
    /// The protocol version the request speaks.
    pub version: u64,
    /// The request id the caller chose.
    pub request_id: String,
    /// The token that ties the host's answer to this request.
    pub nonce: String,
    /// The capability the request names.
    pub capability: String,
    /// The structured input the request carries.
    pub input: Value,
    /// The digest of the input, so the host can bind its approval to it.
    pub input_sha256: String,
    /// The approval token a previous answer issued, when the caller replays it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_token: Option<String>,
    /// The deadline the bridge applies to this request.
    pub timeout_ms: u64,
}

/// What the host answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOutcome {
    /// The capability ran; the answer carries its result.
    Ok(Value),
    /// The capability needs an approval; the answer carries the token and the
    /// review summary the host wants a human to see.
    ApprovalRequired {
        /// The one-time token the host issued.
        approval_token: String,
        /// The reviewable summary of what the host would do.
        review: Value,
    },
    /// The host refused the request.
    Denied {
        /// The stable, machine-readable refusal reason.
        reason: String,
        /// The human-readable refusal detail.
        message: String,
    },
    /// The request was cancelled before it finished.
    Cancelled,
    /// The host cannot serve the capability.
    Unavailable {
        /// The stable, machine-readable reason.
        reason: String,
        /// The human-readable detail.
        message: String,
    },
    /// The host failed, crashed, or broke the protocol.
    Failed {
        /// The stable, machine-readable reason.
        reason: String,
        /// The human-readable detail.
        message: String,
    },
    /// The host did not answer in time.
    TimedOut,
}

/// The seam a platform adapter implements.
///
/// The host answers one request at a time; the bridge gives it a deadline and
/// expects one answer before that deadline.
pub trait HostAdapter {
    /// Hands one request to the host and waits for its answer.
    fn dispatch(&mut self, request: &HostRequest, deadline: Instant) -> HostOutcome;
}

/// The transport the bridge uses when the host configured none.
///
/// The bridge is useless without a host, but it is not broken: it answers
/// with the unavailable status, so a caller learns that no host answered
/// rather than that the request failed.
pub struct UnavailableAdapter;

impl HostAdapter for UnavailableAdapter {
    fn dispatch(&mut self, _request: &HostRequest, _deadline: Instant) -> HostOutcome {
        HostOutcome::Unavailable {
            reason: "no_host_transport".to_string(),
            message: "the host configured no capability transport for the guest".to_string(),
        }
    }
}

/// What went wrong on the connection to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionError {
    /// The host closed the connection before it answered.
    Closed,
    /// The host's frame was larger than the limit.
    TooLarge,
    /// The connection itself failed.
    Io(String),
}

/// The connection the transport talks to the host over.
///
/// The seam is small on purpose: the transport writes one frame and reads
/// one frame. A platform that cannot use a Unix socket supplies its own
/// implementation of this trait, and the protocol around it stays the same.
pub trait HostConnection {
    /// Writes one frame to the host.
    fn send(&mut self, frame: &str) -> Result<(), String>;

    /// Reads one frame from the host, waiting at most `wait` for it.
    ///
    /// The call returns `Ok(None)` when the host sent nothing within `wait`;
    /// a frame that arrives in pieces stays in the connection's own buffer.
    fn receive(&mut self, wait: Duration) -> Result<Option<String>, ConnectionError>;
}

/// The answer the host writes back over the transport.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostReply {
    /// The protocol version the answer speaks.
    version: u64,
    /// The token that ties the answer to the request.
    nonce: String,
    /// The request id the answer belongs to.
    request_id: String,
    /// The outcome of the request.
    status: BridgeStatus,
    /// The capability result, when there is one.
    #[serde(default)]
    result: Option<Value>,
    /// The structured failure, when there is one.
    #[serde(default)]
    error: Option<BridgeError>,
}

/// The fixed host transport: a Unix socket endpoint.
///
/// The host listens on the socket and the guest connects to it. The endpoint
/// comes from the environment, so a request can never choose it.
pub struct SocketHostAdapter {
    /// The socket the bridge connects to.
    path: PathBuf,
}

impl SocketHostAdapter {
    /// Creates a transport that talks to the host through `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The socket the bridge connects to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl HostAdapter for SocketHostAdapter {
    fn dispatch(&mut self, request: &HostRequest, deadline: Instant) -> HostOutcome {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;

            let stream = match UnixStream::connect(&self.path) {
                Ok(stream) => stream,
                Err(error) => {
                    return HostOutcome::Unavailable {
                        reason: "host_unreachable".to_string(),
                        message: format!(
                            "the host socket {} could not be reached: {error}",
                            self.path.display()
                        ),
                    };
                }
            };
            let mut connection = UnixHostConnection::new(stream);
            // The connect itself is to a local endpoint and does not wait for
            // the host, but the deadline still rules: a connection that took
            // longer than the request may take is no answer at all.
            if Instant::now() >= deadline {
                return HostOutcome::TimedOut;
            }
            exchange(&mut connection, request, deadline)
        }
        #[cfg(not(unix))]
        {
            let _ = (request, deadline);
            HostOutcome::Unavailable {
                reason: "host_transport_unsupported".to_string(),
                message:
                    "this platform has no Unix socket transport; the guest runs on a Unix host"
                        .to_string(),
            }
        }
    }
}

/// The connection to the host over its Unix socket.
#[cfg(unix)]
struct UnixHostConnection {
    /// The socket the bridge talks to the host over.
    stream: std::os::unix::net::UnixStream,
    /// The bytes of a frame that arrived in pieces.
    buffer: Vec<u8>,
}

#[cfg(unix)]
impl UnixHostConnection {
    /// Creates a connection over `stream`.
    fn new(stream: std::os::unix::net::UnixStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }
}

#[cfg(unix)]
impl HostConnection for UnixHostConnection {
    fn send(&mut self, frame: &str) -> Result<(), String> {
        use std::io::Write as _;

        self.stream
            .write_all(frame.as_bytes())
            .and_then(|()| self.stream.write_all(b"\n"))
            .and_then(|()| self.stream.flush())
            .map_err(|error| format!("the host socket could not be written: {error}"))
    }

    fn receive(&mut self, wait: Duration) -> Result<Option<String>, ConnectionError> {
        use std::io::Read as _;

        // A read timeout that rounds to zero is an error on some systems, so
        // the wait never drops below a millisecond.
        let wait = wait.max(Duration::from_millis(1));
        self.stream
            .set_read_timeout(Some(wait))
            .map_err(|error| ConnectionError::Io(error.to_string()))?;

        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(ConnectionError::Closed),
            Ok(count) => {
                self.buffer.extend_from_slice(&chunk[..count]);
                if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                    let frame = self.buffer.drain(..=position).collect::<Vec<u8>>();
                    let frame = &frame[..frame.len() - 1];
                    return Ok(Some(String::from_utf8_lossy(frame).to_string()));
                }
                if self.buffer.len() > MAX_HOST_FRAME_BYTES {
                    return Err(ConnectionError::TooLarge);
                }
                Ok(None)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(ConnectionError::Io(error.to_string())),
        }
    }
}

/// Carries one request to the host and reads its answer.
///
/// The exchange is the part of the transport every platform shares: it sends
/// the request, waits for the answer within the deadline, and turns a broken
/// answer into a distinct outcome.
pub fn exchange(
    connection: &mut dyn HostConnection,
    request: &HostRequest,
    deadline: Instant,
) -> HostOutcome {
    if crate::interrupt::is_signaled() {
        return HostOutcome::Cancelled;
    }

    let frame = match serde_json::to_string(request) {
        Ok(frame) => frame,
        Err(error) => {
            return HostOutcome::Failed {
                reason: "transport_error".to_string(),
                message: format!("the request could not be serialized: {error}"),
            };
        }
    };
    if let Err(error) = connection.send(&frame) {
        return HostOutcome::Failed {
            reason: "transport_error".to_string(),
            message: error,
        };
    }

    loop {
        if crate::interrupt::is_signaled() {
            return HostOutcome::Cancelled;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return HostOutcome::TimedOut;
        }

        match connection.receive(remaining.min(POLL_INTERVAL)) {
            Ok(Some(frame)) => return read_answer(&frame, request),
            Ok(None) => {}
            Err(ConnectionError::Closed) => {
                return HostOutcome::Failed {
                    reason: "host_crashed".to_string(),
                    message: "the host closed the connection without answering".to_string(),
                };
            }
            Err(ConnectionError::TooLarge) => {
                return HostOutcome::Failed {
                    reason: "host_protocol_violation".to_string(),
                    message: format!(
                        "the host's answer is larger than the {MAX_HOST_FRAME_BYTES} byte limit"
                    ),
                };
            }
            Err(ConnectionError::Io(error)) => {
                return HostOutcome::Failed {
                    reason: "host_crashed".to_string(),
                    message: format!("the host's answer could not be read: {error}"),
                };
            }
        }
    }
}

/// Turns the host's answer into a bridge outcome.
fn read_answer(frame: &str, request: &HostRequest) -> HostOutcome {
    let reply: HostReply = match serde_json::from_str(frame) {
        Ok(reply) => reply,
        Err(error) => {
            return HostOutcome::Failed {
                reason: "host_crashed".to_string(),
                message: format!("the host's answer could not be read: {error}"),
            };
        }
    };
    if reply.version != PROTOCOL_VERSION {
        return HostOutcome::Failed {
            reason: "host_protocol_violation".to_string(),
            message: format!(
                "the host answered with version {}, not version {PROTOCOL_VERSION}",
                reply.version
            ),
        };
    }
    if reply.nonce != request.nonce || reply.request_id != request.request_id {
        return HostOutcome::Failed {
            reason: "host_protocol_violation".to_string(),
            message: "the host's answer belongs to another request".to_string(),
        };
    }
    outcome_from_reply(reply)
}

/// Turns the host's answer into a bridge outcome.
fn outcome_from_reply(reply: HostReply) -> HostOutcome {
    match reply.status {
        BridgeStatus::Ok => HostOutcome::Ok(reply.result.unwrap_or(Value::Null)),
        BridgeStatus::ApprovalRequired => {
            let result = reply.result.unwrap_or(Value::Null);
            let Some(fields) = result.as_object() else {
                return HostOutcome::Failed {
                    reason: "host_protocol_violation".to_string(),
                    message: "the host asked for an approval without a review summary".to_string(),
                };
            };
            let Some(approval_token) = fields.get("approval_token").and_then(Value::as_str) else {
                return HostOutcome::Failed {
                    reason: "host_protocol_violation".to_string(),
                    message: "the host asked for an approval without a token".to_string(),
                };
            };
            let review = fields.get("review").cloned().unwrap_or(Value::Null);
            HostOutcome::ApprovalRequired {
                approval_token: approval_token.to_string(),
                review,
            }
        }
        BridgeStatus::Denied => {
            let (reason, message) =
                split_error(reply.error, "denied", "the host refused the request");
            HostOutcome::Denied { reason, message }
        }
        BridgeStatus::Cancelled => HostOutcome::Cancelled,
        BridgeStatus::Unavailable => {
            let (reason, message) = split_error(
                reply.error,
                "unavailable",
                "the host cannot serve the capability",
            );
            HostOutcome::Unavailable { reason, message }
        }
        BridgeStatus::Error => {
            let (reason, message) = split_error(reply.error, "host_error", "the host failed");
            HostOutcome::Failed { reason, message }
        }
    }
}

/// Splits a structured failure into a reason and a message.
fn split_error(
    error: Option<BridgeError>,
    default_reason: &str,
    default_message: &str,
) -> (String, String) {
    match error {
        Some(error) => (error.reason, error.message),
        None => (default_reason.to_string(), default_message.to_string()),
    }
}

/// Builds the token that ties one request to its answer.
pub fn new_nonce() -> String {
    let bytes = random_bytes(16).unwrap_or_else(fallback_bytes);
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(nonce, "{byte:02x}");
    }
    nonce
}

/// Reads random bytes from the operating system.
#[cfg(unix)]
fn random_bytes(length: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;

    let mut file = fs::File::open("/dev/urandom").ok()?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// Reads random bytes from the operating system.
#[cfg(not(unix))]
fn random_bytes(_length: usize) -> Option<Vec<u8>> {
    None
}

/// Derives nonce bytes from the clock when the system source is unavailable.
fn fallback_bytes() -> Vec<u8> {
    let mut hasher = DefaultHasher::new();
    unix_seconds().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::time::Instant::now().elapsed().hash(&mut hasher);
    hasher.finish().to_le_bytes().to_vec()
}

/// The current time in seconds since the Unix epoch.
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
