//! Read-only discovery and cataloguing for Skills v1.
//!
//! Discovery reads only bounded YAML frontmatter. The selected package path
//! and its private file identity stay on the in-process entry for later
//! activation code; neither is serialized into the provider-facing catalog.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_yaml_ng::{Mapping, Value};

use crate::config::instruction_directories;

pub const SKILLS_CATALOG_SCHEMA: &str = "leg.skills.catalog/v1";

/// Maximum bytes read from one `SKILL.md` while discovering its metadata.
/// The closing delimiter must occur within this prefix; the body is not read.
pub const MAX_SKILL_FRONTMATTER_BYTES: usize = 16 * 1024;

static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Provider-facing Skills metadata plus private package bindings for the
/// current discovery snapshot.
#[derive(Clone, Serialize)]
pub struct SkillCatalog {
    pub schema: &'static str,
    pub snapshot_id: String,
    pub skills: Vec<SkillEntry>,
    pub warnings: Vec<SkillWarning>,
}

impl SkillCatalog {
    /// Serialize the complete catalog as deterministic JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Skills catalog fields always serialize to JSON")
    }

    pub(crate) fn package_binding(&self, name: &str) -> Option<(&Path, &PackageIdentity)> {
        self.skills
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| {
                (
                    entry.binding.skill_file_path.as_path(),
                    &entry.binding.identity,
                )
            })
    }
}

/// Public catalog metadata for one valid package.
#[derive(Clone, Serialize)]
pub struct SkillEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(skip)]
    binding: PackageBinding,
}

/// A non-fatal discovery diagnostic. Messages contain stable scope-relative
/// labels rather than absolute paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillWarning {
    pub code: String,
    pub message: String,
}

/// Discover the documented project and user roots and build one catalog.
///
/// `catalog_budget_bytes` caps the complete serialized JSON catalog. An entry
/// is kept only when adding it leaves the full catalog within that budget.
/// Diagnostics remain visible even if a very small budget cannot contain the
/// required catalog fields and warnings.
pub fn discover_skill_catalog(
    cwd: &Path,
    home: Option<&Path>,
    codex_home: Option<&Path>,
    catalog_budget_bytes: usize,
) -> SkillCatalog {
    let snapshot_id = next_snapshot_id();
    let roots = discovery_roots(cwd, home, codex_home);
    let mut warnings = Vec::new();
    let mut seen_roots = HashSet::new();
    let mut seen_packages = HashSet::new();
    let mut packages = Vec::new();

    for (priority, root) in roots.into_iter().enumerate() {
        packages.extend(scan_root(
            root,
            priority,
            &mut seen_roots,
            &mut seen_packages,
            &mut warnings,
        ));
    }

    packages.sort_by(|left, right| {
        left.root_priority
            .cmp(&right.root_priority)
            .then_with(|| {
                normalized_component(&left.relative_path)
                    .cmp(&normalized_component(&right.relative_path))
            })
            .then_with(|| {
                canonical_path_order(&left.canonical_package_path)
                    .cmp(&canonical_path_order(&right.canonical_package_path))
            })
    });

    let mut winners: HashMap<String, PackageCandidate> = HashMap::new();
    for candidate in packages {
        if let Some(winner) = winners.get(&candidate.metadata.name) {
            warnings.push(SkillWarning {
                code: "duplicate_skill".to_string(),
                message: format!(
                    "Duplicate skill '{}' skipped: '{}' wins over '{}'.",
                    candidate.metadata.name, winner.display_label, candidate.display_label
                ),
            });
        } else {
            winners.insert(candidate.metadata.name.clone(), candidate);
        }
    }

    let mut winners: Vec<_> = winners.into_values().collect();
    winners.sort_by(|left, right| {
        left.root_priority
            .cmp(&right.root_priority)
            .then_with(|| left.metadata.name.cmp(&right.metadata.name))
            .then_with(|| {
                canonical_path_order(&left.canonical_package_path)
                    .cmp(&canonical_path_order(&right.canonical_package_path))
            })
    });

    let mut catalog = SkillCatalog {
        schema: SKILLS_CATALOG_SCHEMA,
        snapshot_id,
        skills: Vec::new(),
        warnings,
    };
    let mut budget_warning_added = false;
    for candidate in winners {
        let entry = candidate.into_entry(catalog.skills.len(), &catalog.snapshot_id);
        catalog.skills.push(entry);
        if catalog.to_json().len() > catalog_budget_bytes {
            catalog.skills.pop();
            if !budget_warning_added {
                catalog.warnings.push(SkillWarning {
                    code: "catalog_budget_exceeded".to_string(),
                    message: "One or more complete skill entries were omitted because the catalog budget was exceeded."
                        .to_string(),
                });
                budget_warning_added = true;
            }
        }
    }

    catalog
}

