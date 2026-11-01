use super::index;
use super::state::{Approval, GuestState, InstalledPackage};
use super::*;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::error::{GuestFailure, LegError, Result};

/// The pinned main repository index fixture.
const MAIN_INDEX_FIXTURE: &str = include_str!("fixtures/APKINDEX-main");

/// The pinned community repository index fixture.
const COMMUNITY_INDEX_FIXTURE: &str = include_str!("fixtures/APKINDEX-community");

/// A scratch directory removed when the test ends.
struct Scratch {
    /// The scratch directory itself.
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Creates a scratch directory named after the test.
fn scratch(name: &str) -> Scratch {
    let path = std::env::temp_dir().join(format!("leg-guest-tests-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch directory");
    Scratch { path }
}

/// A stand-in for the Alpine guest, driven by the test.
struct FakeGuest {
    /// The release the guest reports.
    release: String,
    /// The architecture the guest reports.
    architecture: String,
    /// The packages the guest has installed.
    installed: BTreeMap<String, String>,
    /// The candidate the pinned repositories offer.
    candidate: Option<Candidate>,
    /// The failure the candidate resolution reports.
    candidate_failure: Option<GuestFailure>,
    /// The bytes the guest reports as free.
    free_bytes: u64,
    /// The output the verification command prints.
    verification_output: String,
    /// The failure the verification command reports.
    verification_failure: Option<GuestFailure>,
    /// The failure the install reports.
    install_failure: Option<GuestFailure>,
    /// The installs the flow performed.
    installs: Vec<(String, String)>,
    /// The removals the flow performed.
    uninstalls: Vec<(String, String)>,
    /// The programs the flow ran.
    runs: Vec<String>,
}

impl FakeGuest {
    /// A guest that resolves one candidate and has room to spare.
    fn new() -> Self {
        FakeGuest {
            release: "3.20.3".to_string(),
            architecture: "x86".to_string(),
            installed: BTreeMap::new(),
            candidate: Some(candidate("bash", "5.2.26-r0")),
            candidate_failure: None,
            free_bytes: 1024 * 1024 * 1024,
            verification_output: "ready".to_string(),
            verification_failure: None,
            install_failure: None,
            installs: Vec::new(),
            uninstalls: Vec::new(),
            runs: Vec::new(),
        }
    }
}

impl GuestEnvironment for FakeGuest {
    fn release(&mut self) -> Result<String> {
        Ok(self.release.clone())
    }

    fn architecture(&mut self) -> Result<String> {
        Ok(self.architecture.clone())
    }

    fn installed_versions(&mut self, _packages: &[&str]) -> Result<BTreeMap<String, String>> {
        Ok(self.installed.clone())
    }

    fn candidate(&mut self, _package: &str) -> Result<Candidate> {
        match &self.candidate_failure {
            Some(failure) => Err(LegError::Guest(failure.clone())),
            None => Ok(self.candidate.clone().expect("a candidate")),
        }
    }

    fn install(&mut self, package: &str, version: &str) -> Result<()> {
        if let Some(failure) = &self.install_failure {
            return Err(LegError::Guest(failure.clone()));
        }
        self.installs
            .push((package.to_string(), version.to_string()));
        self.installed
            .insert(package.to_string(), version.to_string());
        Ok(())
    }

    fn uninstall(&mut self, package: &str, version: &str) -> Result<()> {
        if self.installed.remove(package).is_some() {
            self.uninstalls
                .push((package.to_string(), version.to_string()));
        }
        Ok(())
    }

    fn run(&mut self, program: &str, _args: &[&str]) -> Result<String> {
        self.runs.push(program.to_string());
        match &self.verification_failure {
            Some(failure) => Err(LegError::Guest(failure.clone())),
            None => Ok(self.verification_output.clone()),
        }
    }

    fn free_bytes(&mut self) -> Result<u64> {
        Ok(self.free_bytes)
    }
}

/// The candidate the fixture index offers.
fn candidate(package: &str, version: &str) -> Candidate {
    Candidate {
        package: package.to_string(),
        version: version.to_string(),
        repository: REPOSITORY_URLS[0].to_string(),
        architecture: "x86".to_string(),
        download_bytes: 459_098,
        install_bytes: 1_417_216,
        checksum: "sha1:5313307c0b7ae9288682f6402ade02cbf45042b7".to_string(),
    }
}

/// A structured guest failure.
fn failure(reason: &'static str) -> GuestFailure {
    GuestFailure {
        reason,
        message: format!("the guest reported {reason}"),
        remediation: "try again".to_string(),
    }
}

/// The reason a failure carries.
fn reason_of(error: &LegError) -> &str {
    match error {
        LegError::Guest(failure) => failure.reason,
        other => panic!("expected a guest failure, got {other}"),
    }
}

/// A request for one package.
fn request(package: &str) -> ProvisionRequest {
    ProvisionRequest {
        package: package.to_string(),
        approval_token: None,
        storage_budget_bytes: None,
    }
}

/// A state held in a scratch directory.
fn state(scratch: &Scratch) -> GuestState {
    GuestState::load(&scratch.path).expect("load the guest state")
}

/// The packages the persisted guest state records.
fn persisted(scratch: &Scratch) -> BTreeMap<String, InstalledPackage> {
    GuestState::load(&scratch.path)
        .expect("the persisted state is readable")
        .installed
}

#[test]
fn bootstrap_reports_the_guest_the_pinned_repository_and_the_free_storage() {
    let mut guest = FakeGuest::new();
    guest
        .installed
        .insert("bash".to_string(), "5.2.26-r0".to_string());
    guest.free_bytes = 4096;

    let report = bootstrap(&mut guest).expect("bootstrap succeeds");

    assert_eq!(report.schema, "leg.guest.bootstrap/v1");
    assert_eq!(report.guest.release, "3.20.3");
    assert_eq!(report.guest.architecture, "x86");
    assert_eq!(report.repository.release, "v3.20");
    assert_eq!(
        report.repository.urls,
        vec![
            REPOSITORY_URLS[0].to_string(),
            REPOSITORY_URLS[1].to_string()
        ]
    );
    assert_eq!(report.installed.get("bash"), Some(&"5.2.26-r0".to_string()));
    assert_eq!(report.storage.free_bytes, 4096);
    assert_eq!(report.storage.budget_bytes, DEFAULT_STORAGE_BUDGET_BYTES);
}

#[test]
fn a_guest_on_another_patch_release_is_refused() {
    let scratch = scratch("patch-release");
    let mut guest = FakeGuest::new();
    guest.release = "3.20.1".to_string();
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the guest is refused");

    assert_eq!(reason_of(&error), "guest_incompatible");
    assert!(guest.installs.is_empty());
}

#[test]
fn bootstrap_refuses_a_guest_that_is_not_the_pinned_one() {
    let mut guest = FakeGuest::new();
    guest.release = "3.19.1".to_string();

    let error = bootstrap(&mut guest).expect_err("the guest is refused");

    assert_eq!(reason_of(&error), "guest_incompatible");
}

#[test]
fn bootstrap_refuses_a_guest_on_another_architecture() {
    let mut guest = FakeGuest::new();
    guest.architecture = "x86_64".to_string();

    let error = bootstrap(&mut guest).expect_err("the guest is refused");

    assert_eq!(reason_of(&error), "guest_incompatible");
}

#[test]
fn a_preview_reports_the_candidate_and_leaves_the_guest_unchanged() {
    let scratch = scratch("preview");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let report = provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");

    assert_eq!(report.status, ProvisionStatus::ApprovalRequired);
    assert!(report.approval_required);
    assert!(report.approval_token.is_some());
    assert_eq!(report.candidate.version, "5.2.26-r0");
    assert_eq!(report.candidate.repository, REPOSITORY_URLS[0]);
    assert_eq!(report.candidate.download_bytes, 459_098);
    assert_eq!(report.candidate.install_bytes, 1_417_216);
    assert!(!report.verified);
    assert!(!report.cache_hit);
    assert!(guest.installs.is_empty(), "the preview installed something");
    assert!(guest.runs.is_empty(), "the preview ran an executable");
}

#[test]
fn an_approved_request_installs_the_candidate_and_verifies_the_executable() {
    let scratch = scratch("approved");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");

    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let report = provision(&mut guest, &approved, &mut state).expect("the install succeeds");

    assert_eq!(report.status, ProvisionStatus::Installed);
    assert!(!report.approval_required);
    assert!(report.verified);
    assert!(!report.cache_hit);
    assert_eq!(
        guest.installs,
        vec![("bash".to_string(), "5.2.26-r0".to_string())]
    );
    assert_eq!(guest.runs, vec!["bash".to_string()]);
    assert_eq!(
        state
            .installed
            .get("bash")
            .map(|entry| entry.version.clone()),
        Some("5.2.26-r0".to_string())
    );
    assert_eq!(state.provisioned_install_bytes, 1_417_216);
}

#[test]
fn an_unknown_approval_token_is_refused_without_touching_the_guest() {
    let scratch = scratch("bad-token");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some("not-a-token".to_string()),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the token is refused");

    assert_eq!(reason_of(&error), "approval_invalid");
    assert!(guest.installs.is_empty());
    assert!(state.installed.is_empty());
}

#[test]
fn an_approval_token_is_redeemed_only_once() {
    let scratch = scratch("single-use");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");
    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token.clone()),
        storage_budget_bytes: None,
    };
    provision(&mut guest, &approved, &mut state).expect("the install succeeds");

