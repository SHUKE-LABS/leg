//! Safe, provider-neutral turn content carried by companion wire requests.
//!
//! Images cross the companion boundary only as a managed attachment identity.
//! The native leg process resolves that identity inside its trusted session
//! store; this crate never accepts a filesystem path or image bytes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Identity of one image staged under a managed session directory.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AttachmentRef {
    pub id: String,
    pub media_type: String,
    pub byte_len: u64,
    pub sha256: String,
}

impl AttachmentRef {
    pub fn new(
        id: impl Into<String>,
        media_type: impl Into<String>,
        byte_len: u64,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            media_type: media_type.into(),
            byte_len,
            sha256: sha256.into(),
        }
    }
}

/// Image source shape shared with `baton.message/v1`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// A managed, session-scoped image reference.
    Attachment {
        id: String,
        media_type: String,
        byte_len: u64,
        sha256: String,
    },
    /// Provider payloads are accepted when reading a legacy trail, but are
    /// never produced by the companion request API.
    Base64 { media_type: String, data: String },
}

impl From<AttachmentRef> for ImageSource {
    fn from(value: AttachmentRef) -> Self {
        Self::Attachment {
            id: value.id,
            media_type: value.media_type,
            byte_len: value.byte_len,
            sha256: value.sha256,
        }
    }
}

/// Ordered content blocks accepted by a companion turn request and retained
/// in the parsed exchange trail.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(attachment: AttachmentRef) -> Self {
        Self::Image {
            source: attachment.into(),
        }
    }

    pub fn is_managed_attachment(&self) -> bool {
        matches!(
            self,
            Self::Image {
                source: ImageSource::Attachment { .. }
            }
        )
    }

    /// Whether this block is safe to carry across the companion boundary.
    /// Image bytes and filesystem-like attachment ids never enter that wire.
    pub fn is_wire_safe(&self) -> bool {
        match self {
            Self::Image {
                source:
                    ImageSource::Attachment {
                        id,
                        media_type,
                        sha256,
                        ..
                    },
            } => {
                safe_attachment_id(id)
                    && matches!(
                        media_type.as_str(),
                        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                    )
                    && sha256.len() == 64
                    && sha256.chars().all(|ch| ch.is_ascii_hexdigit())
            }
            Self::Image {
                source: ImageSource::Base64 { .. },
            } => false,
            _ => true,
        }
    }
}

fn safe_attachment_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// A stable fingerprint over the user-visible prompt and content identity.
/// Text-only requests retain the legacy prompt-only digest for durable-store
/// compatibility; multimodal requests include ordered text and image
/// MIME/length/digest (or inline content) while ignoring ephemeral ref ids.
pub fn turn_fingerprint(prompt: &str, content: Option<&[ContentBlock]>) -> String {
    let bytes = match content {
        None => prompt.as_bytes().to_vec(),
        Some(content) => {
            let content = fingerprint_content(content);
            serde_json::to_vec(&FingerprintInput { prompt, content })
                .expect("turn content is serializable")
        }
    };
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Serialize)]
struct FingerprintInput<'a> {
    prompt: &'a str,
    content: Vec<FingerprintBlock<'a>>,
}

/// Removes the ephemeral staged-file id from the canonical identity. A retry
/// may re-stage the same validated bytes under a fresh id after terminal
/// cleanup; MIME, length, digest, order, and text must still compare equal.
fn fingerprint_content(content: &[ContentBlock]) -> Vec<FingerprintBlock<'_>> {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => FingerprintBlock::Text { text },
            ContentBlock::Image { source } => FingerprintBlock::Image {
                source: match source {
                    ImageSource::Attachment {
                        media_type,
                        byte_len,
                        sha256,
                        ..
                    } => FingerprintImageSource::Attachment {
                        media_type,
                        byte_len: *byte_len,
                        sha256,
                    },
                    ImageSource::Base64 { media_type, data } => {
                        FingerprintImageSource::Base64 { media_type, data }
                    }
                },
            },
            ContentBlock::Thinking {
                thinking,
                signature,
            } => FingerprintBlock::Thinking {
                thinking,
                signature,
            },
            ContentBlock::ToolUse { id, name, input } => {
                FingerprintBlock::ToolUse { id, name, input }
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => FingerprintBlock::ToolResult {
                tool_use_id,
                content,
                is_error: *is_error,
            },
        })
        .collect()
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FingerprintBlock<'a> {
    Text {
        text: &'a str,
    },
    Image {
        source: FingerprintImageSource<'a>,
    },
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FingerprintImageSource<'a> {
    Attachment {
        media_type: &'a str,
        byte_len: u64,
        sha256: &'a str,
    },
    Base64 {
        media_type: &'a str,
        data: &'a str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_digest_identity_is_stable_across_staging_ids() {
        let first = [ContentBlock::image(AttachmentRef::new(
            "upload-1",
            "image/png",
            5,
            "0".repeat(64),
        ))];
        let second = [ContentBlock::image(AttachmentRef::new(
            "upload-2",
            "image/png",
            5,
            "0".repeat(64),
        ))];

        assert_eq!(
            turn_fingerprint("describe", Some(&first)),
            turn_fingerprint("describe", Some(&second))
        );
    }

    #[test]
    fn multimodal_fingerprint_uses_the_web_field_order() {
        let content = [ContentBlock::text("describe")];
        let encoded = serde_json::to_string(&FingerprintInput {
            prompt: "describe",
            content: fingerprint_content(&content),
        })
        .expect("serializes fingerprint input");

        assert_eq!(
            encoded,
            r#"{"prompt":"describe","content":[{"type":"text","text":"describe"}]}"#
        );
    }

    #[test]
    fn wire_validation_rejects_inline_bytes_and_pathlike_refs() {
        let inline = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            },
        };
        let pathlike = ContentBlock::image(AttachmentRef::new(
            "../outside",
            "image/png",
            5,
            "0".repeat(64),
        ));
        let valid = ContentBlock::image(AttachmentRef::new(
            "upload-1",
            "image/png",
            5,
            "0".repeat(64),
        ));

        assert!(!inline.is_wire_safe());
        assert!(!pathlike.is_wire_safe());
        assert!(valid.is_wire_safe());
    }
}