/// Discover using the process working directory and standard home variables.
/// `$CODEX_HOME` is the only configurable compatibility root.
pub fn discover_skill_catalog_from_env(catalog_budget_bytes: usize) -> SkillCatalog {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = home_directory();
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    discover_skill_catalog(
        &cwd,
        home.as_deref(),
        codex_home.as_deref(),
        catalog_budget_bytes,
    )
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let preferred = "USERPROFILE";
    #[cfg(not(windows))]
    let preferred = "HOME";

    std::env::var_os(preferred)
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

#[derive(Clone, Debug)]
struct RootSpec {
    path: PathBuf,
    scope: &'static str,
    source: String,
}

fn discovery_roots(cwd: &Path, home: Option<&Path>, codex_home: Option<&Path>) -> Vec<RootSpec> {
    let directories = instruction_directories(cwd);
    let Some(project_root) = directories.first() else {
        return Vec::new();
    };
    let project_kinds = [
        (".agents/skills", [".agents", "skills"]),
        (".codex/skills", [".codex", "skills"]),
        (".claude/skills", [".claude", "skills"]),
        (".pi/skills", [".pi", "skills"]),
    ];
    let mut roots = Vec::new();

    // Nearest directory first; for each directory preserve the contract's
    // root-kind order. `instruction_directories` owns the shared git-root and
    // Windows path spelling behavior used by AGENTS.md discovery.
    for directory in directories.iter().rev() {
        let relative_directory = directory
            .strip_prefix(project_root)
            .ok()
            .map(normalized_relative_path)
            .filter(|relative| !relative.is_empty());
        for (suffix, components) in project_kinds {
            let mut path = directory.to_path_buf();
            for component in components {
                path.push(component);
            }
            let source = match &relative_directory {
                Some(relative) => format!("project:{relative}/{suffix}"),
                None => format!("project:{suffix}"),
            };
            roots.push(RootSpec {
                path,
                scope: "project",
                source,
            });
        }
    }

    if let Some(home) = home {
        roots.push(RootSpec {
            path: home.join(".agents").join("skills"),
            scope: "user",
            source: "user:~/.agents/skills".to_string(),
        });

        let codex_home = codex_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".codex"));
        roots.push(RootSpec {
            path: codex_home.join("skills"),
            scope: "user",
            source: if codex_home == home.join(".codex") {
                "user:$CODEX_HOME/skills (default ~/.codex/skills)".to_string()
            } else {
                "user:$CODEX_HOME/skills".to_string()
            },
        });
        roots.push(RootSpec {
            path: home.join(".claude").join("skills"),
            scope: "user",
            source: "user:~/.claude/skills".to_string(),
        });
        roots.push(RootSpec {
            path: home.join(".pi").join("agent").join("skills"),
            scope: "user",
            source: "user:~/.pi/agent/skills".to_string(),
        });
    } else if let Some(codex_home) = codex_home {
        roots.push(RootSpec {
            path: codex_home.join("skills"),
            scope: "user",
            source: "user:$CODEX_HOME/skills".to_string(),
        });
    }

    roots
}

#[derive(Clone, Debug)]
struct PackageCandidate {
    root_priority: usize,
    relative_path: String,
    canonical_package_path: PathBuf,
    display_label: String,
    source: String,
    scope: &'static str,
    metadata: SkillMetadata,
    binding: PackageBinding,
}

impl PackageCandidate {
    fn into_entry(self, index: usize, snapshot_id: &str) -> SkillEntry {
        SkillEntry {
            id: format!("entry-{snapshot_id}-{index:x}"),
            name: self.metadata.name,
            description: self.metadata.description,
            scope: self.scope.to_string(),
            source: self.source,
            license: self.metadata.license,
            compatibility: self.metadata.compatibility,
            metadata: self.metadata.metadata,
            binding: self.binding,
        }
    }
}

#[derive(Clone, Debug)]
struct SkillMetadata {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug)]
struct PackageBinding {
    skill_file_path: PathBuf,
    identity: PackageIdentity,
}

/// Private change detector retained for a future activation call. Discovery
/// combines the canonical path, frontmatter bytes, size, and modification
/// time without reading the Markdown body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PackageIdentity {
    canonical_package_path: PathBuf,
    canonical_skill_file_path: PathBuf,
    file_len: u64,
    modified_nanos: Option<u128>,
    frontmatter: Vec<u8>,
}

fn scan_root(
    root: RootSpec,
    root_priority: usize,
    seen_roots: &mut HashSet<String>,
    seen_packages: &mut HashSet<String>,
    warnings: &mut Vec<SkillWarning>,
) -> Vec<PackageCandidate> {
    let canonical_root = match fs::canonicalize(&root.path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Ok(path) => path,
        Err(error) => {
            warnings.push(warning(
                "unreadable_root",
                format!("Could not resolve {}: {}.", root.source, error.kind()),
            ));
            return Vec::new();
        }
    };
    let root_metadata = match fs::metadata(&canonical_root) {
        Ok(metadata) => metadata,
        Err(error) => {
            warnings.push(warning(
                "unreadable_root",
                format!("Could not inspect {}: {}.", root.source, error.kind()),
            ));
            return Vec::new();
        }
    };
    if !root_metadata.is_dir() {
        warnings.push(warning(
            "invalid_root",
            format!("{} is not a directory.", root.source),
        ));
        return Vec::new();
    }
    if !seen_roots.insert(canonical_sort_key(&canonical_root)) {
        return Vec::new();
    }

    let mut visited_directories = HashSet::new();
    let mut pending = vec![root.path.clone()];
    let mut packages = Vec::new();

    while let Some(directory) = pending.pop() {
        let canonical_directory = match fs::canonicalize(&directory) {
            Ok(path) => path,
            Err(error) => {
                warnings.push(warning(
                    "unreadable_path",
                    format!(
                        "Could not resolve '{}' under {}: {}.",
                        display_relative(&root.path, &directory),
                        root.source,
                        error.kind()
                    ),
                ));
                continue;
            }
        };
        if !is_within_root(&canonical_root, &canonical_directory) {
            warnings.push(warning(
                "path_escape",
                format!(
                    "Skipped '{}' because its canonical target leaves {}.",
                    display_relative(&root.path, &directory),
                    root.source
                ),
            ));
            continue;
        }

        let relative_directory = directory
            .strip_prefix(&root.path)
            .map(normalized_relative_path)
            .unwrap_or_default();
        let is_root = relative_directory.is_empty();
        if !is_root {
            let skill_file = directory.join("SKILL.md");
            match fs::symlink_metadata(&skill_file) {
                Ok(_) => {
                    if let Some(candidate) = scan_package(
                        &root,
                        root_priority,
                        &canonical_root,
                        &directory,
                        &canonical_directory,
                        &skill_file,
                        &relative_directory,
                        warnings,
                    ) {
                        let package_key = canonical_sort_key(&canonical_directory);
                        if seen_packages.insert(package_key) {
                            packages.push(candidate);
                        }
                    }
                    // A directory containing SKILL.md is a package boundary,
                    // even when that package is invalid.
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    warnings.push(warning(
                        "unreadable_skill",
                        format!(
                            "Could not inspect '{}/SKILL.md' under {}: {}.",
                            relative_directory,
                            root.source,
                            error.kind()
                        ),
                    ));
                    continue;
                }
            }
        }

        let directory_key = canonical_sort_key(&canonical_directory);
        if !visited_directories.insert(directory_key) {
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(warning(
                    "unreadable_root",
                    format!(
                        "Could not read '{}' under {}: {}.",
                        display_relative(&root.path, &directory),
                        root.source,
                        error.kind()
                    ),
                ));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => children.push((
                    entry.path(),
                    entry.file_name().to_string_lossy().into_owned(),
                )),
                Err(error) => warnings.push(warning(
                    "unreadable_root",
                    format!("Could not enumerate {}: {}.", root.source, error.kind()),
                )),
            }
        }
        children.sort_by(|left, right| {
            normalized_component(&left.1)
                .cmp(&normalized_component(&right.1))
                .then_with(|| left.1.cmp(&right.1))
        });

        for (child_path, _) in children.into_iter().rev() {
            let child_type = match fs::symlink_metadata(&child_path) {
                Ok(metadata) => metadata.file_type(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(warning(
                        "unreadable_path",
                        format!(
                            "Could not inspect '{}' under {}: {}.",
                            display_relative(&root.path, &child_path),
                            root.source,
                            error.kind()
                        ),
                    ));
                    continue;
                }
            };
            if child_type.is_symlink() {
                let canonical_target = match fs::canonicalize(&child_path) {
                    Ok(path) => path,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        warnings.push(warning(
                            "path_escape",
                            format!(
                                "Skipped unresolved link '{}' under {}.",
                                display_relative(&root.path, &child_path),
                                root.source
                            ),
                        ));
                        continue;
                    }
                    Err(error) => {
                        warnings.push(warning(
                            "unreadable_path",
                            format!(
                                "Could not resolve '{}' under {}: {}.",
                                display_relative(&root.path, &child_path),
                                root.source,
                                error.kind()
                            ),
                        ));
                        continue;
                    }
                };
                if !is_within_root(&canonical_root, &canonical_target) {
                    warnings.push(warning(
                        "path_escape",
                        format!(
                            "Skipped '{}' because its canonical target leaves {}.",
                            display_relative(&root.path, &child_path),
                            root.source
                        ),
                    ));
                    continue;
                }
                if fs::metadata(&child_path).is_ok_and(|metadata| metadata.is_dir()) {
                    pending.push(child_path);
                }
            } else if child_type.is_dir() {
                pending.push(child_path);
            }
        }
    }

    packages
}