    let mut second_guest = FakeGuest::new();
    let error =
        provision(&mut second_guest, &approved, &mut state).expect_err("the token is spent");
    assert_eq!(reason_of(&error), "approval_invalid");
    assert!(second_guest.installs.is_empty());
}

#[test]
fn an_installed_package_is_reported_without_a_second_install() {
    let scratch = scratch("cache-hit");
    let mut guest = FakeGuest::new();
    guest
        .installed
        .insert("bash".to_string(), "5.2.26-r0".to_string());
    let mut state = state(&scratch);

    let report = provision(&mut guest, &request("bash"), &mut state).expect("the request succeeds");

    assert_eq!(report.status, ProvisionStatus::AlreadySatisfied);
    assert!(report.cache_hit);
    assert!(!report.approval_required);
    assert!(report.approval_token.is_none());
    assert!(
        guest.installs.is_empty(),
        "the satisfied package was installed again"
    );
}

#[test]
fn a_package_outside_the_allowlist_is_refused_before_the_guest_is_read() {
    let scratch = scratch("not-allowlisted");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("curl"), &mut state).expect_err("the package is refused");

    assert_eq!(reason_of(&error), "not_allowlisted");
    assert!(guest.installs.is_empty());
}

#[test]
fn an_offline_guest_fails_before_anything_is_installed() {
    let scratch = scratch("offline");
    let mut guest = FakeGuest::new();
    guest.candidate_failure = Some(failure("offline"));
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the guest is offline");

    assert_eq!(reason_of(&error), "offline");
    assert!(guest.installs.is_empty());
    assert!(state.installed.is_empty());
}

