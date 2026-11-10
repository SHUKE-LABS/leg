use std::fs::File;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use leg_image_contract::{
    MAX_IMAGE_ENCODED_BYTES, MAX_TURN_IMAGE_ENCODED_BYTES, base64_encoded_len,
    detect_image_media_type,
};

use crate::error::{LegError, Result};
use crate::model::{ContentBlock, ImageSource, Message};

pub(crate) const MAX_BASE64_IMAGE_BYTES: usize = MAX_IMAGE_ENCODED_BYTES as usize;
pub(crate) const MAX_IMAGE_REQUEST_BYTES: usize = MAX_TURN_IMAGE_ENCODED_BYTES as usize;
const MAX_RAW_IMAGE_BYTES: usize = (MAX_IMAGE_ENCODED_BYTES / 4 * 3) as usize;
const ATTACHMENTS_DIR: &str = "attachments";

/// Resolves companion attachment references into provider-ready image blocks.
///
/// The reference itself is safe to carry through the companion supervisor and
/// `leg exchange` stdin. Only this trusted resolver opens the managed staging
/// store and materializes image bytes.
pub(crate) fn resolve_content(
    content: &[ContentBlock],
    session_id: Option<&str>,
) -> Result<Vec<ContentBlock>> {
    let mut encoded_total = 0usize;
    resolve_content_with_total(content, session_id, &mut encoded_total, true)
}

/// Resolves every image in a conversation while retaining the caller's role
/// and ordered message boundaries. The returned copy is provider-facing; the
/// session history can then retain that validated copy for replay after staged
/// files are cleaned up.
pub(crate) fn resolve_messages(
    messages: &[Message],
    session_id: Option<&str>,
) -> Result<Vec<Message>> {
    let mut encoded_total = 0usize;
    messages
        .iter()
        .map(|message| {
            Ok(Message::new(
                message.role,
                resolve_content_with_total(
                    &message.content,
                    session_id,
                    &mut encoded_total,
                    false,
                )?,
            ))
        })
        .collect()
}

/// Removes the managed staged files referenced by a completed companion turn.
///
/// The private session trail contains the validated provider content by the
/// time this runs, so replay does not depend on these files. Cleanup is best
/// effort: a missing or already-invalidated ref is harmless, and no path is
/// included in the returned diagnostics or stream records.
pub(crate) fn cleanup_staged_content(content: &[ContentBlock], session_id: &str) {
    if !safe_component(session_id) {
        return;
    }
    let Some(store) = std::env::var_os("LEG_SESSION_DIR") else {
        return;
    };
    let Ok(root) = std::fs::canonicalize(store) else {
        return;
    };
    cleanup_staged_content_from_root(&root, content, session_id);
}

fn cleanup_staged_content_from_root(root: &Path, content: &[ContentBlock], session_id: &str) {
    let Ok(root) = std::fs::canonicalize(root) else {
        return;
    };
    let session_dir = root.join(ATTACHMENTS_DIR).join(session_id);
    let mut removed = std::collections::HashSet::new();
    for block in content {
        let ContentBlock::Image {
            source: ImageSource::Attachment { id, .. },
        } = block
        else {
            continue;
        };
        if !safe_component(id) || !removed.insert(id.clone()) {
            continue;
        }
        let path = session_dir.join(id);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(&path) else {
            continue;
        };
        if canonical.parent() == Some(session_dir.as_path()) {
            let _ = std::fs::remove_file(canonical);
        }
    }
}

