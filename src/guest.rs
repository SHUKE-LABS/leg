//! The `leg guest` commands: inspecting and provisioning the pinned Alpine
//! guest that an iSH host runs.
//!
//! The flow is approval-gated and pinned:
//!
//! - only the four allowlisted packages can be provisioned, and only through
//!   the fixed apk arguments in [`system`];
//! - the candidate a preview reports is read from the repository index that
//!   apk itself downloaded and signature-verified, so the reported version,
//!   byte counts, and checksum are the ones the install would use;
//! - a preview issues a one-time approval token bound to that snapshot, and
//!   the approval re-resolves the snapshot before anything is installed.

pub mod index;
pub mod state;
pub mod system;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{GuestFailure, LegError, Result};

/// The Alpine release the pinned iSH bundle is built against.
pub const GUEST_RELEASE: &str = "3.20.3";

/// The guest architecture the pinned bundle targets.
pub const GUEST_ARCHITECTURE: &str = "i386";

/// The Alpine repository release the pinned repositories serve.
pub const REPOSITORY_RELEASE: &str = "v3.20";

/// The pinned repositories, in resolution order.
pub const REPOSITORY_URLS: [&str; 2] = [
    "https://dl-cdn.alpinelinux.org/alpine/v3.20/main",
    "https://dl-cdn.alpinelinux.org/alpine/v3.20/community",
];

/// The default storage budget: 256 MiB of installed packages.
pub const DEFAULT_STORAGE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// A package the guest is allowed to provision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowedPackage {
    /// The package name, as apk knows it.
    pub name: &'static str,
    /// The command that proves the installed executable works.
    pub verification: Verification,
}

/// The command that proves an installed package's executable works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verification {
    /// The executable to run.
    pub program: &'static str,
    /// Its fixed arguments.
    pub args: &'static [&'static str],
    /// The text the output must carry; empty means any successful run.
    pub expect: &'static str,
    /// Whether the output must also carry the candidate's upstream version.
    pub expect_version: bool,
}

/// The four packages the guest may provision.
pub const ALLOWED_PACKAGES: [AllowedPackage; 4] = [
    AllowedPackage {
        name: "bash",
        verification: Verification {
            program: "bash",
            args: &["-lc", "printf ready"],
            expect: "ready",
            expect_version: false,
        },
    },
    AllowedPackage {
        name: "ffmpeg",
        verification: Verification {
            program: "ffmpeg",
            args: &["-version"],
            expect: "",
            expect_version: true,
        },
    },
    AllowedPackage {
        name: "python3",
        verification: Verification {
            program: "python3",
            args: &["--version"],
            expect: "",
            expect_version: true,
        },
    },
    AllowedPackage {
        name: "ripgrep",
        verification: Verification {
            program: "rg",
            args: &["--version"],
            expect: "",
            expect_version: true,
        },
    },
];

/// A package candidate resolved from the pinned repository index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// The package name.
    pub package: String,
    /// The exact version the index offers.
    pub version: String,
    /// The pinned repository the candidate came from.
    pub repository: String,
    /// The architecture the candidate is built for.
    pub architecture: String,
    /// The size of the package file, in bytes.
    pub download_bytes: u64,
    /// The installed size of the package, in bytes.
    pub install_bytes: u64,
    /// The checksum the index records for the package file.
    pub checksum: String,
}

/// The guest-side operations the provisioning flow needs.
///
/// The seam exists so the flow can be driven against a real Alpine guest and
/// against fixtures alike; the real implementation lives in [`system`].
pub trait GuestEnvironment {
    /// The guest's Alpine release, for example `3.20.3`.
    fn release(&mut self) -> Result<String>;
    /// The guest's apk architecture, for example `x86`.
    fn architecture(&mut self) -> Result<String>;
    /// The versions of the named packages that are installed in the guest.
    fn installed_versions(&mut self, packages: &[&str]) -> Result<BTreeMap<String, String>>;
    /// The candidate the pinned repositories offer for `package`.
    fn candidate(&mut self, package: &str) -> Result<Candidate>;
    /// Installs `package` at `version` through the fixed apk transaction.
    fn install(&mut self, package: &str, version: &str) -> Result<()>;
    /// Removes `package` again, so a failed install leaves no residue.
    fn uninstall(&mut self, package: &str, version: &str) -> Result<()>;
    /// Runs `program` with `args` and returns its standard output.
    fn run(&mut self, program: &str, args: &[&str]) -> Result<String>;
    /// The bytes still available on the guest's storage.
    fn free_bytes(&mut self) -> Result<u64>;
}

