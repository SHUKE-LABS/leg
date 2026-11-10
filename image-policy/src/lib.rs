//! Shared image limits and signature validation for the core and companion hosts.

/// Per-image limit, measured as the provider-facing base64 encoding.
pub const MAX_IMAGE_ENCODED_BYTES: u64 = 10_000_000;
/// Combined image limit for one turn, measured as base64 encodings.
pub const MAX_TURN_IMAGE_ENCODED_BYTES: u64 = 32_000_000;

/// Length of the standard padded base64 encoding of `byte_len` bytes.
pub fn base64_encoded_len(byte_len: u64) -> u64 {
    byte_len.div_ceil(3).saturating_mul(4)
}

/// Detects a supported image MIME type from its leading magic bytes.
pub fn detect_image_media_type(bytes: &[u8]) -> Option<&'static str> {
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