#[allow(clippy::too_many_arguments)]
fn scan_package(
    root: &RootSpec,
    root_priority: usize,
    canonical_root: &Path,
    directory: &Path,
    canonical_directory: &Path,
    skill_file: &Path,
    relative_directory: &str,
    warnings: &mut Vec<SkillWarning>,
) -> Option<PackageCandidate> {
    let canonical_skill_file = match fs::canonicalize(skill_file) {
        Ok(path) => path,
        Err(error) => {
            warnings.push(warning(
                "invalid_package",
                format!(
                    "Could not resolve '{relative_directory}/SKILL.md' under {}: {}.",
                    root.source,
                    error.kind()
                ),
            ));
            return None;
        }
    };
    if !is_within_root(canonical_root, &canonical_skill_file) {
        warnings.push(warning(
            "path_escape",
            format!(
                "Skipped '{relative_directory}/SKILL.md' under {} because its canonical target leaves the permitted root.",
                root.source
            ),
        ));
        return None;
    }
    let file_metadata = match fs::metadata(&canonical_skill_file) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            warnings.push(warning(
                "invalid_package",
                format!(
                    "'{relative_directory}/SKILL.md' under {} is not a regular file.",
                    root.source
                ),
            ));
            return None;
        }
        Err(error) => {
            warnings.push(warning(
                "unreadable_skill",
                format!(
                    "Could not inspect '{relative_directory}/SKILL.md' under {}: {}.",
                    root.source,
                    error.kind()
                ),
            ));
            return None;
        }
    };

    let mut file = match File::open(&canonical_skill_file) {
        Ok(file) => file,
        Err(error) => {
            warnings.push(warning(
                "unreadable_skill",
                format!(
                    "Could not read '{relative_directory}/SKILL.md' under {}: {}.",
                    root.source,
                    error.kind()
                ),
            ));
            return None;
        }
    };
    let (metadata, frontmatter) = match read_skill_frontmatter(&mut file) {
        Ok(parsed) => parsed,
        Err(error) => {
            warnings.push(warning(
                error.warning_code(),
                format!(
                    "'{relative_directory}/SKILL.md' under {}: {}.",
                    root.source,
                    error.message()
                ),
            ));
            return None;
        }
    };

    let package_name = directory.file_name().unwrap_or_default().to_string_lossy();
    if metadata.name != package_name {
        warnings.push(warning(
            "invalid_package",
            format!(
                "'{relative_directory}/SKILL.md' under {} names '{}' but its package directory is '{}'.",
                root.source, metadata.name, package_name
            ),
        ));
        return None;
    }

    let after_read = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            warnings.push(warning(
                "unreadable_skill",
                format!(
                    "Could not inspect '{relative_directory}/SKILL.md' under {}: {}.",
                    root.source,
                    error.kind()
                ),
            ));
            return None;
        }
    };
    if !same_file_version(&file_metadata, &after_read) {
        warnings.push(warning(
            "package_changed_during_discovery",
            format!(
                "'{relative_directory}/SKILL.md' under {} changed while it was being discovered.",
                root.source
            ),
        ));
        return None;
    }

    let relative_path = relative_directory.to_string();
    let display_label = format!("{}/{}", root.source, relative_path);
    let identity = PackageIdentity {
        canonical_package_path: canonical_directory.to_path_buf(),
        canonical_skill_file_path: canonical_skill_file,
        file_len: after_read.len(),
        modified_nanos: modified_nanos(&after_read),
        frontmatter,
    };
    Some(PackageCandidate {
        root_priority,
        relative_path,
        canonical_package_path: canonical_directory.to_path_buf(),
        display_label,
        source: root.source.clone(),
        scope: root.scope,
        metadata,
        binding: PackageBinding {
            skill_file_path: identity.canonical_skill_file_path.clone(),
            identity,
        },
    })
}

