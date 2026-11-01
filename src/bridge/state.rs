//! The guest-side bridge state: the request records that keep a request id
//! unique, the approval tokens that are waiting to be redeemed, and the audit
//! summary the bridge writes.
//!
//! The state lives beside the guest, not on the host: the approval token an
//! answer issues is redeemed by a later request in the same guest, and the
//! request records keep a request id from being used twice.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::protocol::MAX_REQUEST_ID_LEN;

/// The schema of the bridge state file.
const STATE_SCHEMA: &str = "leg.bridge.state/v1";

/// The name of the state file inside the state directory.
const STATE_FILE: &str = "state.json";

/// The name of the audit log inside the state directory.
const AUDIT_FILE: &str = "audit.log";

/// The environment variable that overrides the bridge state directory.
const STATE_DIR_ENV: &str = "LEG_BRIDGE_STATE_DIR";

/// The environment variable that overrides the audit log path.
const AUDIT_FILE_ENV: &str = "LEG_BRIDGE_AUDIT_FILE";

/// The environment variable that turns the audit summary off.
const AUDIT_ENV: &str = "LEG_BRIDGE_AUDIT";

/// How many request records the state keeps.
const MAX_RECORDS: usize = 64;

/// How long an approval token stays redeemable, in seconds.
const APPROVAL_TTL_SECONDS: u64 = 600;

/// The largest audit log the bridge keeps, in bytes.
const MAX_AUDIT_BYTES: u64 = 256 * 1024;

/// One request the bridge has seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// The capability the request named.
    pub capability: String,
    /// The digest of the request input.
    pub input_sha256: String,
    /// The approval token that is waiting to be redeemed, when the request is
    /// waiting for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_token: Option<String>,
    /// When the record was written, in seconds since the Unix epoch.
    pub issued_at: u64,
}

impl RequestRecord {
    /// Whether the record still waits for an approval.
    pub fn is_pending(&self) -> bool {
        self.approval_token.is_some()
    }
}

/// The guest-local bridge state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeState {
    /// The state schema.
    #[serde(default = "state_schema")]
    pub schema: String,
    /// The requests the bridge has seen, keyed by request id.
    #[serde(default)]
    pub requests: BTreeMap<String, RequestRecord>,
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
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(STATE_FILE);
        let mut state = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<BridgeState>(&text).map_err(|error| {
                format!(
                    "the bridge state at {} could not be read: {error}",
                    path.display()
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BridgeState {
                schema: state_schema(),
                ..BridgeState::default()
            },
            Err(error) => {
                return Err(format!(
                    "could not read the bridge state at {}: {error}",
                    path.display()
                ));
            }
        };
        state.dir = dir.to_path_buf();
        Ok(state)
    }

    /// Writes the state back to its directory.
    pub fn save(&self) -> Result<(), String> {
        fs::create_dir_all(&self.dir)
            .map_err(|error| format!("could not create the state directory: {error}"))?;
        let path = self.dir.join(STATE_FILE);
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| format!("could not serialize the bridge state: {error}"))?;
        fs::write(&path, text).map_err(|error| format!("could not write the bridge state: {error}"))
    }

    /// The record of a request the bridge has already seen.
    pub fn record(&self, request_id: &str) -> Option<&RequestRecord> {
        self.requests.get(request_id)
    }

    /// Whether the request id was already used.
    pub fn is_known(&self, request_id: &str) -> bool {
        self.requests.contains_key(request_id)
    }

    /// The record of a request that is waiting for an approval.
    pub fn pending(&self, request_id: &str) -> Option<&RequestRecord> {
        self.requests
            .get(request_id)
            .filter(|record| record.is_pending())
    }

    /// Records a request that waits for an approval token.
    pub fn record_approval(
        &mut self,
        request_id: &str,
        capability: &str,
        input_sha256: &str,
        approval_token: &str,
    ) {
        self.insert(
            request_id,
            RequestRecord {
                capability: capability.to_string(),
                input_sha256: input_sha256.to_string(),
                approval_token: Some(approval_token.to_string()),
                issued_at: unix_seconds(),
            },
        );
    }

    /// Records a request that finished.
    pub fn record_completed(&mut self, request_id: &str, capability: &str, input_sha256: &str) {
        self.insert(
            request_id,
            RequestRecord {
                capability: capability.to_string(),
                input_sha256: input_sha256.to_string(),
                approval_token: None,
                issued_at: unix_seconds(),
            },
        );
    }

    /// Drops the record of a request that finished.
    pub fn settle(&mut self, request_id: &str) {
        self.requests.remove(request_id);
    }

    /// Whether an approval token is still redeemable.
    pub fn approval_is_fresh(record: &RequestRecord) -> bool {
        unix_seconds().saturating_sub(record.issued_at) <= APPROVAL_TTL_SECONDS
    }

    /// Inserts a record and keeps the state bounded.
    fn insert(&mut self, request_id: &str, record: RequestRecord) {
        if request_id.len() > MAX_REQUEST_ID_LEN {
            return;
        }
        self.requests.insert(request_id.to_string(), record);
        while self.requests.len() > MAX_RECORDS {
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