/// What a `leg guest provision` request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionRequest {
    /// The package to provision; it must be on the allowlist.
    pub package: String,
    /// The approval token a previous preview issued, when the host approved.
    pub approval_token: Option<String>,
    /// The storage budget override, in bytes.
    pub storage_budget_bytes: Option<u64>,
}

/// The outcome of a provisioning request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionStatus {
    /// The request was previewed and needs an approval token to proceed.
    ApprovalRequired,
    /// The guest already holds the candidate, so nothing was installed.
    AlreadySatisfied,
    /// The package was installed and its executable verified.
    Installed,
}

impl ProvisionStatus {
    /// The stable machine-readable status string.
    pub fn as_str(self) -> &'static str {
        match self {
            ProvisionStatus::ApprovalRequired => "approval_required",
            ProvisionStatus::AlreadySatisfied => "already_satisfied",
            ProvisionStatus::Installed => "installed",
        }
    }
}

impl Serialize for ProvisionStatus {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The guest's own view of itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuestInfo {
    /// The guest's Alpine release.
    pub release: String,
    /// The guest's apk architecture.
    pub architecture: String,
}

/// The pinned repository identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryInfo {
    /// The Alpine repository release the pinned repositories serve.
    pub release: String,
    /// The pinned repository URLs, in resolution order.
    pub urls: Vec<String>,
}

/// The guest's storage situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StorageInfo {
    /// The bytes still available on the guest's storage.
    pub free_bytes: u64,
    /// The storage budget in force.
    pub budget_bytes: u64,
}

/// What `leg guest bootstrap` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootstrapReport {
    /// The report schema.
    pub schema: &'static str,
    /// The guest's release and architecture.
    pub guest: GuestInfo,
    /// The pinned repository identity.
    pub repository: RepositoryInfo,
    /// The allowlisted packages that are installed, and their versions.
    pub installed: BTreeMap<String, String>,
    /// The guest's storage situation.
    pub storage: StorageInfo,
}

/// The candidate a request would install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateReport {
    /// The exact version the index offers.
    pub version: String,
    /// The pinned repository the candidate came from.
    pub repository: String,
    /// The architecture the candidate is built for.
    pub architecture: String,
    /// The size of the package file, in bytes.
    pub download_bytes: u64,
    /// The installed size of the package, in bytes.
    pub install_bytes: u64,
    /// The checksum the index records for the package file.
    pub checksum: String,
}

impl From<&Candidate> for CandidateReport {
    fn from(candidate: &Candidate) -> Self {
        CandidateReport {
            version: candidate.version.clone(),
            repository: candidate.repository.clone(),
            architecture: candidate.architecture.clone(),
            download_bytes: candidate.download_bytes,
            install_bytes: candidate.install_bytes,
            checksum: candidate.checksum.clone(),
        }
    }
}

/// What `leg guest provision` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvisionReport {
    /// The report schema.
    pub schema: &'static str,
    /// The outcome of the request.
    pub status: ProvisionStatus,
    /// The package the request named.
    pub package: String,
    /// Whether an approval token is still needed.
    pub approval_required: bool,
    /// The one-time approval token, when the request needs one.
    pub approval_token: Option<String>,
    /// The candidate the request resolved.
    pub candidate: CandidateReport,
    /// Whether the installed executable was verified.
    pub verified: bool,
    /// Whether the guest already held the candidate, so nothing was fetched.
    pub cache_hit: bool,
    /// The guest's storage situation.
    pub storage: StorageInfo,
}

/// The names of the allowlisted packages.
pub fn allowed_names() -> Vec<&'static str> {
    ALLOWED_PACKAGES
        .iter()
        .map(|package| package.name)
        .collect()
}

/// The allowlisted package with this name.
pub fn allowed_package(name: &str) -> Option<&'static AllowedPackage> {
    ALLOWED_PACKAGES.iter().find(|package| package.name == name)
}

/// Whether an apk architecture is the one the pinned guest runs.
pub fn architecture_matches(apk_architecture: &str) -> bool {
    apk_architecture == "x86" || apk_architecture == GUEST_ARCHITECTURE
}

