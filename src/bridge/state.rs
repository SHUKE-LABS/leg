//! The guest-side bridge ledger and the audit summary the bridge writes.
//!
//! The ledger lives beside the guest, not on the host. It remembers which
//! request ids have been used, what each of them asked for, and which
//! approvals are still waiting to be redeemed. It holds no request values:
//! only the request id, the digest of the input, the terminal state of the
//! request, the approval binding, and the expiry of that binding.

use std::collections::BTreeMap;
use std::fs;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::protocol::{BridgeStatus, MAX_REQUEST_ID_LEN, capability, sha256_hex, valid_request_id};

/// The schema of the bridge state file.
const STATE_SCHEMA: &str = "leg.bridge.state/v1";

/// The name of the state file inside the state directory.
const STATE_FILE: &str = "state.json";

/// The name of the lock file that guards the ledger.
const LOCK_FILE: &str = "state.lock";

/// The state a request holds while it is on its way to the host.
const STATE_DISPATCHING: &str = "dispatching";

/// How long the bridge waits between tries for the ledger lock.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The name of the audit log inside the state directory.
const AUDIT_FILE: &str = "audit.log";

/// The environment variable that overrides the bridge state directory.
const STATE_DIR_ENV: &str = "LEG_BRIDGE_STATE_DIR";

/// The environment variable that overrides the audit log path.
const AUDIT_FILE_ENV: &str = "LEG_BRIDGE_AUDIT_FILE";

/// The environment variable that turns the audit summary off.
const AUDIT_ENV: &str = "LEG_BRIDGE_AUDIT";

/// How many request records the ledger keeps.
const MAX_REQUESTS: usize = 64;

/// How many approval bindings the ledger keeps.
const MAX_APPROVALS: usize = 32;

/// The largest ledger file the bridge reads, in bytes.
///
/// The ledger is bounded by its contents — a few dozen short records — so a
/// file that grew past this limit is not one the bridge wrote, and the bridge
/// refuses it rather than reading it into memory.
const MAX_STATE_BYTES: u64 = 128 * 1024;

/// How long an approval token stays redeemable, in seconds.
const APPROVAL_TTL_SECONDS: u64 = 600;

/// The largest audit log the bridge keeps, in bytes.
const MAX_AUDIT_BYTES: u64 = 256 * 1024;

/// One request the bridge has dispatched.
///
/// The record holds the request id, the digest of the request input, and the
/// state the request ended in. It never holds the input itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    /// The digest of the request input.
    pub input_sha256: String,
    /// The state the request ended in: the status the bridge answered with.
    pub terminal_state: String,
    /// When the record was written, in seconds since the Unix epoch.
    pub issued_at: u64,
}

/// One approval the host issued and the bridge recorded.
///
/// The binding names the capability and the digest of the input the approval
/// was granted for, and it expires. The token itself is not kept: the ledger
/// holds its digest, so a token that reaches the ledger file cannot be
/// replayed from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    /// The capability the approval was granted for.
    pub capability: String,
    /// The digest of the input the approval was granted for.
    pub input_sha256: String,
    /// The digest of the approval token.
    pub token_sha256: String,
    /// When the approval was granted, in seconds since the Unix epoch.
    pub issued_at: u64,
    /// When the approval expires, in seconds since the Unix epoch.
    pub expires_at: u64,
    /// Whether the approval was already redeemed.
    #[serde(default)]
    pub redeemed: bool,
}

/// The guest-local bridge ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeState {
    /// The state schema.
    pub schema: String,
    /// The requests the bridge has dispatched, keyed by request id.
    #[serde(default)]
    pub requests: BTreeMap<String, RequestRecord>,
    /// The approvals the bridge has recorded, keyed by the digest of the
    /// approval token.
    #[serde(default)]
    pub approvals: BTreeMap<String, ApprovalRecord>,
    /// The directory the state lives in.
    #[serde(skip)]
    dir: PathBuf,
}

/// The schema string of a fresh state.
fn state_schema() -> String {
    STATE_SCHEMA.to_string()
}

/// Resolves the bridge state directory.
pub fn state_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(STATE_DIR_ENV) {
        if path.is_empty() {
            return Err(format!("{STATE_DIR_ENV} must not be blank"));
        }
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = std::env::var_os("XDG_STATE_HOME")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path).join("leg").join("bridge"));
    }

    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|path| !path.is_empty()))
        .ok_or_else(|| "could not determine a home directory for the bridge state".to_string())?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("leg")
        .join("bridge"))
}

