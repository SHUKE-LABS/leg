//! Ordered companion draft content and validated image staging metadata.
//!
//! A draft image is owned by the catalog process: its bytes live in a private
//! catalog store and only an opaque [`AttachmentRef`] plus a display label
//! (a basename or the clipboard marker) reaches a UI. The label is never a
//! path and never crosses the companion wire.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalog::TrailOutcome;
use crate::content::{
    AttachmentRef, ContentBlock, MAX_IMAGE_ENCODED_BYTES, MAX_TURN_IMAGE_ENCODED_BYTES,
    base64_encoded_len, detect_image_media_type, sha256_hex, turn_fingerprint,
};

/// Display label for an image read from the host clipboard.
pub const CLIPBOARD_IMAGE_LABEL: &str = "clipboard image";
const MAX_LABEL_CHARS: usize = 80;

/// One staged draft image and its UI-safe label.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DraftImage {
    pub attachment: AttachmentRef,
    pub label: String,
}

/// One ordered draft segment.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DraftBlock {
    Text { text: String },
    Image(DraftImage),
}

impl DraftBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    fn image(&self) -> Option<&DraftImage> {
        match self {
            Self::Image(image) => Some(image),
            Self::Text { .. } => None,
        }
    }
}

/// A recorded send of one draft. The request identity is reused while the
/// draft fingerprint is unchanged, and its blocks keep the staged images owned
/// until the submission is settled.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DraftSubmission {
    pub request_id: String,
    pub fingerprint: String,
    pub blocks: Vec<DraftBlock>,
    /// Number of trail turns before this submission started.
    pub base_turns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionClaim {
    /// The request identity is new and the caller may start the turn.
    Fresh,
    /// The same request identity and content were already submitted; the
    /// caller must not start a second turn.
    Replay,
}

/// The durable result of a recorded submission after reconnecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmissionResolution {
    None,
    /// The session still runs or its ownership cannot be verified.
    Pending {
        request_id: String,
    },
    /// The trail records the submitted turn.
    Accepted {
        request_id: String,
        turn_index: u64,
        outcome: TrailOutcome,
    },
    /// No turn was recorded; the draft needs a new explicit send.
    NotAccepted {
        request_id: String,
    },
}

/// Bounded, basename-only image validation and staging errors.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AttachmentError {
    #[error("{label}: not a supported JPEG, PNG, GIF, or WebP image")]
    Unsupported { label: String },
    #[error("{label}: named as {declared} but the bytes are {detected}")]
    MimeMismatch {
        label: String,
        declared: String,
        detected: String,
    },
    #[error("{label}: exceeds the 10 MB per-image limit (base64-encoded)")]
    TooLarge { label: String },
    #[error("{label}: draft images would exceed the 32 MB per-turn limit (base64-encoded)")]
    AggregateTooLarge { label: String },
    #[error("{label}: could not stage the image ({reason})")]
    Staging { label: String, reason: String },
}

/// Reduces a display name to a bounded, control-free basename.
pub fn attachment_label(raw: &str) -> String {
    let basename = raw
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let label = basename
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_LABEL_CHARS)
        .collect::<String>();
    if label.trim().is_empty() {
        "image".to_string()
    } else {
        label
    }
}

/// Validates image bytes against the fixed limits and magic MIME. Returns the
/// detected MIME and SHA-256 digest.
pub(crate) fn validate_image_bytes(
    bytes: &[u8],
    declared_media_type: Option<&str>,
    label: &str,
    existing: &[AttachmentRef],
) -> Result<(&'static str, String), AttachmentError> {
    let label = label.to_string();
    let encoded = base64_encoded_len(bytes.len() as u64);
    if encoded > MAX_IMAGE_ENCODED_BYTES {
        return Err(AttachmentError::TooLarge { label });
    }
    let Some(detected) = detect_image_media_type(bytes) else {
        return Err(AttachmentError::Unsupported { label });
    };
    if let Some(declared) = declared_media_type
        && declared != detected
    {
        return Err(AttachmentError::MimeMismatch {
            label,
            declared: declared.to_string(),
            detected: detected.to_string(),
        });
    }
    let total = existing
        .iter()
        .map(|attachment| base64_encoded_len(attachment.byte_len))
        .fold(encoded, u64::saturating_add);
    if total > MAX_TURN_IMAGE_ENCODED_BYTES {
        return Err(AttachmentError::AggregateTooLarge { label });
    }
    Ok((detected, sha256_hex(bytes)))
}

/// The prompt text projection: every text segment in order, images omitted.
pub fn draft_prompt(blocks: &[DraftBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            DraftBlock::Text { text } => Some(text.as_str()),
            DraftBlock::Image(_) => None,
        })
        .collect()
}

pub fn draft_images(blocks: &[DraftBlock]) -> impl Iterator<Item = &DraftImage> {
    blocks.iter().filter_map(DraftBlock::image)
}

