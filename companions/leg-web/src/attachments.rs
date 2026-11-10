//! Authenticated Web attachment staging.
//!
//! Browser requests carry only the opaque `AttachmentRef` returned by this
//! store. The native resolver remains the authority that expands a ref into
//! provider content; this module validates the untrusted upload/ref boundary
//! and owns the short-lived staged bytes.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use leg_ui_client::{AttachmentRef, ContentBlock, ImageSource};
use sha2::{Digest, Sha256};

pub const MAX_BASE64_IMAGE_BYTES: usize = 10_000_000;
pub const MAX_IMAGE_REQUEST_BYTES: usize = 32_000_000;
pub const MAX_RAW_IMAGE_BYTES: usize = MAX_BASE64_IMAGE_BYTES / 4 * 3;
/// Leaves bounded room for multipart headers while keeping the upload route
/// stricter than the JSON submit route's 64 KiB limit.
pub const MAX_UPLOAD_BODY_BYTES: usize = MAX_RAW_IMAGE_BYTES + 64 * 1024;
pub const ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const ATTACHMENTS_DIR: &str = "attachments";
const SUPPORTED_MEDIA_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

#[derive(Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentError {
    Invalid,
    Missing,
    Expired,
    TooLarge,
    MimeMismatch,
    DigestMismatch,
    Io,
}

impl fmt::Display for AttachmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Invalid => "attachment is invalid",
            Self::Missing => "attachment is missing",
            Self::Expired => "attachment has expired",
            Self::TooLarge => "attachment exceeds the fixed size limit",
            Self::MimeMismatch => "attachment MIME does not match its bytes",
            Self::DigestMismatch => "attachment digest does not match its metadata",
            Self::Io => "attachment staging is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AttachmentError {}

impl AttachmentStore {
    pub fn open(sessions_dir: &Path) -> Result<Self, AttachmentError> {
        let root = sessions_dir.join(ATTACHMENTS_DIR);
        fs::create_dir_all(&root).map_err(|_| AttachmentError::Io)?;
        set_private_directory(&root).map_err(|_| AttachmentError::Io)?;
        let root = fs::canonicalize(root).map_err(|_| AttachmentError::Io)?;
        Ok(Self { root })
    }

    pub fn stage(
        &self,
        namespace: &str,
        declared_media_type: &str,
        bytes: &[u8],
    ) -> Result<AttachmentRef, AttachmentError> {
        validate_component(namespace)?;
        let declared_media_type = normalize_media_type(declared_media_type);
        if !is_supported_media_type(declared_media_type) {
            return Err(AttachmentError::MimeMismatch);
        }
        if bytes.len() > MAX_RAW_IMAGE_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        let detected = detect_media_type(bytes).ok_or(AttachmentError::Invalid)?;
        if detected != declared_media_type {
            return Err(AttachmentError::MimeMismatch);
        }
        if encoded_len(bytes.len()) > MAX_BASE64_IMAGE_BYTES {
            return Err(AttachmentError::TooLarge);
        }

        let directory = self.namespace_dir(namespace)?;
        fs::create_dir_all(&directory).map_err(|_| AttachmentError::Io)?;
        set_private_directory(&directory).map_err(|_| AttachmentError::Io)?;
        let sha256 = sha256_hex(bytes);
        for _ in 0..16 {
            let id = format!("upload-{}", random_hex(16)?);
            let path = directory.join(&id);
            let temporary = directory.join(format!(".upload-tmp-{}", random_hex(8)?));
            let result = (|| {
                let mut file = private_create_new(&temporary).map_err(|_| AttachmentError::Io)?;
                file.write_all(bytes).map_err(|_| AttachmentError::Io)?;
                file.sync_all().map_err(|_| AttachmentError::Io)?;
                drop(file);
                match fs::rename(&temporary, &path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        Err(AttachmentError::Missing)
                    }
                    Err(_) => Err(AttachmentError::Io),
                }
            })();
            let _ = fs::remove_file(&temporary);
            match result {
                Ok(()) => {
                    sync_directory(&directory);
                    return Ok(AttachmentRef::new(id, detected, bytes.len() as u64, sha256));
                }
                Err(AttachmentError::Missing) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(AttachmentError::Io)
    }

    pub fn validate_content(
        &self,
        namespace: &str,
        content: &[ContentBlock],
    ) -> Result<(), AttachmentError> {
        validate_component(namespace)?;
        let mut encoded_total = 0usize;
        for block in content {
            let ContentBlock::Image { source } = block else {
                continue;
            };
            let ImageSource::Attachment {
                id,
                media_type,
                byte_len,
                sha256,
            } = source
            else {
                return Err(AttachmentError::Invalid);
            };
            let bytes = self.read_ref(namespace, id, media_type, *byte_len, sha256)?;
            encoded_total = encoded_total.saturating_add(encoded_len(bytes.len()));
            if encoded_total > MAX_IMAGE_REQUEST_BYTES {
                return Err(AttachmentError::TooLarge);
            }
        }
        Ok(())
    }

    pub fn remove_content(&self, namespace: &str, content: Option<&[ContentBlock]>) {
        let Some(content) = content else { return };
        if !valid_component(namespace) {
            return;
        }
        for block in content {
            let ContentBlock::Image {
                source: ImageSource::Attachment { id, .. },
            } = block
            else {
                continue;
            };
            let _ = self.remove_id(namespace, id);
        }
        self.remove_empty_namespace(namespace);
    }

    pub fn remove_id(&self, namespace: &str, id: &str) -> Result<bool, AttachmentError> {
        validate_component(namespace)?;
        validate_component(id)?;
        let directory = self.namespace_dir(namespace)?;
        let path = directory.join(id);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(AttachmentError::Io),
        };
        if !metadata.file_type().is_file() {
            return Err(AttachmentError::Invalid);
        }
        let canonical = fs::canonicalize(&path).map_err(|_| AttachmentError::Io)?;
        let canonical_directory = fs::canonicalize(&directory).map_err(|_| AttachmentError::Io)?;
        if canonical.parent() != Some(canonical_directory.as_path()) {
            return Err(AttachmentError::Invalid);
        }
        fs::remove_file(canonical).map_err(|_| AttachmentError::Io)?;
        sync_directory(&directory);
        Ok(true)
    }

