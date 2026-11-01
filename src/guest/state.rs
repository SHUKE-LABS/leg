//! The guest-local provisioning state: the record of what this tool installed
//! and the approval tokens waiting to be redeemed.
//!
//! The state lives beside the guest, not on the host that drives it: the
//! approval token a preview issues is redeemed by a later command in the same
//! guest, and the installed-package record keeps the storage budget honest
//! across runs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{GuestFailure, LegError, Result};
use crate::guest::Candidate;

/// The schema of the guest state file.
const STATE_SCHEMA: &str = "leg.guest.state/v1";

/// The schema of an approval record.
const APPROVAL_SCHEMA: &str = "leg.guest.approval/v1";

/// The name of the state file inside the state directory.
const STATE_FILE: &str = "state.json";

/// The name of the approvals directory inside the state directory.
const APPROVALS_DIR: &str = "approvals";

/// The environment variable that overrides the guest state directory.
const STATE_DIR_ENV: &str = "LEG_GUEST_STATE_DIR";

/// How long an issued approval token stays redeemable, in seconds.
const APPROVAL_TTL_SECONDS: u64 = 3600;

/// A package this tool installed in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPackage {
    /// The installed version.
    pub version: String,
    /// The installed size in bytes, as the index recorded it.
    pub install_bytes: u64,
}

/// The guest-local state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestState {
    /// The state schema.
    #[serde(default = "state_schema")]
    pub schema: String,
    /// The allowlisted packages this tool has recorded as installed, keyed by
    /// package name.
    #[serde(default)]
    pub installed: BTreeMap<String, InstalledPackage>,
    /// The total install bytes this tool provisioned.
    #[serde(default)]
    pub provisioned_install_bytes: u64,
    /// The directory the state lives in.
    #[serde(skip)]
    dir: PathBuf,
}

/// The schema string of a fresh state.
fn state_schema() -> String {
    STATE_SCHEMA.to_string()
}

/// A pending approval: the exact snapshot a host may authorize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The approval schema.
    #[serde(default = "approval_schema")]
    pub schema: String,
    /// The package the approval covers.
    pub package: String,
    /// The candidate the approval was issued for.
    pub candidate: Candidate,
    /// When the approval was issued, in seconds since the Unix epoch.
    pub issued_at: u64,
}

/// The schema string of a fresh approval.
fn approval_schema() -> String {
    APPROVAL_SCHEMA.to_string()
}

/// Resolves the guest state directory.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(STATE_DIR_ENV) {
        if path.is_empty() {
            return Err(LegError::Config(format!(
                "{STATE_DIR_ENV} must not be blank"
            )));
        }
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = std::env::var_os("XDG_STATE_HOME")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path).join("leg").join("guest"));
    }

    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|path| !path.is_empty()))
        .ok_or_else(|| {
            LegError::Config("could not determine home directory for the guest state".to_string())
        })?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("leg")
        .join("guest"))
}

impl GuestState {
    /// Reads the state held in `dir`; a missing file yields an empty state.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(STATE_FILE);
        let mut state = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<GuestState>(&text).map_err(|error| {
                LegError::Guest(GuestFailure {
                    reason: "state_unreadable",
                    message: format!(
                        "the guest state at {} could not be read: {error}",
                        path.display()
                    ),
                    remediation: "remove the file to start a fresh guest state".to_string(),
                })
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuestState {
                schema: state_schema(),
                ..GuestState::default()
            },
            Err(error) => {
                return Err(LegError::Io(format!(
                    "could not read the guest state at {}: {error}",
                    path.display()
                )));
            }
        };
        state.dir = dir.to_path_buf();
        Ok(state)
    }