impl BridgeState {
    /// Reads the state held in `dir`; a missing file yields an empty state.
    ///
    /// The ledger is bounded and it is the bridge's own file: a file that is
    /// larger than the limit, that speaks another schema, or that carries
    /// fields the bridge does not know is refused. The bridge never reads a
    /// foreign ledger as if it were its own, and never resets one it does not
    /// understand.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(STATE_FILE);
        let text = match read_bounded(&path)? {
            Some(text) => text,
            None => {
                let mut state = BridgeState {
                    schema: state_schema(),
                    ..BridgeState::default()
                };
                state.dir = dir.to_path_buf();
                return Ok(state);
            }
        };
        let mut state: BridgeState = serde_json::from_str(&text).map_err(|error| {
            format!(
                "the bridge state at {} could not be read: {error}",
                path.display()
            )
        })?;
        if state.schema != STATE_SCHEMA {
            return Err(format!(
                "the bridge state at {} speaks schema `{}`, not `{STATE_SCHEMA}`",
                path.display(),
                state.schema
            ));
        }
        state.check_records(&path)?;
        state.trim_to_bounds();
        state.dir = dir.to_path_buf();
        Ok(state)
    }

    /// Reads the ledger again from its directory.
    ///
    /// Another invocation may have written to it since this one loaded it,
    /// and the ledger is only true while it is held under the lock.
    pub fn reload(&mut self) -> Result<(), String> {
        let fresh = BridgeState::load(&self.dir)?;
        self.schema = fresh.schema;
        self.requests = fresh.requests;
        self.approvals = fresh.approvals;
        Ok(())
    }

    /// The directory the ledger lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Takes the lock that guards the ledger in this directory.
    ///
    /// The lock is an advisory lock on a file in the state directory: two
    /// invocations that share the state directory take turns, so a request id
    /// cannot be dispatched twice. The wait is bounded by the request's own
    /// deadline, and the operating system releases the lock when the holder
    /// ends, so a crash leaves no stale lock behind.
    pub fn lock_until(&self, deadline: Instant) -> Result<StateLock, LockError> {
        StateLock::acquire_until(&self.dir, deadline)
    }

    /// Writes the state back to its directory.
    ///
    /// The write is atomic: the ledger reaches the disk whole or not at all,
    /// so a request id is never half remembered. The ledger is bounded on the
    /// way out as well as on the way in: a state that would grow past the
    /// limit is refused, so the bridge never publishes a ledger it would
    /// refuse to read.
    pub fn save(&self) -> Result<(), String> {
        fs::create_dir_all(&self.dir)
            .map_err(|error| format!("could not create the state directory: {error}"))?;
        let path = self.dir.join(STATE_FILE);
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| format!("could not serialize the bridge state: {error}"))?;
        if text.len() as u64 > MAX_STATE_BYTES {
            return Err(format!(
                "the bridge state would be larger than the {MAX_STATE_BYTES} byte limit"
            ));
        }
        let temporary = path.with_extension("tmp");
        {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
            file.write_all(text.as_bytes())
                .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
            file.sync_all()
                .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        }
        fs::rename(&temporary, &path)
            .map_err(|error| format!("could not publish {}: {error}", path.display()))
    }

    /// The record of a request the bridge has already dispatched.
    pub fn record(&self, request_id: &str) -> Option<&RequestRecord> {
        self.requests.get(request_id)
    }

    /// Whether the request id was already used.
    ///
    /// The ledger remembers the last [`MAX_REQUESTS`] request ids, so an id
    /// is refused for as long as it is kept.
    pub fn is_known(&self, request_id: &str) -> bool {
        self.requests.contains_key(request_id)
    }

    /// Records a request that reached the host.
    pub fn record_dispatched(
        &mut self,
        request_id: &str,
        input_sha256: &str,
        terminal_state: &str,
    ) {
        self.insert_request(
            request_id,
            RequestRecord {
                input_sha256: input_sha256.to_string(),
                terminal_state: terminal_state.to_string(),
                issued_at: unix_seconds(),
            },
        );
    }

    /// Reserves a request id for a request that is on its way to the host.
    ///
    /// The reservation is taken under the lock, before the host is called, so
    /// a second invocation that carries the same id finds it taken.
    pub fn reserve(&mut self, request_id: &str, input_sha256: &str) {
        self.record_dispatched(request_id, input_sha256, STATE_DISPATCHING);
    }

    /// Gives a reserved request id back.
    ///
    /// A request that never reached the host changed nothing, so its id is
    /// free again and the caller may try once more.
    pub fn release(&mut self, request_id: &str) {
        self.requests.remove(request_id);
    }

    /// Records an approval the host issued.
    ///
    /// The binding names the capability and the digest of the input the
    /// approval was granted for, so a token cannot be redeemed for another
    /// capability or for another input.
    pub fn record_approval(&mut self, capability: &str, input_sha256: &str, approval_token: &str) {
        let issued_at = unix_seconds();
        let token_sha256 = sha256_hex(approval_token.as_bytes());
        let record = ApprovalRecord {
            capability: capability.to_string(),
            input_sha256: input_sha256.to_string(),
            token_sha256: token_sha256.clone(),
            issued_at,
            expires_at: issued_at + APPROVAL_TTL_SECONDS,
            redeemed: false,
        };
        self.approvals.insert(token_sha256, record);
        self.trim_to_bounds();
    }

    /// The approval the token belongs to.
    pub fn approval_for(&self, approval_token: &str) -> Option<&ApprovalRecord> {
        self.approvals.get(&sha256_hex(approval_token.as_bytes()))
    }

    /// Marks an approval as redeemed, so its token cannot be used twice.
    pub fn redeem_approval(&mut self, approval_token: &str) {
        if let Some(record) = self
            .approvals
            .get_mut(&sha256_hex(approval_token.as_bytes()))
        {
            record.redeemed = true;
        }
    }

    /// Whether an approval is still within its window.
    pub fn approval_is_fresh(record: &ApprovalRecord) -> bool {
        unix_seconds() <= record.expires_at
    }

    /// Inserts a request record and keeps the ledger bounded.
    fn insert_request(&mut self, request_id: &str, record: RequestRecord) {
        if request_id.len() > MAX_REQUEST_ID_LEN {
            return;
        }
        self.requests.insert(request_id.to_string(), record);
        self.trim_to_bounds();
    }

    /// Checks that every record in the ledger has the documented shape.
    ///
    /// The bridge trusts the ledger it reads: it decides which request ids
    /// are used and which approvals are still redeemable. A record that does
    /// not have the shape the bridge writes is not one the bridge wrote, so
    /// the whole ledger is refused instead of being read as if it were its
    /// own.
    fn check_records(&self, path: &Path) -> Result<(), String> {
        for (request_id, record) in &self.requests {
            if !valid_request_id(request_id) {
                return Err(format!(
                    "the bridge state at {} holds the request id `{request_id}`, which is not one",
                    path.display()
                ));
            }
            if !is_digest(&record.input_sha256) {
                return Err(format!(
                    "the bridge state at {} holds a request record whose digest is not a digest",
                    path.display()
                ));
            }
            if !is_terminal_state(&record.terminal_state) {
                return Err(format!(
                    "the bridge state at {} holds a request record that ended in `{}`",
                    path.display(),
                    record.terminal_state
                ));
            }
        }
        for (token_digest, record) in &self.approvals {
            if !is_digest(token_digest) || token_digest != &record.token_sha256 {
                return Err(format!(
                    "the bridge state at {} holds an approval under a key that is not its digest",
                    path.display()
                ));
            }
            if !is_digest(&record.input_sha256) {
                return Err(format!(
                    "the bridge state at {} holds an approval whose digest is not a digest",
                    path.display()
                ));
            }
            if capability(&record.capability).is_none() {
                return Err(format!(
                    "the bridge state at {} holds an approval for the unknown capability `{}`",
                    path.display(),
                    record.capability
                ));
            }
            if record.expires_at < record.issued_at {
                return Err(format!(
                    "the bridge state at {} holds an approval that expires before it was issued",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// Keeps the ledger within its bounds.
    ///
    /// The oldest records fall out first, so the ledger always holds the most
    /// recent requests. A ledger that grew past its bounds — because it was
    /// written by another program, or by a version that kept more — is cut
    /// back to them here.
    fn trim_to_bounds(&mut self) {
        while self.requests.len() > MAX_REQUESTS {
            let Some(oldest) = self
                .requests
                .iter()
                .min_by_key(|(_, record)| record.issued_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.requests.remove(&oldest);
        }
        while self.approvals.len() > MAX_APPROVALS {
            let Some(oldest) = self
                .approvals
                .iter()
                .min_by_key(|(_, record)| record.issued_at)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            self.approvals.remove(&oldest);
        }
    }
}

/// Reads a ledger file that may not exist, at most `MAX_STATE_BYTES` long.
///
/// The file is read through a bounded reader, so a file that grew between a
/// size check and the read cannot be pulled into memory, and a file that is
/// over the limit is refused rather than truncated.
fn read_bounded(path: &Path) -> Result<Option<String>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read the bridge state at {}: {error}",
                path.display()
            ));
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "could not read the bridge state at {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(format!(
            "the bridge state at {} is larger than the {MAX_STATE_BYTES} byte limit",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("the bridge state at {} is not valid UTF-8", path.display()))
}

/// Whether a string is a SHA-256 digest in lowercase hex.
fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
}