fn resolve_content_with_total(
    content: &[ContentBlock],
    session_id: Option<&str>,
    encoded_total: &mut usize,
    enforce_combined_limit: bool,
) -> Result<Vec<ContentBlock>> {
    let mut resolved = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ContentBlock::Image {
                source:
                    ImageSource::Attachment {
                        id,
                        media_type,
                        byte_len,
                        sha256,
                    },
            } => {
                let session_id = session_id.ok_or_else(|| {
                    LegError::Usage(
                        "image attachment refs require a managed exchange session".to_string(),
                    )
                })?;
                let bytes = read_staged_attachment(session_id, id, media_type, *byte_len, sha256)?;
                let data = STANDARD.encode(bytes);
                if data.len() > MAX_BASE64_IMAGE_BYTES {
                    return Err(LegError::Usage(
                        "base64-encoded image exceeds the 10 MB Messages API per-image limit"
                            .to_string(),
                    ));
                }
                *encoded_total = encoded_total.saturating_add(data.len());
                if enforce_combined_limit && *encoded_total > MAX_IMAGE_REQUEST_BYTES {
                    return Err(LegError::Usage(
                        "combined image data exceeds the 32 MB Messages API request limit"
                            .to_string(),
                    ));
                }
                resolved.push(ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: media_type.clone(),
                        data,
                    },
                });
            }
            ContentBlock::Image {
                source: ImageSource::Base64 { data, .. },
            } => {
                *encoded_total = encoded_total.saturating_add(data.len());
                if data.len() > MAX_BASE64_IMAGE_BYTES {
                    return Err(LegError::Usage(
                        "base64-encoded image exceeds the 10 MB Messages API per-image limit"
                            .to_string(),
                    ));
                }
                if enforce_combined_limit && *encoded_total > MAX_IMAGE_REQUEST_BYTES {
                    return Err(LegError::Usage(
                        "combined image data exceeds the 32 MB Messages API request limit"
                            .to_string(),
                    ));
                }
                resolved.push(block.clone());
            }
            _ => resolved.push(block.clone()),
        }
    }
    Ok(resolved)
}