/// Whether a guest release is the one the pinned bundle is built against.
pub fn release_matches(release: &str) -> bool {
    release == GUEST_RELEASE
}

/// The upstream version of an Alpine package version: `6.1.1-r8` is `6.1.1`.
pub fn upstream_version(version: &str) -> &str {
    match version.rsplit_once('-') {
        Some((upstream, _)) if !upstream.is_empty() => upstream,
        _ => version,
    }
}

/// Builds a structured guest failure.
fn failure(
    reason: &'static str,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> LegError {
    LegError::Guest(GuestFailure {
        reason,
        message: message.into(),
        remediation: remediation.into(),
    })
}

/// Reports the guest's release, architecture, pinned repository, installed
/// allowlisted packages, and free storage.
///
/// A guest that is not the pinned one is refused with the structured error
/// instead of a report, so an incompatible guest never reads as a healthy one.
pub fn bootstrap(env: &mut dyn GuestEnvironment) -> Result<BootstrapReport> {
    let release = env.release()?;
    let architecture = env.architecture()?;
    ensure_supported_guest(&release, &architecture)?;

    let installed = env.installed_versions(&allowed_names())?;
    let free_bytes = env.free_bytes()?;

    Ok(BootstrapReport {
        schema: "leg.guest.bootstrap/v1",
        guest: GuestInfo {
            release,
            architecture,
        },
        repository: RepositoryInfo {
            release: REPOSITORY_RELEASE.to_string(),
            urls: REPOSITORY_URLS.iter().map(|url| url.to_string()).collect(),
        },
        installed,
        storage: StorageInfo {
            free_bytes,
            budget_bytes: DEFAULT_STORAGE_BUDGET_BYTES,
        },
    })
}

/// Refuses a guest that is not the pinned release and architecture.
fn ensure_supported_guest(release: &str, architecture: &str) -> Result<()> {
    if release_matches(release) && architecture_matches(architecture) {
        return Ok(());
    }
    Err(failure(
        "guest_incompatible",
        format!(
            "the guest runs Alpine {release} on {architecture}, not the pinned \
             Alpine {GUEST_RELEASE} on {GUEST_ARCHITECTURE}"
        ),
        "run the guest command inside the pinned iSH bundle",
    ))
}