    /// Writes the state back to its directory.
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(|error| {
            LegError::Io(format!(
                "could not create the guest state directory {}: {error}",
                self.dir.display()
            ))
        })?;
        let path = self.dir.join(STATE_FILE);
        let text = serde_json::to_string_pretty(self).map_err(|error| {
            LegError::Io(format!("could not serialize the guest state: {error}"))
        })?;
        fs::write(&path, text).map_err(|error| {
            LegError::Io(format!(
                "could not write the guest state to {}: {error}",
                path.display()
            ))
        })
    }

    /// Records an installed package and its share of the storage budget.
    pub fn record_install(&mut self, package: &str, version: &str, install_bytes: u64) {
        let previous = self
            .installed
            .insert(
                package.to_string(),
                InstalledPackage {
                    version: version.to_string(),
                    install_bytes,
                },
            )
            .map(|entry| entry.install_bytes)
            .unwrap_or(0);
        self.provisioned_install_bytes = self
            .provisioned_install_bytes
            .saturating_sub(previous)
            .saturating_add(install_bytes);
    }

    /// Issues a one-time approval token for `candidate`.
    pub fn issue_approval(&self, package: &str, candidate: &Candidate) -> Result<String> {
        let directory = self.dir.join(APPROVALS_DIR);
        fs::create_dir_all(&directory).map_err(|error| {
            LegError::Io(format!(
                "could not create the approval directory {}: {error}",
                directory.display()
            ))
        })?;

        let token = generate_token();
        let approval = Approval {
            schema: approval_schema(),
            package: package.to_string(),
            candidate: candidate.clone(),
            issued_at: unix_seconds(),
        };
        let text = serde_json::to_string_pretty(&approval)
            .map_err(|error| LegError::Io(format!("could not serialize the approval: {error}")))?;
        fs::write(approval_path(&directory, &token), text).map_err(|error| {
            LegError::Io(format!("could not write the approval record: {error}"))
        })?;
        Ok(token)
    }

    /// Redeems an approval token, removing the record so it cannot be reused.
    pub fn take_approval(&self, token: &str) -> Result<Approval> {
        if !valid_token(token) {
            return Err(LegError::Guest(GuestFailure {
                reason: "approval_invalid",
                message: format!("the approval token {token} is not a pending approval"),
                remediation: "preview the package again to get a fresh token".to_string(),
            }));
        }
        let directory = self.dir.join(APPROVALS_DIR);
        let path = approval_path(&directory, token);
        let text = fs::read_to_string(&path).map_err(|_| {
            LegError::Guest(GuestFailure {
                reason: "approval_invalid",
                message: format!("the approval token {token} is not a pending approval"),
                remediation: "preview the package again to get a fresh token".to_string(),
            })
        })?;
        fs::remove_file(&path).map_err(|error| {
            LegError::Io(format!("could not remove the approval record: {error}"))
        })?;

        let approval: Approval = serde_json::from_str(&text).map_err(|error| {
            LegError::Guest(GuestFailure {
                reason: "approval_invalid",
                message: format!("the approval record could not be read: {error}"),
                remediation: "preview the package again to get a fresh token".to_string(),
            })
        })?;
        if unix_seconds().saturating_sub(approval.issued_at) > APPROVAL_TTL_SECONDS {
            return Err(LegError::Guest(GuestFailure {
                reason: "approval_expired",
                message: format!("the approval token {token} has expired"),
                remediation: "preview the package again to get a fresh token".to_string(),
            }));
        }
        Ok(approval)
    }
}

/// The path of the record for one approval token.
fn approval_path(directory: &Path, token: &str) -> PathBuf {
    directory.join(format!("{token}.json"))
}

/// Whether a token can name an approval record.
///
/// The tokens this module issues are hexadecimal, so a token that carries
/// anything else never names a record.
fn valid_token(token: &str) -> bool {
    (16..=128).contains(&token.len())
        && token.chars().all(|character| character.is_ascii_hexdigit())
}

/// The current time in seconds since the Unix epoch.
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Generates a one-time approval token.
fn generate_token() -> String {
    let bytes = random_bytes(24).unwrap_or_else(fallback_bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Reads random bytes from the operating system.
#[cfg(unix)]
fn random_bytes(length: usize) -> Option<Vec<u8>> {
    use std::io::Read;

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

/// Derives token bytes from the clock when the system source is unavailable.
fn fallback_bytes() -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    unix_seconds().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::time::Instant::now().elapsed().hash(&mut hasher);
    let digest = hasher.finish();
    digest.to_le_bytes().to_vec()
}
