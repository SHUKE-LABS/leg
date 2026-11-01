//! The real guest: the local Alpine installation, driven through apk.
//!
//! Every apk invocation uses fixed arguments. The package name and version
//! come from the allowlist and from the repository index apk itself
//! downloaded and verified, never from the caller, and the repositories are
//! pinned to the URLs in [`crate::guest::REPOSITORY_URLS`].
//!
//! A candidate is resolved against a throwaway cache directory, so resolving
//! one leaves the guest's own package cache and installed packages alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{GuestFailure, LegError, Result};
use crate::guest::index;
use crate::guest::{Candidate, GuestEnvironment, REPOSITORY_URLS};

/// The file apk records an installed package in.
const INSTALLED_FILE: &str = "/lib/apk/db/installed";

/// The prefix of the cache file apk keeps a repository index in.
const INDEX_PREFIX: &str = "APKINDEX.";

/// The suffix of the cache file apk keeps a repository index in.
const INDEX_SUFFIX: &str = ".tar.gz";

/// The guest operations the provisioning flow drives.
pub struct SystemGuest {
    /// The apk executable to run.
    apk: PathBuf,
}

impl SystemGuest {
    /// Creates a guest that drives the apk on the PATH.
    ///
    /// The guest is rejected when apk is missing or is not apk-tools, so a
    /// desktop installation fails with a remediation instead of touching the
    /// host's package manager.
    pub fn new() -> Result<Self> {
        let guest = SystemGuest {
            apk: PathBuf::from("apk"),
        };
        let output = guest
            .apk_output(&["--version"])
            .map_err(|error| match error {
                LegError::Guest(failure) if failure.reason == "apk_missing" => {
                    LegError::Guest(GuestFailure {
                        reason: "guest_unavailable",
                        message: "apk is not available in this environment".to_string(),
                        remediation: "run the guest command inside the pinned Alpine guest"
                            .to_string(),
                    })
                }
                other => other,
            })?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !version.starts_with("apk-tools") {
            return Err(LegError::Guest(GuestFailure {
                reason: "guest_incompatible",
                message: format!("the guest runs {version}, not apk-tools"),
                remediation: "run the guest command inside the pinned Alpine guest".to_string(),
            }));
        }
        Ok(guest)
    }

    /// Runs apk with fixed arguments and returns its output.
    fn apk_output(&self, args: &[&str]) -> Result<Output> {
        let output = Command::new(&self.apk)
            .args(args)
            .output()
            .map_err(|error| {
                LegError::Guest(GuestFailure {
                    reason: "apk_missing",
                    message: format!("apk could not be run: {error}"),
                    remediation: "run the guest command inside the pinned Alpine guest".to_string(),
                })
            })?;
        if !output.status.success() {
            return Err(apk_failure(args, &output));
        }
        Ok(output)
    }

    /// Writes a repositories file holding exactly the pinned repositories.
    fn write_repositories(directory: &Path) -> Result<PathBuf> {
        let path = directory.join("repositories");
        let text = REPOSITORY_URLS
            .iter()
            .map(|url| format!("{url}\n"))
            .collect::<String>();
        fs::write(&path, text).map_err(|error| {
            LegError::Io(format!(
                "could not write the pinned repositories to {}: {error}",
                path.display()
            ))
        })?;
        Ok(path)
    }
}

impl GuestEnvironment for SystemGuest {
    fn release(&mut self) -> Result<String> {
        let path = Path::new("/etc/alpine-release");
        let text = fs::read_to_string(path).map_err(|error| {
            LegError::Guest(GuestFailure {
                reason: "guest_unavailable",
                message: format!("could not read {}: {error}", path.display()),
                remediation: "run the guest command inside the pinned Alpine guest".to_string(),
            })
        })?;
        Ok(text.trim().to_string())
    }