fn same_file_version(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len() && modified_nanos(before) == modified_nanos(after)
}

fn modified_nanos(metadata: &Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
}

#[derive(Debug)]
enum FrontmatterError {
    Io(io::Error),
    TooLarge,
    Invalid(String),
}

impl FrontmatterError {
    fn warning_code(&self) -> &'static str {
        match self {
            Self::Io(_) => "unreadable_skill",
            Self::TooLarge | Self::Invalid(_) => "invalid_package",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Io(error) => format!("read failed: {}", error.kind()),
            Self::TooLarge => format!(
                "frontmatter did not close within the {} byte limit",
                MAX_SKILL_FRONTMATTER_BYTES
            ),
            Self::Invalid(message) => message.clone(),
        }
    }
}

fn read_skill_frontmatter<R: Read>(
    reader: &mut R,
) -> Result<(SkillMetadata, Vec<u8>), FrontmatterError> {
    let mut consumed = 0usize;
    let mut line = Vec::new();
    let mut yaml = Vec::new();
    let mut saw_opening = false;
    let mut byte = [0u8; 1];

    loop {
        if consumed == MAX_SKILL_FRONTMATTER_BYTES {
            return Err(FrontmatterError::TooLarge);
        }
        let read = reader.read(&mut byte).map_err(FrontmatterError::Io)?;
        if read == 0 {
            let trimmed = line.strip_suffix(b"\r").unwrap_or(&line);
            if saw_opening && trimmed == b"---" {
                return parse_skill_metadata(&yaml).map(|metadata| (metadata, yaml));
            }
            return Err(FrontmatterError::Invalid(if saw_opening {
                "missing closing --- delimiter".to_string()
            } else {
                "missing opening --- delimiter".to_string()
            }));
        }
        consumed += 1;
        if byte[0] != b'\n' {
            line.push(byte[0]);
            continue;
        }

        let current_line = line.strip_suffix(b"\r").unwrap_or(&line);
        if !saw_opening {
            if current_line != b"---" {
                return Err(FrontmatterError::Invalid(
                    "missing opening --- delimiter".to_string(),
                ));
            }
            saw_opening = true;
        } else if current_line == b"---" {
            return parse_skill_metadata(&yaml).map(|metadata| (metadata, yaml));
        } else {
            yaml.extend_from_slice(current_line);
            yaml.push(b'\n');
        }
        line.clear();
    }
}

fn parse_skill_metadata(yaml: &[u8]) -> Result<SkillMetadata, FrontmatterError> {
    let yaml = std::str::from_utf8(yaml)
        .map_err(|_| FrontmatterError::Invalid("frontmatter is not UTF-8".to_string()))?;
    let document: Value = serde_yaml_ng::from_str(yaml)
        .map_err(|_| FrontmatterError::Invalid("frontmatter is not valid YAML".to_string()))?;
    let Value::Mapping(mapping) = document else {
        return Err(FrontmatterError::Invalid(
            "frontmatter must be a YAML mapping".to_string(),
        ));
    };

    let name = required_string(&mapping, "name")?;
    if !valid_skill_name(&name) {
        return Err(FrontmatterError::Invalid(
            "name must be 1–64 lowercase letters, digits, and single hyphens".to_string(),
        ));
    }
    let description = required_string(&mapping, "description")?;
    if description.trim().is_empty() || description.chars().count() > 1024 {
        return Err(FrontmatterError::Invalid(
            "description must be non-empty and at most 1024 characters".to_string(),
        ));
    }
    let license = optional_string(&mapping, "license")?;
    if license
        .as_deref()
        .is_some_and(|license| !valid_license(license))
    {
        return Err(FrontmatterError::Invalid(
            "license must be a short name or a relative bundled file path".to_string(),
        ));
    }
    let compatibility = optional_string(&mapping, "compatibility")?;
    if compatibility
        .as_deref()
        .is_some_and(|value| value.trim().is_empty() || value.chars().count() > 500)
    {
        return Err(FrontmatterError::Invalid(
            "compatibility must be non-empty and at most 500 characters".to_string(),
        ));
    }
    let metadata = optional_string_mapping(&mapping, "metadata")?;

    Ok(SkillMetadata {
        name,
        description,
        license,
        compatibility,
        metadata,
    })
}

fn required_string(mapping: &Mapping, field: &str) -> Result<String, FrontmatterError> {
    optional_string(mapping, field)?
        .ok_or_else(|| FrontmatterError::Invalid(format!("required field '{field}' is missing")))
}

fn optional_string(mapping: &Mapping, field: &str) -> Result<Option<String>, FrontmatterError> {
    let key = Value::String(field.to_string());
    match mapping.get(&key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(FrontmatterError::Invalid(format!(
            "field '{field}' must be a string"
        ))),
    }
}

fn optional_string_mapping(
    mapping: &Mapping,
    field: &str,
) -> Result<Option<BTreeMap<String, String>>, FrontmatterError> {
    let key = Value::String(field.to_string());
    let Some(value) = mapping.get(&key) else {
        return Ok(None);
    };
    let Value::Mapping(values) = value else {
        return Err(FrontmatterError::Invalid(format!(
            "field '{field}' must be a mapping of strings"
        )));
    };
    let mut result = BTreeMap::new();
    for (key, value) in values {
        let (Value::String(key), Value::String(value)) = (key, value) else {
            return Err(FrontmatterError::Invalid(format!(
                "field '{field}' must contain only string keys and values"
            )));
        };
        result.insert(key.clone(), value.clone());
    }
    Ok(Some(result))
}

fn valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || !name.is_ascii() {
        return false;
    }
    let mut previous_hyphen = true;
    for byte in name.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_hyphen = false,
            b'-' if !previous_hyphen => previous_hyphen = true,
            _ => return false,
        }
    }
    !previous_hyphen
}