/// Ordered request content, or `None` for a text-only draft so the legacy
/// text request shape is preserved. Only empty text segments are omitted;
/// whitespace between images is part of the prompt.
pub fn draft_content(blocks: &[DraftBlock]) -> Option<Vec<ContentBlock>> {
    draft_images(blocks).next()?;
    Some(
        blocks
            .iter()
            .filter_map(|block| match block {
                DraftBlock::Text { text } if text.is_empty() => None,
                DraftBlock::Text { text } => Some(ContentBlock::text(text.clone())),
                DraftBlock::Image(image) => Some(ContentBlock::image(image.attachment.clone())),
            })
            .collect(),
    )
}

/// Identity of a draft send: ordered text and image MIME/length/SHA-256.
pub fn draft_fingerprint(blocks: &[DraftBlock]) -> String {
    turn_fingerprint(&draft_prompt(blocks), draft_content(blocks).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str, sha: char) -> DraftBlock {
        DraftBlock::Image(DraftImage {
            attachment: AttachmentRef::new(id, "image/png", 9, sha.to_string().repeat(64)),
            label: "shot.png".into(),
        })
    }

    #[test]
    fn labels_are_bounded_basenames() {
        assert_eq!(attachment_label("/home/me/secret/shot.png"), "shot.png");
        assert_eq!(attachment_label("C:\\Users\\me\\shot.png"), "shot.png");
        assert_eq!(attachment_label("dir/"), "dir");
        assert_eq!(attachment_label("a\u{1b}[31mb.png"), "a[31mb.png");
        assert_eq!(attachment_label(""), "image");
        assert_eq!(attachment_label(&"x".repeat(200)).chars().count(), 80);
    }

    #[test]
    fn ordered_content_and_fingerprint_follow_text_and_image_identity() {
        let blocks = vec![
            DraftBlock::text("before "),
            image("a", '1'),
            DraftBlock::text(" "),
            image("b", '2'),
            DraftBlock::text(" after"),
        ];
        assert_eq!(draft_prompt(&blocks), "before   after");
        let content = draft_content(&blocks).unwrap();
        assert_eq!(content.len(), 5);
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "before "));
        assert!(content[1].is_managed_attachment());
        assert!(matches!(&content[2], ContentBlock::Text { text } if text == " "));
        assert!(content[3].is_managed_attachment());
        let with_empty = vec![DraftBlock::text(""), image("a", '1'), DraftBlock::text("x")];
        assert_eq!(draft_content(&with_empty).unwrap().len(), 2);

        let same_with_new_ids = vec![
            DraftBlock::text("before "),
            image("c", '1'),
            DraftBlock::text(" "),
            image("d", '2'),
            DraftBlock::text(" after"),
        ];
        assert_eq!(
            draft_fingerprint(&blocks),
            draft_fingerprint(&same_with_new_ids)
        );
        let removed = vec![
            DraftBlock::text("before "),
            image("a", '1'),
            DraftBlock::text("  after"),
        ];
        assert_ne!(draft_fingerprint(&blocks), draft_fingerprint(&removed));
        let reordered = vec![
            DraftBlock::text("before "),
            image("b", '2'),
            DraftBlock::text(" "),
            image("a", '1'),
            DraftBlock::text(" after"),
        ];
        assert_ne!(draft_fingerprint(&blocks), draft_fingerprint(&reordered));

        let text_only = vec![DraftBlock::text("plain")];
        assert_eq!(draft_content(&text_only), None);
        assert_eq!(
            draft_fingerprint(&text_only),
            turn_fingerprint("plain", None)
        );
    }

    #[test]
    fn validation_checks_size_magic_mime_and_aggregate() {
        let png = b"\x89PNG\r\n\x1a\nimage".to_vec();
        let (mime, sha) = validate_image_bytes(&png, Some("image/png"), "a.png", &[]).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(sha.len(), 64);
        assert_eq!(
            validate_image_bytes(b"plain text", None, "a.png", &[]),
            Err(AttachmentError::Unsupported {
                label: "a.png".into()
            })
        );
        assert!(matches!(
            validate_image_bytes(&png, Some("image/jpeg"), "a.jpg", &[]),
            Err(AttachmentError::MimeMismatch { .. })
        ));
        let oversized = vec![0u8; 7_500_001];
        assert_eq!(
            validate_image_bytes(&oversized, None, "big.png", &[]),
            Err(AttachmentError::TooLarge {
                label: "big.png".into()
            })
        );
        let existing = [
            AttachmentRef::new("a", "image/png", 7_500_000, "0".repeat(64)),
            AttachmentRef::new("b", "image/png", 7_500_000, "0".repeat(64)),
            AttachmentRef::new("c", "image/png", 7_500_000, "0".repeat(64)),
        ];
        let mut near_limit = png.clone();
        near_limit.resize(1_500_001, 0);
        assert_eq!(
            validate_image_bytes(&near_limit, None, "d.png", &existing),
            Err(AttachmentError::AggregateTooLarge {
                label: "d.png".into()
            })
        );
        near_limit.truncate(1_500_000);
        assert!(validate_image_bytes(&near_limit, None, "d.png", &existing).is_ok());
    }
}