    pub fn remove_namespace(&self, namespace: &str) -> Result<(), AttachmentError> {
        validate_component(namespace)?;
        let directory = self.namespace_dir(namespace)?;
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(AttachmentError::Io),
        };
        if !metadata.file_type().is_dir() {
            return Err(AttachmentError::Invalid);
        }
        let canonical = fs::canonicalize(&directory).map_err(|_| AttachmentError::Io)?;
        if canonical.parent() != Some(self.root.as_path()) {
            return Err(AttachmentError::Invalid);
        }
        fs::remove_dir_all(canonical).map_err(|_| AttachmentError::Io)?;
        sync_directory(&self.root);
        Ok(())
    }

    pub fn rehome(
        &self,
        from_namespace: &str,
        to_namespace: &str,
        content: Option<&[ContentBlock]>,
    ) -> Result<(), AttachmentError> {
        let Some(content) = content else {
            return Ok(());
        };
        validate_component(from_namespace)?;
        validate_component(to_namespace)?;
        if from_namespace == to_namespace {
            return self.validate_content(from_namespace, content);
        }
        self.validate_content(from_namespace, content)?;
        let from = self.namespace_dir(from_namespace)?;
        let to = self.namespace_dir(to_namespace)?;
        fs::create_dir_all(&to).map_err(|_| AttachmentError::Io)?;
        set_private_directory(&to).map_err(|_| AttachmentError::Io)?;
        let mut moved = Vec::new();
        let mut moved_ids = std::collections::HashSet::new();
        for block in content {
            let ContentBlock::Image {
                source: ImageSource::Attachment { id, .. },
            } = block
            else {
                continue;
            };
            if !moved_ids.insert(id.as_str()) {
                continue;
            }
            let source = from.join(id);
            let destination = to.join(id);
            if destination.exists() {
                rollback_moves(&moved);
                return Err(AttachmentError::Invalid);
            }
            if let Err(error) = fs::rename(&source, &destination) {
                rollback_moves(&moved);
                return Err(if error.kind() == std::io::ErrorKind::NotFound {
                    AttachmentError::Missing
                } else {
                    AttachmentError::Io
                });
            }
            moved.push((source, destination));
        }
        sync_directory(&to);
        self.remove_empty_namespace(from_namespace);
        Ok(())
    }

    pub fn cleanup_expired(&self) {
        let Ok(namespaces) = fs::read_dir(&self.root) else {
            return;
        };
        let now = SystemTime::now();
        for namespace in namespaces.flatten() {
            let path = namespace.path();
            if !namespace
                .file_type()
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(files) = fs::read_dir(&path) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                let expired = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age >= ATTACHMENT_TTL);
                if expired && metadata.is_file() {
                    let _ = fs::remove_file(file.path());
                }
            }
            let _ = fs::remove_dir(&path);
        }
    }

    pub fn cleanup_orphans(&self, live_namespaces: &std::collections::HashSet<String>) {
        let Ok(namespaces) = fs::read_dir(&self.root) else {
            return;
        };
        for namespace in namespaces.flatten() {
            let name = namespace.file_name().to_string_lossy().into_owned();
            if namespace
                .file_type()
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
                && !live_namespaces.contains(&name)
            {
                let _ = fs::remove_dir_all(namespace.path());
            }
        }
    }

    fn read_ref(
        &self,
        namespace: &str,
        id: &str,
        media_type: &str,
        byte_len: u64,
        sha256: &str,
    ) -> Result<Vec<u8>, AttachmentError> {
        validate_component(id)?;
        let media_type = normalize_media_type(media_type);
        if !is_supported_media_type(media_type) {
            return Err(AttachmentError::Invalid);
        }
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AttachmentError::Invalid);
        }
        let directory = self.namespace_dir(namespace)?;
        let path = directory.join(id);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AttachmentError::Missing
            } else {
                AttachmentError::Io
            }
        })?;
        if !metadata.file_type().is_file() {
            return Err(AttachmentError::Invalid);
        }
        let modified = metadata.modified().map_err(|_| AttachmentError::Io)?;
        if SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            >= ATTACHMENT_TTL
        {
            let _ = fs::remove_file(&path);
            return Err(AttachmentError::Expired);
        }
        let canonical = fs::canonicalize(&path).map_err(|_| AttachmentError::Io)?;
        let canonical_directory = fs::canonicalize(&directory).map_err(|_| AttachmentError::Io)?;
        if canonical.parent() != Some(canonical_directory.as_path()) {
            return Err(AttachmentError::Invalid);
        }
        let bytes = fs::read(&canonical).map_err(|_| AttachmentError::Io)?;
        if bytes.len() as u64 != byte_len || bytes.len() > MAX_RAW_IMAGE_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        if encoded_len(bytes.len()) > MAX_BASE64_IMAGE_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        if detect_media_type(&bytes) != Some(media_type) {
            return Err(AttachmentError::MimeMismatch);
        }
        if !sha256.eq_ignore_ascii_case(&sha256_hex(&bytes)) {
            return Err(AttachmentError::DigestMismatch);
        }
        Ok(bytes)
    }

    fn namespace_dir(&self, namespace: &str) -> Result<PathBuf, AttachmentError> {
        validate_component(namespace)?;
        Ok(self.root.join(namespace))
    }

    fn remove_empty_namespace(&self, namespace: &str) {
        if !valid_component(namespace) {
            return;
        }
        let _ = fs::remove_dir(self.root.join(namespace));
    }
}