fn valid_license(license: &str) -> bool {
    if license.trim().is_empty() || license.chars().count() > 512 {
        return false;
    }
    if license.starts_with('/') || license.starts_with('\\') {
        return false;
    }
    let bytes = license.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    let components = license.split(['/', '\\']).collect::<Vec<_>>();
    !components.iter().any(|component| *component == "..")
        && components
            .last()
            .is_some_and(|component| !component.is_empty() && *component != ".")
}

fn warning(code: &str, message: String) -> SkillWarning {
    SkillWarning {
        code: code.to_string(),
        message,
    }
}

fn next_snapshot_id() -> String {
    use std::hash::{Hash, Hasher};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    now.hash(&mut hasher);
    sequence.hash(&mut hasher);
    format!("snapshot-{:016x}", hasher.finish())
}

fn normalized_component(component: &str) -> String {
    #[cfg(windows)]
    {
        component.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        component.to_string()
    }
}

fn normalized_relative_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_string()
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(normalized_relative_path)
        .unwrap_or_else(|_| normalized_relative_path(path))
}

fn canonical_sort_key(path: &Path) -> String {
    let normalized = normalized_relative_path(path);
    #[cfg(windows)]
    {
        normalized.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn canonical_path_order(path: &Path) -> String {
    normalized_relative_path(path)
}

fn is_within_root(root: &Path, path: &Path) -> bool {
    let root = root
        .components()
        .map(|component| normalized_component(&component.as_os_str().to_string_lossy()))
        .collect::<Vec<_>>();
    let path = path
        .components()
        .map(|component| normalized_component(&component.as_os_str().to_string_lossy()))
        .collect::<Vec<_>>();
    path.len() >= root.len() && path[..root.len()] == root
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(label: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "leg-skills-{label}-{}-{sequence}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock after epoch")
                    .as_nanos()
            ));
            fs::create_dir_all(&path).expect("create temp tree");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn skill(root: &Path, name: &str, body: &str) -> PathBuf {
        skill_with_frontmatter(
            root,
            name,
            &format!("---\nname: {name}\ndescription: A useful {name} skill.\n---\n{body}"),
        )
    }

    fn skill_with_frontmatter(root: &Path, name: &str, content: &str) -> PathBuf {
        let package = root.join(name);
        fs::create_dir_all(&package).expect("create package");
        fs::write(package.join("SKILL.md"), content).expect("write SKILL.md");
        package
    }

    fn discover(cwd: &Path, home: Option<&Path>, codex: Option<&Path>) -> SkillCatalog {
        discover_skill_catalog(cwd, home, codex, usize::MAX)
    }

    fn names(catalog: &SkillCatalog) -> Vec<&str> {
        catalog
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }

    fn warning_codes(catalog: &SkillCatalog) -> Vec<&str> {
        catalog
            .warnings
            .iter()
            .map(|warning| warning.code.as_str())
            .collect()
    }

    #[test]
    fn reads_only_frontmatter_and_stops_before_body_bytes() {
        let source = b"---\nname: parser\ndescription: Parser skill.\n---\nBODY_MARKER_SECRET";
        let mut reader = Cursor::new(source);
        let (metadata, yaml) = read_skill_frontmatter(&mut reader).expect("valid metadata");
        let closing = source
            .windows(5)
            .position(|window| window == b"---\nB")
            .expect("closing delimiter");

        assert_eq!(metadata.name, "parser");
        assert!(
            !String::from_utf8(yaml)
                .unwrap()
                .contains("BODY_MARKER_SECRET")
        );
        assert_eq!(reader.position() as usize, closing + 4);
    }

    #[test]
    fn unterminated_frontmatter_fails_at_the_byte_cap() {
        struct CountingReader {
            inner: Cursor<Vec<u8>>,
            read: usize,
        }
        impl Read for CountingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.inner.read(buffer)?;
                self.read += count;
                Ok(count)
            }
        }

        let mut content = b"---\nname: oversized\ndescription: Too long.\n".to_vec();
        content.resize(MAX_SKILL_FRONTMATTER_BYTES * 4, b'x');
        let mut reader = CountingReader {
            inner: Cursor::new(content),
            read: 0,
        };

        assert!(matches!(
            read_skill_frontmatter(&mut reader),
            Err(FrontmatterError::TooLarge)
        ));
        assert_eq!(reader.read, MAX_SKILL_FRONTMATTER_BYTES);
    }

    #[test]
    fn discovers_all_roots_with_nearest_directory_and_root_kind_precedence() {
        let tree = TempTree::new("precedence");
        let repo = tree.path().join("repo");
        let app = repo.join("app");
        let cwd = app.join("nested");
        fs::create_dir_all(&cwd).expect("create project directories");
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .expect("run git init");
        assert!(git.success(), "git init failed");

        let project_roots = [".agents", ".codex", ".claude", ".pi"];
        for (index, root_kind) in project_roots.iter().enumerate() {
            let root = cwd.join(root_kind).join("skills");
            skill(&root, &format!("current-{index}"), "current");
            let app_root = app.join(root_kind).join("skills");
            skill(&app_root, &format!("app-{index}"), "app");
            let repo_root = repo.join(root_kind).join("skills");
            skill(&repo_root, &format!("repo-{index}"), "repo");
        }
        skill(&cwd.join(".agents/skills"), "same-kind", "nearest .agents");
        skill(&cwd.join(".codex/skills"), "same-kind", "later .codex");
        skill(&cwd.join(".pi/skills"), "near-directory", "near .pi");
        skill(
            &repo.join(".agents/skills"),
            "near-directory",
            "far .agents",
        );

        let home = tree.path().join("home");
        let codex_home = tree.path().join("codex-home");
        skill(&home.join(".agents/skills"), "user-agents", "user");
        skill(&codex_home.join("skills"), "user-codex", "configured");
        skill(&home.join(".claude/skills"), "user-claude", "user");
        skill(&home.join(".pi/agent/skills"), "user-pi", "user");
        skill(&home.join(".agents/skills"), "user-wins", "home agents");
        skill(&codex_home.join("skills"), "user-wins", "codex");
        skill(&home.join(".claude/skills"), "user-wins", "claude");

        let catalog = discover(&cwd, Some(&home), Some(&codex_home));
        assert_eq!(catalog.schema, SKILLS_CATALOG_SCHEMA);
        assert_eq!(
            names(&catalog),
            vec![
                "current-0",
                "same-kind",
                "current-1",
                "current-2",
                "current-3",
                "near-directory",
                "app-0",
                "app-1",
                "app-2",
                "app-3",
                "repo-0",
                "repo-1",
                "repo-2",
                "repo-3",
                "user-agents",
                "user-wins",
                "user-codex",
                "user-claude",
                "user-pi",
            ]
        );
        let near = catalog
            .skills
            .iter()
            .find(|entry| entry.name == "near-directory")
            .unwrap();
        assert_eq!(near.source, "project:app/nested/.pi/skills");
        let same_kind = catalog
            .skills
            .iter()
            .find(|entry| entry.name == "same-kind")
            .unwrap();
        assert_eq!(same_kind.source, "project:app/nested/.agents/skills");
        assert!(catalog.warnings.iter().any(|warning| {
            warning.code == "duplicate_skill"
                && warning.message.contains(
                    "user:~/.agents/skills/user-wins' wins over 'user:$CODEX_HOME/skills/user-wins",
                )
        }));
    }

    #[test]
    fn uses_default_codex_home_and_only_current_directory_without_a_repository() {
        let tree = TempTree::new("nonrepo");
        let parent = tree.path().join("parent");
        let cwd = parent.join("workspace");
        let home = tree.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        skill(&parent.join(".agents/skills"), "parent-only", "parent");
        skill(&cwd.join(".agents/skills"), "local", "current");
        skill(&home.join(".codex/skills"), "default-codex", "fallback");

        let catalog = discover(&cwd, Some(&home), None);
        assert_eq!(names(&catalog), vec!["local", "default-codex"]);
        assert_eq!(
            catalog.skills[1].source,
            "user:$CODEX_HOME/skills (default ~/.codex/skills)"
        );
    }

    #[test]
    fn duplicate_names_in_one_root_use_normalized_relative_path_order() {
        let tree = TempTree::new("same-root-duplicates");
        let cwd = tree.path().join("workspace");
        let root = cwd.join(".agents/skills");
        skill_with_frontmatter(
            &root.join("a"),
            "shared",
            "---\nname: shared\ndescription: First by relative path.\nmetadata:\n  winner: a\n---\n",
        );
        skill_with_frontmatter(
            &root.join("z"),
            "shared",
            "---\nname: shared\ndescription: Later by relative path.\nmetadata:\n  winner: z\n---\n",
        );

        let catalog = discover(&cwd, None, None);
        assert_eq!(names(&catalog), vec!["shared"]);
        assert_eq!(catalog.skills[0].metadata.as_ref().unwrap()["winner"], "a");
        assert!(catalog.warnings.iter().any(|warning| {
            warning.code == "duplicate_skill"
                && warning.message.contains(
                    "project:.agents/skills/a/shared' wins over 'project:.agents/skills/z/shared",
                )
        }));
    }

    #[test]
    fn missing_roots_are_skipped_and_root_level_skill_md_is_not_a_package() {
        let tree = TempTree::new("missing-roots");
        let cwd = tree.path().join("workspace");
        let root = cwd.join(".agents/skills");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\nname: root\ndescription: Root.\n---\n",
        )
        .unwrap();

        let catalog = discover(&cwd, None, None);
        assert!(catalog.skills.is_empty());
        assert!(catalog.warnings.is_empty());
    }

    #[test]
    fn rejects_invalid_yaml_metadata_layout_and_directory_names() {
        let tree = TempTree::new("invalid");
        let cwd = tree.path().join("workspace");
        let root = cwd.join(".agents/skills");
        let cases = [
            ("missing-name", "---\ndescription: Missing name.\n---\n".to_string()),
            ("wrong-name-type", "---\nname: 12\ndescription: Bad type.\n---\n".to_string()),
            ("wrong-description-type", "---\nname: wrong-description-type\ndescription: [bad]\n---\n".to_string()),
            ("invalid-name", "---\nname: Bad_Name\ndescription: Bad name.\n---\n".to_string()),
            ("bad-hyphens", "---\nname: one--two\ndescription: Bad hyphens.\n---\n".to_string()),
            ("long-name", format!("---\nname: {}\ndescription: Long name.\n---\n", "a".repeat(65))),
            ("long-description", format!("---\nname: long-description\ndescription: {}\n---\n", "x".repeat(1025))),
            ("bad-license", "---\nname: bad-license\ndescription: Bad license.\nlicense: ../LICENSE\n---\n".to_string()),
            ("bad-license-type", "---\nname: bad-license-type\ndescription: Bad license type.\nlicense: 7\n---\n".to_string()),
            ("bad-compatibility", "---\nname: bad-compatibility\ndescription: Bad compatibility.\ncompatibility: null\n---\n".to_string()),
            ("long-compatibility", format!("---\nname: long-compatibility\ndescription: Long compatibility.\ncompatibility: {}\n---\n", "x".repeat(501))),
            ("bad-metadata-type", "---\nname: bad-metadata-type\ndescription: Bad metadata.\nmetadata: [not, a, map]\n---\n".to_string()),
            ("bad-metadata-value", "---\nname: bad-metadata-value\ndescription: Bad metadata.\nmetadata:\n  owner: 7\n---\n".to_string()),
            ("bad-metadata-key", "---\nname: bad-metadata-key\ndescription: Bad metadata key.\nmetadata:\n  7: owner\n---\n".to_string()),
            ("mismatch-directory", "---\nname: different-name\ndescription: Mismatch.\n---\n".to_string()),
            ("invalid-yaml", "---\nname: invalid-yaml\ndescription: [unterminated\n---\n".to_string()),
            ("duplicate-key", "---\nname: duplicate-key\nname: duplicate-key\ndescription: Duplicate key.\n---\n".to_string()),
            ("missing-close", "---\nname: missing-close\ndescription: No close.\n".to_string()),
        ];
        let case_count = cases.len();
        for (directory, content) in cases {
            skill_with_frontmatter(&root, directory, &content);
        }
        // The invalid high-priority package must not reserve the name.
        skill_with_frontmatter(
            &cwd.join(".codex/skills"),
            "recoverable",
            "---\nname: recoverable\ndescription: Valid lower priority.\n---\n",
        );
        skill_with_frontmatter(
            &root,
            "recoverable",
            "---\nname: recoverable\ndescription: Missing close.\n",
        );

        let catalog = discover(&cwd, None, None);
        assert_eq!(names(&catalog), vec!["recoverable"]);
        assert_eq!(catalog.skills[0].source, "project:.codex/skills");
        assert!(warning_codes(&catalog).contains(&"invalid_package"));
        assert!(warning_codes(&catalog).contains(&"invalid_package"));
        assert!(catalog.warnings.len() >= case_count);
    }

    #[test]
    fn rejects_frontmatter_larger_than_the_explicit_cap() {
        let tree = TempTree::new("frontmatter-cap");
        let cwd = tree.path().join("workspace");
        let package = cwd.join(".agents/skills/oversized");
        fs::create_dir_all(&package).unwrap();
        let mut content = b"---\nname: oversized\ndescription: Too large.\n".to_vec();
        content.resize(MAX_SKILL_FRONTMATTER_BYTES * 2, b'x');
        fs::write(package.join("SKILL.md"), content).unwrap();

        let catalog = discover(&cwd, None, None);
        assert!(catalog.skills.is_empty());
        assert!(warning_codes(&catalog).contains(&"invalid_package"));
        assert!(catalog.warnings[0].message.contains("byte limit"));
    }

    #[test]
    fn accepts_metadata_at_contract_length_boundaries() {
        let name = "a".repeat(64);
        let description = "d".repeat(1024);
        let compatibility = "c".repeat(500);
        let content = format!(
            "---\nname: {name}\ndescription: {description}\ncompatibility: {compatibility}\nlicense: ./LICENSE.txt\n---\n"
        );
        let (metadata, _) =
            read_skill_frontmatter(&mut Cursor::new(content)).expect("boundary values are valid");

        assert_eq!(metadata.name.chars().count(), 64);
        assert_eq!(metadata.description.chars().count(), 1024);
        assert_eq!(metadata.compatibility.unwrap().chars().count(), 500);
        assert_eq!(metadata.license.as_deref(), Some("./LICENSE.txt"));
    }

    #[test]
    fn reports_non_regular_skill_file_and_stops_nested_package_discovery() {
        let tree = TempTree::new("package-boundary");
        let cwd = tree.path().join("workspace");
        let root = cwd.join(".agents/skills");
        let parent = root.join("outer");
        fs::create_dir_all(parent.join("SKILL.md")).unwrap();
        skill(&parent.join("nested"), "inner", "nested");

        let catalog = discover(&cwd, None, None);
        assert!(catalog.skills.is_empty());
        assert!(warning_codes(&catalog).contains(&"invalid_package"));
    }

    #[test]
    fn budget_keeps_a_complete_entry_and_omits_the_next_one_whole() {
        let tree = TempTree::new("budget");
        let cwd = tree.path().join("workspace");
        let one_root = cwd.join("one/.agents/skills");
        skill(&one_root, "skill-a", "same body");
        let one_catalog = discover_skill_catalog(
            &one_root.parent().unwrap().parent().unwrap(),
            None,
            None,
            usize::MAX,
        );
        let one_entry_bytes = one_catalog.to_json().len();

        let root = cwd.join(".agents/skills");
        skill(&root, "skill-a", "same body");
        skill(&root, "skill-b", "same body");
        let catalog = discover_skill_catalog(&cwd, None, None, one_entry_bytes);

        assert_eq!(names(&catalog), vec!["skill-a"]);
        assert!(warning_codes(&catalog).contains(&"catalog_budget_exceeded"));
    }

    #[test]
    fn refreshes_added_removed_and_edited_packages_with_new_private_identity() {
        let tree = TempTree::new("refresh");
        let cwd = tree.path().join("workspace");
        let root = cwd.join(".agents/skills");
        let first_package = skill(&root, "alpha", "before");
        let first = discover(&cwd, None, None);
        let first_identity = first.package_binding("alpha").unwrap().1.clone();

        skill(&root, "beta", "added");
        fs::write(
            first_package.join("SKILL.md"),
            "---\nname: alpha\ndescription: A useful alpha skill.\n---\nchange",
        )
        .unwrap();
        let second = discover(&cwd, None, None);
        assert_ne!(first.snapshot_id, second.snapshot_id);
        assert_ne!(first.skills[0].id, second.skills[0].id);
        assert_eq!(names(&second), vec!["alpha", "beta"]);
        assert_ne!(
            first_identity,
            second.package_binding("alpha").unwrap().1.clone()
        );

        fs::remove_dir_all(root.join("beta")).unwrap();
        let third = discover(&cwd, None, None);
        assert_ne!(second.snapshot_id, third.snapshot_id);
        assert_eq!(names(&third), vec!["alpha"]);
    }

    #[test]
    fn canonical_roots_deduplicate_and_out_of_root_package_links_are_rejected() {
        let tree = TempTree::new("symlinks");
        let cwd = tree.path().join("workspace");
        let project_root = cwd.join(".agents/skills");
        skill(&project_root, "inside", "valid");
        let internal_package = skill(&project_root.join("actual"), "same-package", "internal");
        let internal_alias = project_root.join("alias/same-package");
        fs::create_dir_all(internal_alias.parent().unwrap()).unwrap();
        symlink_dir(&internal_package, &internal_alias);
        let outside = tree.path().join("outside");
        skill(&outside, "outside-skill", "must not be read");
        let file_escape = project_root.join("file-escape");
        fs::create_dir_all(&file_escape).unwrap();
        symlink_file(
            &outside.join("outside-skill/SKILL.md"),
            &file_escape.join("SKILL.md"),
        );

        let home = tree.path().join("home");
        fs::create_dir_all(home.join(".agents")).unwrap();
        symlink_dir(&project_root, &home.join(".agents/skills"));
        symlink_dir(&outside.join("outside-skill"), &project_root.join("escape"));

        let catalog = discover(&cwd, Some(&home), None);
        assert_eq!(names(&catalog), vec!["inside", "same-package"]);
        assert!(warning_codes(&catalog).contains(&"path_escape"));
        assert!(
            !catalog
                .warnings
                .iter()
                .any(|warning| warning.code == "duplicate_skill")
        );
    }

    #[test]
    fn discovery_never_executes_package_resources() {
        let tree = TempTree::new("no-execution");
        let cwd = tree.path().join("workspace");
        let package = skill(&cwd.join(".agents/skills"), "scripted", "description only");
        let scripts = package.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        let marker = scripts.join("script-ran.marker");
        #[cfg(unix)]
        let script = "#!/bin/sh\nprintf ran > \"$(dirname \"$0\")/script-ran.marker\"\n";
        #[cfg(windows)]
        let script = "@echo off\necho ran > \"%~dp0script-ran.marker\"\n";
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script_path = scripts.join("write-marker.sh");
            fs::write(&script_path, script).unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(script_path, permissions).unwrap();
        }
        #[cfg(windows)]
        {
            fs::write(scripts.join("write-marker.cmd"), script).unwrap();
            fs::write(
                scripts.join("write-marker.sh"),
                "#!/bin/sh\nprintf ran > \"$(dirname \"$0\")/script-ran.marker\"\n",
            )
            .unwrap();
        }

        let catalog = discover(&cwd, None, None);
        assert_eq!(names(&catalog), vec!["scripted"]);
        assert!(!marker.exists());
    }

    #[test]
    fn serializes_only_catalog_metadata_and_never_absolute_package_paths() {
        let tree = TempTree::new("serialization");
        let cwd = tree.path().join("workspace");
        let package = skill_with_frontmatter(
            &cwd.join(".agents/skills"),
            "portable",
            "---\nname: portable\ndescription: Portable metadata.\nlicense: Apache-2.0\ncompatibility: Works here\nmetadata:\n  owner: tools\nunknown: ignored\n---\nPRIVATE_BODY_MARKER",
        );

        let catalog = discover(&cwd, None, None);
        let json = catalog.to_json();
        assert!(json.contains("leg.skills.catalog/v1"));
        assert!(json.contains("Apache-2.0"));
        assert!(json.contains("\"owner\":\"tools\""));
        assert!(!json.contains("PRIVATE_BODY_MARKER"));
        assert!(!json.contains(&package.to_string_lossy().to_string()));
        assert!(!json.contains("scripts"));
    }

    #[test]
    fn discovers_the_frozen_fixture_corpus_outcomes() {
        let tree = TempTree::new("fixture-corpus");
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills-v1");
        let cwd = tree.path().join("workspace");
        let project_skills = cwd.join(".agents/skills");
        let home = tree.path().join("home");
        let user_skills = home.join(".agents/skills");

        copy_directory(
            &fixture_root.join("valid/fixture-valid"),
            &project_skills.join("fixture-valid"),
        );
        copy_directory(
            &fixture_root.join("malformed/fixture-malformed"),
            &project_skills.join("fixture-malformed"),
        );
        copy_directory(
            &fixture_root.join("duplicate/project/.agents/skills/fixture-duplicate"),
            &project_skills.join("fixture-duplicate"),
        );
        copy_directory(
            &fixture_root.join("duplicate/user/.agents/skills/fixture-duplicate"),
            &user_skills.join("fixture-duplicate"),
        );

        let catalog = discover(&cwd, Some(&home), None);
        assert_eq!(names(&catalog), vec!["fixture-duplicate", "fixture-valid"]);
        let duplicate = catalog
            .skills
            .iter()
            .find(|entry| entry.name == "fixture-duplicate")
            .unwrap();
        assert_eq!(duplicate.scope, "project");
        assert_eq!(
            duplicate.metadata.as_ref().unwrap().get("winner").unwrap(),
            "project"
        );
        assert!(catalog.warnings.iter().any(|warning| {
            warning.code == "duplicate_skill"
                && warning.message.contains("fixture-duplicate")
                && warning.message.contains("project:.agents/skills/fixture-duplicate' wins over 'user:~/.agents/skills/fixture-duplicate")
        }));
        assert!(warning_codes(&catalog).contains(&"invalid_package"));
        assert!(!catalog.to_json().contains("DUPLICATE_USER_LOSER"));
        assert!(
            !catalog
                .to_json()
                .contains("This body is loaded only after explicit activation")
        );
    }

    #[test]
    fn maps_read_errors_to_non_fatal_unreadable_diagnostics() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture"))
            }
        }

        let error = read_skill_frontmatter(&mut FailingReader).unwrap_err();
        assert_eq!(error.warning_code(), "unreadable_skill");
        assert!(error.message().starts_with("read failed:"));
    }

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("create directory symlink");
    }

    #[cfg(windows)]
    fn symlink_dir(target: &Path, link: &Path) {
        std::os::windows::fs::symlink_dir(target, link).expect("create directory symlink");
    }

    #[cfg(unix)]
    fn symlink_file(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("create file symlink");
    }

    #[cfg(windows)]
    fn symlink_file(target: &Path, link: &Path) {
        std::os::windows::fs::symlink_file(target, link).expect("create file symlink");
    }

    fn copy_directory(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).expect("create copied fixture directory");
        for entry in fs::read_dir(source).expect("read fixture directory") {
            let entry = entry.expect("read fixture entry");
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_directory(&source_path, &destination_path);
            } else {
                fs::copy(source_path, destination_path).expect("copy fixture file");
            }
        }
    }
}
