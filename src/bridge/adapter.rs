//! The host-adapter seam.
//!
//! The host owns the transport, the permission prompt, the dispatch, the
//! cancellation, and the user confirmation. The bridge only owns the
//! protocol, so it reaches the host through the [`HostAdapter`] trait: a
//! later mobile slice implements that trait for its platform, and this slice
//! ships the file-backed transport plus the seam itself.
//!
//! The file-backed transport needs no socket and no exec, which matters in
//! the iSH guest: the host can hand the guest a directory both of them see,
//! and the guest can hand the host answers back through the same directory.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{BridgeError, BridgeStatus, PROTOCOL_VERSION};

/// The prefix of the file the bridge writes its request into.
///
/// The file carries the request id, so two requests never share a file.
pub const REQUEST_FILE_PREFIX: &str = "bridge-request-";

/// The prefix of the file the host writes its answer into.
pub const RESPONSE_FILE_PREFIX: &str = "bridge-response-";

/// The prefix of the file the bridge writes a cancellation notice into.
pub const CANCEL_FILE_PREFIX: &str = "bridge-cancel-";

/// The suffix every file of the file transport carries.
const FILE_SUFFIX: &str = ".json";

/// The interval the file transport polls the host with.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The name of the file the bridge writes the request for `request_id` into.
pub fn request_file(request_id: &str) -> String {
    format!("{REQUEST_FILE_PREFIX}{request_id}{FILE_SUFFIX}")
}

/// The name of the file the host writes the answer for `request_id` into.
pub fn response_file(request_id: &str) -> String {
    format!("{RESPONSE_FILE_PREFIX}{request_id}{FILE_SUFFIX}")
}

/// The name of the file the bridge writes the cancellation notice for
/// `request_id` into.
pub fn cancel_file(request_id: &str) -> String {
    format!("{CANCEL_FILE_PREFIX}{request_id}{FILE_SUFFIX}")
}

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
    /// Whether the request may only be previewed.
    ///
    /// A preview asks the host what it would do and must not perform the
    /// action; the host answers it with an approval summary and a token.
    pub preview: bool,
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

/// The answer the host writes back over the file transport.
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

/// The file-backed host transport.
///
/// The bridge and the host share one directory: the bridge writes the request
/// into [`request_file`], and the host writes its answer into
/// [`response_file`]. The bridge reads the answer only when it carries the
/// request's nonce, so a stale answer is never mistaken for a fresh one.
pub struct FileHostAdapter {
    /// The directory the bridge and the host share.
    dir: PathBuf,
    /// The interval the transport polls the host with.
    poll_interval: Duration,
}

impl FileHostAdapter {
    /// Creates a transport that talks to the host through `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            poll_interval: POLL_INTERVAL,
        }
    }

    /// Creates a transport that polls the host at `poll_interval`.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// The directory the bridge and the host share.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the request into the shared directory.
    fn write_request(&self, request: &HostRequest) -> Result<(), String> {
        let text = serde_json::to_string(request)
            .map_err(|error| format!("the request could not be serialized: {error}"))?;
        write_atomic(
            &self.dir.join(request_file(&request.request_id)),
            text.as_bytes(),
        )
    }

    /// Writes the cancellation notice into the shared directory.
    fn write_cancel(&self, request: &HostRequest) {
        let notice = serde_json::json!({
            "version": PROTOCOL_VERSION,
            "request_id": request.request_id,
            "nonce": request.nonce,
            "reason": "the guest stopped waiting for this request",
        });
        if let Ok(text) = serde_json::to_string(&notice) {
            let _ = write_atomic(
                &self.dir.join(cancel_file(&request.request_id)),
                text.as_bytes(),
            );
        }
    }

    /// Reads the host's answer, waiting for it until the deadline.
    fn read_answer(&self, request: &HostRequest, deadline: Instant) -> HostOutcome {
        let path = self.dir.join(response_file(&request.request_id));
        loop {
            if crate::interrupt::is_signaled() {
                self.write_cancel(request);
                return HostOutcome::Cancelled;
            }
            match fs::read(&path) {
                Ok(bytes) => {
                    match serde_json::from_slice::<HostReply>(&bytes) {
                        Ok(reply) => {
                            if reply.nonce != request.nonce
                                || reply.request_id != request.request_id
                            {
                                // The answer belongs to another request; keep
                                // waiting for this one.
                                std::thread::sleep(self.poll_interval);
                                continue;
                            }
                            if reply.version != PROTOCOL_VERSION {
                                return HostOutcome::Failed {
                                    reason: "host_protocol_violation".to_string(),
                                    message: format!(
                                        "the host answered with version {}, not version {PROTOCOL_VERSION}",
                                        reply.version
                                    ),
                                };
                            }
                            return outcome_from_reply(reply);
                        }
                        Err(error) => {
                            return HostOutcome::Failed {
                                reason: "host_crashed".to_string(),
                                message: format!("the host's answer could not be read: {error}"),
                            };
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return HostOutcome::Failed {
                        reason: "host_crashed".to_string(),
                        message: format!("the host's answer could not be read: {error}"),
                    };
                }
            }

            if Instant::now() >= deadline {
                self.write_cancel(request);
                return HostOutcome::TimedOut;
            }
            std::thread::sleep(self.poll_interval);
        }
    }
}

impl HostAdapter for FileHostAdapter {
    fn dispatch(&mut self, request: &HostRequest, deadline: Instant) -> HostOutcome {
        if !self.dir.is_dir() {
            return HostOutcome::Unavailable {
                reason: "host_unreachable".to_string(),
                message: format!(
                    "the host transport directory {} does not exist",
                    self.dir.display()
                ),
            };
        }

        // A stale answer from an earlier request must never be mistaken for
        // this one's answer.
        let response_path = self.dir.join(response_file(&request.request_id));
        if response_path.exists() {
            let _ = fs::remove_file(&response_path);
        }
        let _ = fs::remove_file(self.dir.join(cancel_file(&request.request_id)));

        if let Err(error) = self.write_request(request) {
            return HostOutcome::Failed {
                reason: "transport_error".to_string(),
                message: error,
            };
        }

        let outcome = self.read_answer(request, deadline);
        // The request has been answered; the shared directory must not keep it
        // around for the next request to pick up.
        let _ = fs::remove_file(self.dir.join(request_file(&request.request_id)));
        outcome
    }
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

/// Writes `bytes` into `path` through a temporary file and one rename, so a
/// reader never sees a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    }
    fs::rename(&temporary, path)
        .map_err(|error| format!("could not publish {}: {error}", path.display()))
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