fn rollback_moves(moved: &[(PathBuf, PathBuf)]) {
    for (source, destination) in moved.iter().rev() {
        let _ = fs::rename(destination, source);
    }
}

fn encoded_len(raw_len: usize) -> usize {
    raw_len.div_ceil(3) * 4
}

fn normalize_media_type(value: &str) -> &str {
    value.split(';').next().unwrap_or(value).trim()
}

fn is_supported_media_type(value: &str) -> bool {
    SUPPORTED_MEDIA_TYPES.contains(&value)
}

fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn validate_component(value: &str) -> Result<(), AttachmentError> {
    if valid_component(value) {
        Ok(())
    } else {
        Err(AttachmentError::Invalid)
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn random_hex(bytes: usize) -> Result<String, AttachmentError> {
    let mut value = vec![0u8; bytes];
    getrandom::fill(&mut value).map_err(|_| AttachmentError::Io)?;
    Ok(sha256_hex(&value)[..bytes * 2].to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn set_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn private_create_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (AttachmentStore, tempfile::TempDir) {
        let temp = tempdir().unwrap();
        let store = AttachmentStore::open(&temp.path().join("sessions")).unwrap();
        (store, temp)
    }

    #[test]
    fn stages_and_validates_a_png_without_exposing_a_path() {
        let (store, _temp) = store();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        let reference = store.stage("sess-1", "image/png", bytes).unwrap();
        assert!(reference.id.starts_with("upload-"));
        assert_eq!(reference.byte_len, bytes.len() as u64);
        assert_eq!(reference.sha256, sha256_hex(bytes));
        let content = [ContentBlock::image(reference.clone())];
        store.validate_content("sess-1", &content).unwrap();
        assert!(!format!("{reference:?}").contains("sessions"));
    }

    #[test]
    fn rejects_mismatched_magic_and_mime() {
        let (store, _temp) = store();
        assert_eq!(
            store.stage("sess-1", "image/jpeg", b"not-an-image"),
            Err(AttachmentError::Invalid)
        );
        assert_eq!(
            store.stage("sess-1", "image/jpeg", b"\x89PNG\r\n\x1a\nfixture"),
            Err(AttachmentError::MimeMismatch)
        );
    }

    #[test]
    fn rehomes_refs_without_changing_their_identity() {
        let (store, _temp) = store();
        let reference = store
            .stage("draft-1", "image/png", b"\x89PNG\r\n\x1a\nfixture")
            .unwrap();
        let content = [ContentBlock::image(reference.clone())];
        store.rehome("draft-1", "sess-1", Some(&content)).unwrap();
        assert_eq!(
            reference,
            match &content[0] {
                ContentBlock::Image {
                    source:
                        ImageSource::Attachment {
                            id,
                            media_type,
                            byte_len,
                            sha256,
                        },
                } => AttachmentRef::new(id.clone(), media_type.clone(), *byte_len, sha256.clone()),
                _ => unreachable!(),
            }
        );
        store.validate_content("sess-1", &content).unwrap();
        assert!(store.validate_content("draft-1", &content).is_err());
    }
}