/// Previews or applies one provisioning request.
///
/// Without an approval token the request is only previewed: the candidate is
/// resolved, a one-time token is issued for it, and the guest is left
/// untouched. With a valid token the snapshot is re-resolved and compared,
/// the package is installed through the fixed apk transaction, and its
/// executable is verified.
pub fn provision(
    env: &mut dyn GuestEnvironment,
    request: &ProvisionRequest,
    state: &mut state::GuestState,
) -> Result<ProvisionReport> {
    let package = allowed_package(&request.package).ok_or_else(|| {
        failure(
            "not_allowlisted",
            format!("{} is not an approved guest package", request.package),
            format!("provision one of {} instead", allowed_names().join(", ")),
        )
    })?;

    let budget = request
        .storage_budget_bytes
        .unwrap_or(DEFAULT_STORAGE_BUDGET_BYTES);

    let release = env.release()?;
    let architecture = env.architecture()?;
    ensure_supported_guest(&release, &architecture)?;

    let free_bytes = env.free_bytes()?;
    let candidate = env.candidate(package.name)?;
    if !architecture_matches(&candidate.architecture) {
        return Err(failure(
            "architecture_unsupported",
            format!(
                "{} {} is built for {}, not {}",
                candidate.package, candidate.version, candidate.architecture, architecture
            ),
            "the pinned repositories do not carry this package for the guest architecture",
        ));
    }

    let installed = env.installed_versions(&allowed_names())?;
    let storage = StorageInfo {
        free_bytes,
        budget_bytes: budget,
    };

    if installed.get(package.name) == Some(&candidate.version) {
        state.record_install(package.name, &candidate.version, candidate.install_bytes);
        state.save()?;
        return Ok(ProvisionReport {
            schema: "leg.guest.provision/v1",
            status: ProvisionStatus::AlreadySatisfied,
            package: package.name.to_string(),
            approval_required: false,
            approval_token: None,
            candidate: CandidateReport::from(&candidate),
            verified: false,
            cache_hit: true,
            storage,
        });
    }

    let projected = state
        .provisioned_install_bytes
        .saturating_add(candidate.install_bytes);
    if projected > budget {
        return Err(failure(
            "budget_exceeded",
            format!(
                "provisioning {} needs {projected} bytes, over the {budget} byte budget",
                candidate.package
            ),
            "raise --storage-budget-bytes or provision fewer packages",
        ));
    }

    let needed = candidate
        .download_bytes
        .saturating_add(candidate.install_bytes);
    if free_bytes < needed {
        return Err(failure(
            "insufficient_space",
            format!(
                "provisioning {} needs {needed} bytes but only {free_bytes} are free",
                candidate.package
            ),
            "free space in the guest before provisioning",
        ));
    }

    let Some(token) = request.approval_token.as_deref() else {
        let token = state.issue_approval(package.name, &candidate)?;
        return Ok(ProvisionReport {
            schema: "leg.guest.provision/v1",
            status: ProvisionStatus::ApprovalRequired,
            package: package.name.to_string(),
            approval_required: true,
            approval_token: Some(token),
            candidate: CandidateReport::from(&candidate),
            verified: false,
            cache_hit: false,
            storage,
        });
    };

    let approval = state.take_approval(token)?;
    if approval.package != package.name {
        return Err(failure(
            "approval_mismatch",
            format!(
                "the approval token was issued for {}, not {}",
                approval.package, package.name
            ),
            "preview the package again to get a token for it",
        ));
    }
    if approval.candidate != candidate {
        return Err(failure(
            "snapshot_changed",
            format!(
                "the pinned repositories now offer {} {}, not the approved {}",
                candidate.package, candidate.version, approval.candidate.version
            ),
            "preview the package again to approve the current candidate",
        ));
    }

    if let Err(error) = install_and_verify(env, state, package, &candidate) {
        return Err(rollback(
            env,
            state,
            package.name,
            &candidate.version,
            error,
        ));
    }

    Ok(ProvisionReport {
        schema: "leg.guest.provision/v1",
        status: ProvisionStatus::Installed,
        package: package.name.to_string(),
        approval_required: false,
        approval_token: None,
        candidate: CandidateReport::from(&candidate),
        verified: true,
        cache_hit: false,
        storage,
    })
}

/// Installs the candidate, verifies its executable, and records it.
///
/// The three steps are one transaction: when any of them fails, the caller
/// rolls the install back.
fn install_and_verify(
    env: &mut dyn GuestEnvironment,
    state: &mut state::GuestState,
    package: &AllowedPackage,
    candidate: &Candidate,
) -> Result<()> {
    env.install(package.name, &candidate.version)?;
    verify_executable(env, package, &candidate.version)?;
    state.record_install(package.name, &candidate.version, candidate.install_bytes);
    state.save()
}

/// Rolls a failed install back, so the guest keeps the state it had.
///
/// The package is removed again and the state record is dropped; when the
/// removal fails the guest may still hold the package, which the failure
/// says.
fn rollback(
    env: &mut dyn GuestEnvironment,
    state: &mut state::GuestState,
    package: &str,
    version: &str,
    error: LegError,
) -> LegError {
    state.forget(package);
    match env.uninstall(package, version) {
        Ok(()) => error,
        Err(uninstall_error) => failure(
            "rollback_failed",
            format!("{error}; the package could not be removed: {uninstall_error}"),
            "the guest may still hold the package; remove it by hand",
        ),
    }
}

/// Runs the package's fixed verification command and checks its output.
fn verify_executable(
    env: &mut dyn GuestEnvironment,
    package: &AllowedPackage,
    version: &str,
) -> Result<()> {
    let verification = package.verification;
    let output = env.run(verification.program, verification.args)?;
    let missing = if !verification.expect.is_empty() && !output.contains(verification.expect) {
        Some(verification.expect.to_string())
    } else if verification.expect_version {
        let upstream = upstream_version(version);
        if !output.contains(upstream) {
            Some(upstream.to_string())
        } else {
            None
        }
    } else {
        None
    };

    match missing {
        None => Ok(()),
        Some(expected) => Err(failure(
            "verification_failed",
            format!(
                "{} did not report {expected} when it was run",
                verification.program
            ),
            format!(
                "reinstall {} from the pinned repositories before using it",
                package.name
            ),
        )),
    }
}