    fn architecture(&mut self) -> Result<String> {
        let output = self.apk_output(&["--print-arch"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn installed_versions(&mut self, packages: &[&str]) -> Result<BTreeMap<String, String>> {
        let mut args = vec!["policy"];
        args.extend_from_slice(packages);
        let output = self.apk_output(&args)?;
        Ok(parse_policy(&String::from_utf8_lossy(&output.stdout)))
    }

    fn candidate(&mut self, package: &str) -> Result<Candidate> {
        let scratch = Scratch::create("candidate")?;
        let architecture = self.architecture()?;

        for url in REPOSITORY_URLS {
            let repository_dir = scratch.path().join(repository_slug(url));
            let cache_dir = repository_dir.join("cache");
            fs::create_dir_all(&cache_dir).map_err(|error| {
                LegError::Io(format!(
                    "could not create the scratch cache {}: {error}",
                    cache_dir.display()
                ))
            })?;
            let repositories = Self::write_repositories(&repository_dir)?;
            let (cache, repositories) = (
                cache_dir.to_string_lossy().into_owned(),
                repositories.to_string_lossy().into_owned(),
            );
            self.apk_output(&[
                "--cache-dir",
                &cache,
                "--repositories-file",
                &repositories,
                "--no-progress",
                "update",
            ])?;

            let index_path = find_index(&cache_dir).ok_or_else(|| {
                LegError::Guest(GuestFailure {
                    reason: "index_missing",
                    message: format!("apk cached no repository index for {url}"),
                    remediation: "check the guest's network and its /etc/apk/keys".to_string(),
                })
            })?;
            let bytes = fs::read(&index_path).map_err(|error| {
                LegError::Io(format!(
                    "could not read the repository index {}: {error}",
                    index_path.display()
                ))
            })?;
            let entries = index::parse_index_archive(&bytes).map_err(|error| {
                LegError::Guest(GuestFailure {
                    reason: "index_unreadable",
                    message: format!("the repository index for {url} could not be read: {error}"),
                    remediation: "check the guest's network and its /etc/apk/keys".to_string(),
                })
            })?;

            let entry = entries.iter().find(|entry| {
                entry.name == package
                    && (entry.architecture == architecture || entry.architecture == "noarch")
            });
            let Some(entry) = entry else {
                continue;
            };
            return Ok(Candidate {
                package: entry.name.clone(),
                version: entry.version.clone(),
                repository: url.to_string(),
                architecture: entry.architecture.clone(),
                download_bytes: entry.download_bytes,
                install_bytes: entry.install_bytes,
                checksum: entry.checksum.clone(),
            });
        }

        Err(LegError::Guest(GuestFailure {
            reason: "package_unavailable",
            message: format!("the pinned repositories offer no {package} for {architecture}"),
            remediation: "the pinned repositories do not carry this package".to_string(),
        }))
    }

    fn install(&mut self, package: &str, version: &str) -> Result<()> {
        let scratch = Scratch::create("install")?;
        let repositories = Self::write_repositories(scratch.path())?;
        let repositories = repositories.to_string_lossy().into_owned();
        let spec = format!("{package}={version}");
        self.apk_output(&[
            "--repositories-file",
            &repositories,
            "--no-progress",
            "add",
            &spec,
        ])?;
        Ok(())
    }

    fn run(&mut self, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program).args(args).output().map_err(|error| {
            LegError::Guest(GuestFailure {
                reason: "executable_missing",
                message: format!("{program} could not be run: {error}"),
                remediation: format!("check that {program} is on the guest's PATH"),
            })
        })?;
        if !output.status.success() {
            return Err(LegError::Guest(GuestFailure {
                reason: "verification_failed",
                message: format!(
                    "{program} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                remediation: format!("reinstall the package before using {program}"),
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn free_bytes(&mut self) -> Result<u64> {
        free_bytes()
    }
}

/// The bytes still available on the guest's storage.
///
/// The `statvfs` field widths follow the target's word size — a 32-bit guest
/// counts in 32-bit blocks — so the casts are needed on a 32-bit target and
/// redundant on a 64-bit one.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn free_bytes() -> Result<u64> {
    let path = std::ffi::CString::new("/")
        .map_err(|error| LegError::Io(format!("could not build the guest root path: {error}")))?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut stats) };
    if result != 0 {
        return Err(LegError::Io(
            "could not read the guest's free storage".to_string(),
        ));
    }
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

/// The bytes still available on the guest's storage.
#[cfg(not(unix))]
fn free_bytes() -> Result<u64> {
    Err(LegError::Guest(GuestFailure {
        reason: "guest_unavailable",
        message: "the guest's free storage is only available in an Alpine guest".to_string(),
        remediation: "run the guest command inside the pinned Alpine guest".to_string(),
    }))
}

/// Parses `apk policy` output into the installed package versions.
fn parse_policy(output: &str) -> BTreeMap<String, String> {
    let mut installed = BTreeMap::new();
    let mut package: Option<String> = None;
    let mut version: Option<String> = None;

    for line in output.lines() {
        if !line.starts_with(' ') {
            package = line.strip_suffix(" policy:").map(str::to_string);
            version = None;
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if indent == 2 && trimmed.ends_with(':') {
            version = Some(trimmed.trim_end_matches(':').to_string());
        } else if indent >= 4
            && trimmed == INSTALLED_FILE
            && let (Some(name), Some(value)) = (&package, &version)
        {
            installed
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
    }
    installed
}

/// The cache file apk keeps a repository index in.
fn find_index(cache_dir: &Path) -> Option<PathBuf> {
    let mut names: Vec<String> = fs::read_dir(cache_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(INDEX_PREFIX) && name.ends_with(INDEX_SUFFIX))
        .collect();
    names.sort();
    names.into_iter().next().map(|name| cache_dir.join(name))
}

/// The file name a repository URL is stored under.
fn repository_slug(url: &str) -> String {
    url.rsplit('/')
        .next()
        .unwrap_or("repository")
        .replace(['.', ':'], "-")
}

/// Maps an apk failure onto a structured guest failure.
///
/// The classification only shapes the remediation; every apk failure fails
/// closed whatever it says.
fn apk_failure(args: &[&str], output: &Output) -> LegError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let message = format!(
        "apk {} failed: {}",
        args.join(" "),
        if detail.is_empty() {
            output.status.to_string()
        } else {
            detail.to_string()
        }
    );

    let lowered = message.to_ascii_lowercase();
    let (reason, remediation) = if contains_any(
        &lowered,
        &[
            "untrusted signature",
            "failed to verify",
            "signature verification failed",
        ],
    ) {
        (
            "signature_invalid",
            "the guest's signing keys do not match the pinned repository; check /etc/apk/keys",
        )
    } else if contains_any(&lowered, &["checksum", "mismatch"]) {
        (
            "checksum_mismatch",
            "the fetched package did not match the repository checksum; retry, and report it if it persists",
        )
    } else if contains_any(
        &lowered,
        &[
            "can't connect",
            "connection refused",
            "network is unreachable",
            "temporary failure in name resolution",
            "failed to fetch",
            "could not resolve",
        ],
    ) {
        (
            "offline",
            "the guest could not reach the pinned repository; check the guest's network",
        )
    } else if contains_any(
        &lowered,
        &[
            "conflicts with",
            "unable to select package",
            "unsatisfiable",
            "conflict",
        ],
    ) {
        (
            "conflict",
            "the pinned repository cannot satisfy this package together with the guest's installed packages",
        )
    } else if contains_any(&lowered, &["no space left", "not enough space"]) {
        (
            "insufficient_space",
            "free space in the guest before provisioning",
        )
    } else if contains_any(
        &lowered,
        &[
            "permission denied",
            "operation not permitted",
            "must be root",
        ],
    ) {
        (
            "permission_denied",
            "run the guest command as root; installing packages needs it",
        )
    } else {
        (
            "apk_failed",
            "run apk in the guest to see the full diagnostic",
        )
    };

    LegError::Guest(GuestFailure {
        reason,
        message,
        remediation: remediation.to_string(),
    })
}

/// Whether the text holds any of the needles.
fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// A temporary directory that is removed when it goes out of scope.
struct Scratch {
    /// The directory itself.
    path: PathBuf,
}

impl Scratch {
    /// Creates a scratch directory named after the operation.
    fn create(label: &str) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "leg-guest-{label}-{}-{counter}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).map_err(|error| {
            LegError::Io(format!(
                "could not create the scratch directory {}: {error}",
                path.display()
            ))
        })?;
        Ok(Scratch { path })
    }

    /// The scratch directory.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