#[test]
fn an_untrusted_repository_signature_fails_before_anything_is_installed() {
    let scratch = scratch("signature");
    let mut guest = FakeGuest::new();
    guest.candidate_failure = Some(failure("signature_invalid"));
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the signature is refused");

    assert_eq!(reason_of(&error), "signature_invalid");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_checksum_mismatch_fails_before_anything_is_installed() {
    let scratch = scratch("checksum");
    let mut guest = FakeGuest::new();
    guest.candidate_failure = Some(failure("checksum_mismatch"));
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the checksum is refused");

    assert_eq!(reason_of(&error), "checksum_mismatch");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_conflicting_package_fails_before_anything_is_installed() {
    let scratch = scratch("conflict");
    let mut guest = FakeGuest::new();
    guest.candidate_failure = Some(failure("conflict"));
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the conflict is refused");

    assert_eq!(reason_of(&error), "conflict");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_candidate_for_another_architecture_is_refused() {
    let scratch = scratch("architecture");
    let mut guest = FakeGuest::new();
    guest.candidate = Some(Candidate {
        architecture: "x86_64".to_string(),
        ..candidate("bash", "5.2.26-r0")
    });
    let mut state = state(&scratch);

    let error = provision(&mut guest, &request("bash"), &mut state)
        .expect_err("the architecture is refused");

    assert_eq!(reason_of(&error), "architecture_unsupported");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_guest_on_another_release_is_refused() {
    let scratch = scratch("incompatible");
    let mut guest = FakeGuest::new();
    guest.release = "3.19.1".to_string();
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the guest is refused");

    assert_eq!(reason_of(&error), "guest_incompatible");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_request_over_the_storage_budget_is_refused() {
    let scratch = scratch("budget");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let over_budget = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: None,
        storage_budget_bytes: Some(1_000),
    };
    let error =
        provision(&mut guest, &over_budget, &mut state).expect_err("the budget is exceeded");

    assert_eq!(reason_of(&error), "budget_exceeded");
    assert!(guest.installs.is_empty());
    assert!(state.installed.is_empty());
}

#[test]
fn the_provisioned_bytes_count_towards_the_budget() {
    let scratch = scratch("budget-carry");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);
    state.record_install("bash", "5.2.26-r0", 1_417_216);

    let over_budget = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: None,
        storage_budget_bytes: Some(1_500_000),
    };
    let error =
        provision(&mut guest, &over_budget, &mut state).expect_err("the budget is exceeded");

    assert_eq!(reason_of(&error), "budget_exceeded");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_request_without_free_space_is_refused() {
    let scratch = scratch("space");
    let mut guest = FakeGuest::new();
    guest.free_bytes = 10;
    let mut state = state(&scratch);

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the space is short");

    assert_eq!(reason_of(&error), "insufficient_space");
    assert!(guest.installs.is_empty());
}

#[test]
fn a_candidate_that_changed_after_the_preview_is_refused() {
    let scratch = scratch("snapshot");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");

    guest.candidate = Some(candidate("bash", "5.2.26-r1"));
    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the snapshot changed");

    assert_eq!(reason_of(&error), "snapshot_changed");
    assert!(guest.installs.is_empty());
    assert!(state.installed.is_empty());
}

#[test]
fn a_failed_verification_rolls_the_install_back() {
    let scratch = scratch("verification");
    let mut guest = FakeGuest::new();
    guest.verification_output = String::new();
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");
    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the verification fails");

    assert_eq!(reason_of(&error), "verification_failed");
    assert_eq!(guest.installs.len(), 1, "the package was never installed");
    assert_eq!(
        guest.uninstalls,
        vec![("bash".to_string(), "5.2.26-r0".to_string())],
        "the package was not rolled back"
    );
    assert!(
        !guest.installed.contains_key("bash"),
        "the guest still holds the package"
    );
    assert!(state.installed.is_empty(), "the state kept the package");
    assert_eq!(state.provisioned_install_bytes, 0);
    assert!(
        persisted(&scratch).is_empty(),
        "the persisted state kept the package"
    );
}

#[test]
fn a_guest_holding_another_version_is_refused_before_anything_is_installed() {
    let scratch = scratch("version-mismatch");
    let mut guest = FakeGuest::new();
    guest
        .installed
        .insert("bash".to_string(), "5.2.25-r0".to_string());
    let mut state = state(&scratch);
    state.record_install("bash", "5.2.25-r0", 1_000_000);
    state.save().expect("the state is written");

    let error =
        provision(&mut guest, &request("bash"), &mut state).expect_err("the guest is refused");

    assert_eq!(reason_of(&error), "version_mismatch");
    assert!(guest.installs.is_empty());
    assert_eq!(
        guest.installed.get("bash"),
        Some(&"5.2.25-r0".to_string()),
        "the guest lost the version it held"
    );
    assert_eq!(
        persisted(&scratch)
            .get("bash")
            .map(|entry| entry.version.clone()),
        Some("5.2.25-r0".to_string()),
        "the persisted state lost the version it held"
    );
}

#[test]
fn a_failed_install_keeps_the_previous_state_record() {
    let scratch = scratch("previous-record");
    let mut guest = FakeGuest::new();
    guest.install_failure = Some(failure("apk_failed"));
    let mut state = state(&scratch);
    state.record_install("bash", "5.2.25-r0", 1_000_000);
    state.save().expect("the state is written");

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");
    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the install fails");

    assert_eq!(reason_of(&error), "apk_failed");
    assert_eq!(
        state
            .installed
            .get("bash")
            .map(|entry| entry.version.clone()),
        Some("5.2.25-r0".to_string()),
        "the state lost the record it held"
    );
    assert_eq!(state.provisioned_install_bytes, 1_000_000);
    assert_eq!(
        persisted(&scratch)
            .get("bash")
            .map(|entry| entry.version.clone()),
        Some("5.2.25-r0".to_string()),
        "the persisted state lost the record it held"
    );
}

#[test]
fn a_state_that_cannot_be_saved_keeps_the_previous_state_record() {
    let scratch = scratch("previous-record-save");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);
    state.record_install("bash", "5.2.25-r0", 1_000_000);
    state.save().expect("the state is written");

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");

    // A directory where the state file belongs makes writing the state fail.
    let state_file = scratch.path.join("state.json");
    let _ = fs::remove_file(&state_file);
    fs::create_dir(&state_file).expect("replace the state file with a directory");

    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the state is not saved");

    assert!(
        matches!(error, LegError::Io(_)),
        "unexpected error: {error}"
    );
    assert_eq!(
        state
            .installed
            .get("bash")
            .map(|entry| entry.version.clone()),
        Some("5.2.25-r0".to_string()),
        "the state lost the record it held"
    );
    assert_eq!(state.provisioned_install_bytes, 1_000_000);
    assert!(
        !guest.installed.contains_key("bash"),
        "the guest still holds the package"
    );
}

#[test]
fn a_failed_install_leaves_the_guest_unchanged() {
    let scratch = scratch("install-failure");
    let mut guest = FakeGuest::new();
    guest.install_failure = Some(failure("apk_failed"));
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");
    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the install fails");

    assert_eq!(reason_of(&error), "apk_failed");
    assert!(guest.installs.is_empty());
    assert!(
        guest.uninstalls.is_empty(),
        "a package the guest never held was removed"
    );
    assert!(state.installed.is_empty());
    assert_eq!(state.provisioned_install_bytes, 0);
    assert!(
        persisted(&scratch).is_empty(),
        "the persisted state kept the package"
    );
}

#[test]
fn a_state_that_cannot_be_saved_rolls_the_install_back() {
    let scratch = scratch("state-save");
    let mut guest = FakeGuest::new();
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("bash"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");

    // A directory where the state file belongs makes writing the state fail.
    let state_file = scratch.path.join("state.json");
    let _ = fs::remove_file(&state_file);
    fs::create_dir(&state_file).expect("replace the state file with a directory");

    let approved = ProvisionRequest {
        package: "bash".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error = provision(&mut guest, &approved, &mut state).expect_err("the state is not saved");

    assert!(
        matches!(error, LegError::Io(_)),
        "unexpected error: {error}"
    );
    assert_eq!(
        guest.uninstalls,
        vec![("bash".to_string(), "5.2.26-r0".to_string())],
        "the package was not rolled back"
    );
    assert!(
        !guest.installed.contains_key("bash"),
        "the guest still holds the package"
    );
    assert!(state.installed.is_empty(), "the state kept the package");
    assert_eq!(state.provisioned_install_bytes, 0);
    assert!(
        state_file.is_dir(),
        "the persisted state was replaced by the rollback"
    );
}

#[test]
fn a_version_check_needs_the_candidate_version_in_the_output() {
    let scratch = scratch("version-check");
    let mut guest = FakeGuest::new();
    guest.candidate = Some(candidate("python3", "3.12.13-r0"));
    guest.verification_output = "Python 3.11.0".to_string();
    let mut state = state(&scratch);

    let preview =
        provision(&mut guest, &request("python3"), &mut state).expect("the preview succeeds");
    let token = preview.approval_token.expect("a token");
    let approved = ProvisionRequest {
        package: "python3".to_string(),
        approval_token: Some(token),
        storage_budget_bytes: None,
    };
    let error =
        provision(&mut guest, &approved, &mut state).expect_err("the version does not match");

    assert_eq!(reason_of(&error), "verification_failed");
}

#[test]
fn the_upstream_version_drops_the_alpine_revision() {
    assert_eq!(upstream_version("6.1.1-r8"), "6.1.1");
    assert_eq!(upstream_version("14.1.0-r0"), "14.1.0");
    assert_eq!(upstream_version("3.12.13"), "3.12.13");
}

#[test]
fn the_allowlist_holds_the_four_approved_packages() {
    assert_eq!(
        allowed_names(),
        vec!["bash", "ffmpeg", "python3", "ripgrep"]
    );
    assert_eq!(
        allowed_package("bash").map(|package| package.verification.args),
        Some(&["-lc", "printf ready"][..])
    );
    assert_eq!(allowed_package("curl"), None);
}

#[test]
fn an_issued_approval_binds_the_candidate_it_was_issued_for() {
    let scratch = scratch("approval");
    let state = state(&scratch);

    let token = state
        .issue_approval("bash", &candidate("bash", "5.2.26-r0"))
        .expect("the approval is issued");

    let approval: Approval = state
        .take_approval(&token)
        .expect("the approval is redeemed");
    assert_eq!(approval.package, "bash");
    assert_eq!(approval.candidate.version, "5.2.26-r0");
    assert_eq!(approval.candidate.install_bytes, 1_417_216);

    let error = state
        .take_approval(&token)
        .expect_err("the approval is spent");
    assert_eq!(reason_of(&error), "approval_invalid");
}

#[test]
fn the_state_records_the_installed_packages_and_their_bytes() {
    let scratch = scratch("state");
    let mut state = state(&scratch);

    state.record_install("bash", "5.2.26-r0", 1_417_216);
    state.save().expect("the state is written");

    let reloaded = GuestState::load(&scratch.path).expect("the state is read back");
    assert_eq!(
        reloaded.installed.get("bash"),
        Some(&InstalledPackage {
            version: "5.2.26-r0".to_string(),
            install_bytes: 1_417_216,
        })
    );
    assert_eq!(reloaded.provisioned_install_bytes, 1_417_216);
}

#[test]
fn recording_the_same_package_twice_counts_its_bytes_once() {
    let scratch = scratch("state-twice");
    let mut state = state(&scratch);

    state.record_install("bash", "5.2.26-r0", 1_417_216);
    state.record_install("bash", "5.2.26-r0", 1_417_216);

    assert_eq!(state.provisioned_install_bytes, 1_417_216);
}

#[test]
fn the_index_fixtures_yield_the_pinned_candidate_metadata() {
    let main = index::parse_index_text(MAIN_INDEX_FIXTURE);
    let community = index::parse_index_text(COMMUNITY_INDEX_FIXTURE);

    let bash = main
        .iter()
        .find(|entry| entry.name == "bash")
        .expect("bash is in the main fixture");
    assert_eq!(bash.version, "5.2.26-r0");
    assert_eq!(bash.architecture, "x86");
    assert_eq!(bash.download_bytes, 459_098);
    assert_eq!(bash.install_bytes, 1_417_216);
    assert_eq!(
        bash.checksum,
        "sha1:5313307c0b7ae9288682f6402ade02cbf45042b7"
    );

    let ffmpeg = community
        .iter()
        .find(|entry| entry.name == "ffmpeg")
        .expect("ffmpeg is in the community fixture");
    assert_eq!(ffmpeg.version, "6.1.1-r8");
    assert_eq!(ffmpeg.download_bytes, 245_732);
    assert_eq!(ffmpeg.install_bytes, 626_688);
}

#[test]
fn the_index_fixtures_hold_every_allowlisted_package() {
    let main = index::parse_index_text(MAIN_INDEX_FIXTURE);
    let community = index::parse_index_text(COMMUNITY_INDEX_FIXTURE);
    for name in allowed_names() {
        assert!(
            main.iter().any(|entry| entry.name == name)
                || community.iter().any(|entry| entry.name == name),
            "{name} is missing from the index fixtures"
        );
    }
}

#[test]
fn an_index_archive_yields_its_entries() {
    let archive = index_archive("APKINDEX", COMMUNITY_INDEX_FIXTURE.as_bytes());
    let entries = index::parse_index_archive(&archive).expect("the archive is parsed");

    assert_eq!(
        entries.len(),
        index::parse_index_text(COMMUNITY_INDEX_FIXTURE).len()
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "ripgrep" && entry.version == "14.1.0-r0")
    );
}

#[test]
fn an_archive_without_an_index_is_reported() {
    let archive = index_archive("OTHER", b"not an index");
    let error = index::parse_index_archive(&archive).expect_err("there is no index member");
    assert!(error.contains("APKINDEX"), "unexpected error: {error}");
}

#[test]
fn a_checksum_is_rendered_as_a_digest() {
    assert_eq!(
        index::format_checksum("Q1UxMwfAt66SiGgvZAKt4Cy/RQQrc="),
        "sha1:5313307c0b7ae9288682f6402ade02cbf45042b7"
    );
    assert_eq!(index::format_checksum("Q2YWJj"), "sha256:616263");
    assert_eq!(index::format_checksum("unparsable"), "unparsable");
}

#[test]
fn an_index_entry_without_a_version_is_skipped() {
    let entries = index::parse_index_text("P:bash\nA:x86\n\n");
    assert!(entries.is_empty());
}

#[test]
fn the_architecture_of_the_pinned_guest_is_recognized() {
    assert!(architecture_matches("x86"));
    assert!(architecture_matches("i386"));
    assert!(!architecture_matches("x86_64"));
    assert!(!architecture_matches("aarch64"));
}

/// Builds a gzipped tar holding one `APKINDEX` member.
fn index_archive(member: &str, index: &[u8]) -> Vec<u8> {
    let mut tar = vec![0u8; 512];
    let name = format!("{member}\0");
    tar[0..name.len()].copy_from_slice(name.as_bytes());
    tar[100..107].copy_from_slice(b"0000644");
    tar[108..115].copy_from_slice(b"0000000");
    tar[116..123].copy_from_slice(b"0000000");
    tar[124..136].copy_from_slice(format!("{:011o}\0", index.len()).as_bytes());
    tar[136..147].copy_from_slice(b"00000000000");
    tar[148..156].copy_from_slice(b"        ");
    tar[156] = b'0';
    tar[257..262].copy_from_slice(b"ustar");
    tar[263..265].copy_from_slice(b"00");

    tar.extend_from_slice(index);
    let padding = (512 - index.len() % 512) % 512;
    tar.extend(std::iter::repeat_n(0u8, padding));
    tar.extend(std::iter::repeat_n(0u8, 1024));

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar).expect("the archive is compressed");
    encoder.finish().expect("the archive is finished")
}