/// Whether a state is one the bridge writes.
fn is_terminal_state(state: &str) -> bool {
    state == STATE_DISPATCHING
        || [
            BridgeStatus::Ok,
            BridgeStatus::ApprovalRequired,
            BridgeStatus::Denied,
            BridgeStatus::Cancelled,
            BridgeStatus::Unavailable,
            BridgeStatus::Error,
        ]
        .iter()
        .any(|status| status.as_str() == state)
}

/// Why the lock on the ledger could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// Another invocation holds the lock and the deadline ran out.
    Timeout,
    /// The request was cancelled while the bridge waited for the lock.
    Cancelled,
    /// The lock file could not be opened or locked.
    Io(String),
}

/// The lock that guards the ledger.
///
/// The lock is held for as long as the guard lives, and the operating system
/// releases it when the process ends — a bridge that dies mid-request leaves
/// no lock behind.
pub struct StateLock {
    /// The file the lock is held on.
    file: File,
    /// The path of that file.
    path: PathBuf,
}

impl StateLock {
    /// Takes the lock in `dir`, waiting for the holder to finish.
    ///
    /// The wait is bounded by `deadline`: a request that cannot take the lock
    /// in time answers `timeout` rather than waiting for a host that may
    /// never come. The wait also stops when the guest interrupts the bridge.
    pub fn acquire_until(dir: &Path, deadline: Instant) -> Result<Self, LockError> {
        fs::create_dir_all(dir).map_err(|error| {
            LockError::Io(format!("could not create the state directory: {error}"))
        })?;
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                LockError::Io(format!(
                    "could not open the ledger lock {}: {error}",
                    path.display()
                ))
            })?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file, path }),
                // Another invocation holds the lock: the bridge waits for it,
                // within the request's own deadline.
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(LockError::Io(format!(
                        "could not lock the ledger {}: {error}",
                        path.display()
                    )));
                }
            }
            if crate::interrupt::is_signaled() {
                return Err(LockError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(LockError::Timeout);
            }
            std::thread::sleep(LOCK_POLL_INTERVAL);
        }
    }

    /// The file the lock is held on.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// One audit summary line.