/// Validates image payloads supplied by an untrusted message envelope.
///
/// Companion attachment refs are checked by the managed staging resolver;
/// inline base64 is checked here before a provider transport can see it.
pub(crate) fn validate_untrusted_content(content: &[ContentBlock]) -> Result<()> {
    let mut encoded_total = 0usize;
    for block in content {
        let ContentBlock::Image {
            source: ImageSource::Base64 { media_type, data },
        } = block
        else {
            continue;
        };
        validate_base64_image(media_type, data)?;
        encoded_total = encoded_total.saturating_add(data.len());
        if encoded_total > MAX_IMAGE_REQUEST_BYTES {
            return Err(LegError::Usage(
                "combined image data exceeds the 32 MB Messages API request limit".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_base64_image(media_type: &str, data: &str) -> Result<()> {
    if data.len() > MAX_BASE64_IMAGE_BYTES {
        return Err(LegError::Usage(
            "base64-encoded image exceeds the 10 MB Messages API per-image limit".to_string(),
        ));
    }
    if !matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Err(LegError::Usage(
            "image has an unsupported MIME type; use JPEG, PNG, GIF, or WebP".to_string(),
        ));
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| LegError::Usage("image data is not valid base64".to_string()))?;
    let detected = detect_image_media_type(&bytes).ok_or_else(|| {
        LegError::Usage("image data is not a supported JPEG, PNG, GIF, or WebP image".to_string())
    })?;
    if detected != media_type {
        return Err(LegError::Usage(
            "image MIME type does not match its bytes".to_string(),
        ));
    }
    Ok(())
}

fn read_staged_attachment(
    session_id: &str,
    id: &str,
    media_type: &str,
    byte_len: u64,
    sha256: &str,
) -> Result<Vec<u8>> {
    if !safe_component(session_id) || !safe_component(id) {
        return Err(LegError::Usage(
            "image attachment ref has an invalid id".to_string(),
        ));
    }
    if !matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Err(LegError::Usage(
            "image attachment ref has an unsupported MIME type".to_string(),
        ));
    }
    if sha256.len() != 64 || !sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(LegError::Usage(
            "image attachment ref has an invalid SHA-256 digest".to_string(),
        ));
    }
    let store = std::env::var_os("LEG_SESSION_DIR").ok_or_else(|| {
        LegError::Config("LEG_SESSION_DIR is required to resolve image attachments".to_string())
    })?;
    let root = std::fs::canonicalize(&store)
        .map_err(|_| LegError::Usage("image attachment staging is unavailable".to_string()))?;
    read_staged_attachment_from_root(&root, session_id, id, media_type, byte_len, sha256)
}

fn read_staged_attachment_from_root(
    root: &Path,
    session_id: &str,
    id: &str,
    media_type: &str,
    byte_len: u64,
    sha256: &str,
) -> Result<Vec<u8>> {
    let root = std::fs::canonicalize(root)
        .map_err(|_| LegError::Usage("image attachment staging is unavailable".to_string()))?;
    let session_dir = root.join(ATTACHMENTS_DIR).join(session_id);
    let path = session_dir.join(id);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| LegError::Usage("image attachment staging is missing".to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(LegError::Usage(
            "image attachment staging is not a regular file".to_string(),
        ));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|_| LegError::Usage("image attachment staging is unavailable".to_string()))?;
    if canonical.parent() != Some(session_dir.as_path()) {
        return Err(LegError::Usage(
            "image attachment ref escapes its session staging directory".to_string(),
        ));
    }
    let file = File::open(&canonical)
        .map_err(|_| LegError::Usage("image attachment staging is unavailable".to_string()))?;
    if file.metadata().map(|meta| meta.len()).unwrap_or(u64::MAX) != byte_len {
        return Err(LegError::Usage(
            "image attachment length does not match its metadata".to_string(),
        ));
    }
    if byte_len > MAX_RAW_IMAGE_BYTES as u64
        || base64_encoded_len(byte_len) > MAX_IMAGE_ENCODED_BYTES
    {
        return Err(LegError::Usage(
            "image attachment exceeds the 10 MB base64-encoded size limit".to_string(),
        ));
    }
    let mut bytes = Vec::with_capacity(byte_len as usize);
    file.take(MAX_RAW_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LegError::Usage("image attachment staging is unreadable".to_string()))?;
    if bytes.len() as u64 != byte_len {
        return Err(LegError::Usage(
            "image attachment length does not match its metadata".to_string(),
        ));
    }
    let detected = detect_image_media_type(&bytes).ok_or_else(|| {
        LegError::Usage(
            "image attachment is not a supported JPEG, PNG, GIF, or WebP image".to_string(),
        )
    })?;
    if detected != media_type {
        return Err(LegError::Usage(
            "image attachment MIME does not match its bytes".to_string(),
        ));
    }
    let digest = crate::bridge::protocol::sha256_hex(&bytes);
    if !digest.eq_ignore_ascii_case(sha256) {
        return Err(LegError::Usage(
            "image attachment digest does not match its metadata".to_string(),
        ));
    }
    Ok(bytes)
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

pub(crate) fn load_images(paths: &[String]) -> Result<Vec<ContentBlock>> {
    let mut images = Vec::with_capacity(paths.len());
    let mut encoded_total = 0usize;

    for path in paths {
        let file_path = Path::new(path);
        let file = File::open(file_path)
            .map_err(|err| LegError::Io(format!("failed to open --image file {path:?}: {err}")))?;
        let file_size = file
            .metadata()
            .map_err(|err| LegError::Io(format!("failed to inspect --image file {path:?}: {err}")))?
            .len();
        if file_size > MAX_RAW_IMAGE_BYTES as u64
            || base64_encoded_len(file_size) > MAX_IMAGE_ENCODED_BYTES
        {
            return Err(image_too_large(path));
        }

        let mut bytes = Vec::with_capacity(file_size as usize);
        let mut limited = file.take(MAX_RAW_IMAGE_BYTES as u64 + 1);
        limited
            .read_to_end(&mut bytes)
            .map_err(|err| LegError::Io(format!("failed to read --image file {path:?}: {err}")))?;
        if bytes.len() > MAX_RAW_IMAGE_BYTES {
            return Err(image_too_large(path));
        }

        let media_type = detect_image_media_type(&bytes).ok_or_else(|| {
            LegError::Usage(format!(
                "--image file {path:?} is not a supported JPEG, PNG, GIF, or WebP image"
            ))
        })?;
        let data = STANDARD.encode(bytes);
        if data.len() > MAX_BASE64_IMAGE_BYTES {
            return Err(image_too_large(path));
        }
        encoded_total = encoded_total.saturating_add(data.len());
        if encoded_total > MAX_IMAGE_REQUEST_BYTES {
            return Err(LegError::Usage(
                "combined --image data exceeds the 32 MB Messages API request limit".to_string(),
            ));
        }

        images.push(ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: media_type.to_string(),
                data,
            },
        });
    }

    Ok(images)
}

pub(crate) fn validate_message_image_limits(messages: &[Message]) -> Result<()> {
    let mut encoded_total = 0usize;
    for block in messages.iter().flat_map(|message| &message.content) {
        if let ContentBlock::Image { source } = block {
            let ImageSource::Base64 { data, .. } = source else {
                return Err(LegError::Usage(
                    "image attachment was not resolved before the provider call".to_string(),
                ));
            };
            if data.len() > MAX_BASE64_IMAGE_BYTES {
                return Err(LegError::Usage(
                    "base64-encoded image exceeds the 10 MB Messages API per-image limit"
                        .to_string(),
                ));
            }
            encoded_total = encoded_total.saturating_add(data.len());
            if encoded_total > MAX_IMAGE_REQUEST_BYTES {
                return Err(LegError::Usage(
                    "combined image data exceeds the 32 MB Messages API request limit".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn image_too_large(path: &str) -> LegError {
    LegError::Usage(format!(
        "--image file {path:?} exceeds the 10 MB base64-encoded size limit"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "leg-image-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn detects_supported_image_formats_from_file_signatures() {
        for (bytes, media_type) in [
            (&b"\xff\xd8\xff"[..], "image/jpeg"),
            (&b"\x89PNG\r\n\x1a\n"[..], "image/png"),
            (&b"GIF87a"[..], "image/gif"),
            (&b"GIF89a"[..], "image/gif"),
            (&b"RIFFxxxxWEBP"[..], "image/webp"),
        ] {
            assert_eq!(detect_image_media_type(bytes), Some(media_type));
        }
        assert_eq!(detect_image_media_type(b"not an image"), None);
    }

    #[test]
    fn loads_image_as_messages_api_base64_block() {
        let path = temp_path();
        fs::write(&path, b"\x89PNG\r\n\x1a\nimage").expect("writes fixture");

        let images = load_images(&[path.to_string_lossy().into_owned()]).expect("loads image");
        fs::remove_file(path).expect("removes fixture");

        assert_eq!(
            images,
            vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "iVBORw0KGgppbWFnZQ==".to_string(),
                },
            }]
        );
    }

    #[test]
    fn rejects_unsupported_image_data_with_a_clear_error() {
        let path = temp_path();
        fs::write(&path, b"not an image").expect("writes fixture");

        let err = load_images(&[path.to_string_lossy().into_owned()]).unwrap_err();
        fs::remove_file(path).expect("removes fixture");

        assert!(matches!(err, LegError::Usage(_)));
        assert!(err.to_string().contains("JPEG, PNG, GIF, or WebP"));
    }

    #[test]
    fn rejects_images_above_the_base64_size_limit_before_reading_them() {
        let path = temp_path();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("creates fixture");
        file.set_len(MAX_RAW_IMAGE_BYTES as u64 + 1)
            .expect("extends sparse fixture");
        drop(file);

        let err = load_images(&[path.to_string_lossy().into_owned()]).unwrap_err();
        fs::remove_file(path).expect("removes fixture");

        assert!(matches!(err, LegError::Usage(_)));
        assert!(err.to_string().contains("10 MB base64-encoded size limit"));
    }

    #[test]
    fn validates_both_per_image_and_combined_request_limits() {
        let over_limit = "x".repeat(MAX_BASE64_IMAGE_BYTES + 1);
        let messages = [Message::new(
            crate::model::Role::User,
            vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: over_limit,
                },
            }],
        )];
        assert!(matches!(
            validate_message_image_limits(&messages),
            Err(LegError::Usage(_))
        ));

        let image_data = "x".repeat(MAX_IMAGE_REQUEST_BYTES / 4 + 1);
        assert!(image_data.len() <= MAX_BASE64_IMAGE_BYTES);
        let messages = [Message::new(
            crate::model::Role::User,
            (0..4)
                .map(|_| ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".to_string(),
                        data: image_data.clone(),
                    },
                })
                .collect(),
        )];
        assert!(matches!(
            validate_message_image_limits(&messages),
            Err(LegError::Usage(_))
        ));
    }

    #[test]
    fn rejects_invalid_inline_image_payloads_before_provider_use() {
        for (media_type, data, message) in [
            ("image/png", "not-base64", "image data is not valid base64"),
            ("image/tiff", "iVBORw0KGgo=", "unsupported MIME type"),
            (
                "image/png",
                "aGVsbG8=",
                "supported JPEG, PNG, GIF, or WebP image",
            ),
        ] {
            let content = [ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: media_type.to_string(),
                    data: data.to_string(),
                },
            }];
            let error = validate_untrusted_content(&content).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn resolves_message_copies_without_mutating_attachment_refs() {
        let messages = [Message::new(
            crate::model::Role::User,
            vec![
                ContentBlock::text("inspect"),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".to_string(),
                        data: "aGVsbG8=".to_string(),
                    },
                },
            ],
        )];

        let resolved = resolve_messages(&messages, Some("sess-test")).expect("resolves");

        assert_eq!(resolved, messages);
        assert_eq!(messages[0].content[0], ContentBlock::text("inspect"));
    }

    #[test]
    fn refuses_to_resolve_an_attachment_without_a_managed_session() {
        let messages = [Message::new(
            crate::model::Role::User,
            vec![ContentBlock::Image {
                source: ImageSource::Attachment {
                    id: "upload-1".to_string(),
                    media_type: "image/png".to_string(),
                    byte_len: 8,
                    sha256: "0".repeat(64),
                },
            }],
        )];

        let error = resolve_messages(&messages, None).unwrap_err();

        assert!(error.to_string().contains("managed exchange session"));
    }

    #[test]
    fn resolves_a_staged_attachment_only_after_validating_its_metadata() {
        let root = temp_path();
        let session_dir = root.join(ATTACHMENTS_DIR).join("sess-test");
        fs::create_dir_all(&session_dir).expect("creates staging directory");
        let bytes = b"\x89PNG\r\n\x1a\nimage";
        fs::write(session_dir.join("upload-1"), bytes).expect("writes staged image");
        let digest = crate::bridge::protocol::sha256_hex(bytes);

        let resolved = read_staged_attachment_from_root(
            &root,
            "sess-test",
            "upload-1",
            "image/png",
            bytes.len() as u64,
            &digest,
        )
        .expect("resolves staged image");
        assert_eq!(resolved, bytes);

        let error = read_staged_attachment_from_root(
            &root,
            "sess-test",
            "upload-1",
            "image/png",
            bytes.len() as u64,
            &"0".repeat(64),
        )
        .unwrap_err();
        assert!(error.to_string().contains("digest does not match"));

        fs::remove_dir_all(root).expect("removes staging fixture");
    }

    #[test]
    fn terminal_cleanup_removes_only_validated_session_files() {
        let root = temp_path();
        let session_dir = root.join(ATTACHMENTS_DIR).join("sess-test");
        fs::create_dir_all(&session_dir).expect("creates staging directory");
        fs::write(session_dir.join("upload-1"), b"image").expect("writes staged image");
        fs::write(session_dir.join("upload-2"), b"keep").expect("writes second image");
        cleanup_staged_content_from_root(
            &root,
            &[
                ContentBlock::Image {
                    source: ImageSource::Attachment {
                        id: "upload-1".to_string(),
                        media_type: "image/png".to_string(),
                        byte_len: 5,
                        sha256: "0".repeat(64),
                    },
                },
                ContentBlock::Image {
                    source: ImageSource::Attachment {
                        id: "../upload-2".to_string(),
                        media_type: "image/png".to_string(),
                        byte_len: 4,
                        sha256: "0".repeat(64),
                    },
                },
            ],
            "sess-test",
        );
        assert!(!session_dir.join("upload-1").exists());
        assert!(session_dir.join("upload-2").exists());
        fs::remove_dir_all(root).expect("removes fixture");
    }
}