///
/// The summary carries request metadata and the *shape* of the input — the
/// field names and the digest — never the field values. A photo's contents, a
/// file's contents, and a shortcut's inputs therefore never reach the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEntry {
    /// When the request was answered, in seconds since the Unix epoch.
    pub ts: u64,
    /// The request id the summary belongs to.
    pub request_id: String,
    /// The capability the request named.
    pub capability: String,
    /// The outcome of the request.
    pub status: &'static str,
    /// The failure reason, when the request failed.
    pub reason: Option<String>,
    /// How long the request took, in milliseconds.
    pub duration_ms: u64,
    /// The digest of the request input.
    pub input_sha256: String,
    /// The names of the input fields the request carried.
    pub input_fields: Vec<String>,
    /// Whether the request redeemed an approval token.
    pub approved: bool,
}

/// The audit log the bridge writes.
#[derive(Debug, Clone)]
pub struct AuditLog {
    /// The file the summaries are appended to.
    path: Option<PathBuf>,
}

impl AuditLog {
    /// Creates an audit log that appends to `path`.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    /// Creates the audit log the environment configures.
    ///
    /// The summary goes to `LEG_BRIDGE_AUDIT_FILE`, else to `audit.log` in the
    /// state directory. `LEG_BRIDGE_AUDIT=off` turns the log off.
    pub fn from_env(state_dir: &Path) -> Self {
        let enabled = std::env::var(AUDIT_ENV)
            .map(|value| !value.eq_ignore_ascii_case("off"))
            .unwrap_or(true);
        if !enabled {
            return Self::new(None);
        }
        let path = std::env::var_os(AUDIT_FILE_ENV)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join(AUDIT_FILE));
        Self::new(Some(path))
    }

    /// Whether the log writes summaries.
    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// The file the summaries are appended to.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Appends one summary to the log.
    pub fn append(&self, entry: &AuditEntry) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut line = serde_json::to_string(entry)
            .map_err(|error| format!("could not serialize the audit summary: {error}"))?;
        line.push('\n');

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create the audit directory: {error}"))?;
        }
        // The log stays bounded: a log that reached its cap starts over
        // instead of growing without limit.
        if let Ok(metadata) = fs::metadata(path)
            && metadata.len() + line.len() as u64 > MAX_AUDIT_BYTES
        {
            let _ = fs::remove_file(path);
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| format!("could not open the audit log: {error}"))?;
        file.write_all(line.as_bytes())
            .map_err(|error| format!("could not write the audit summary: {error}"))
    }
}

/// The current time in seconds since the Unix epoch.
pub fn audit_timestamp() -> u64 {
    unix_seconds()
}

/// The current time in seconds since the Unix epoch.
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
