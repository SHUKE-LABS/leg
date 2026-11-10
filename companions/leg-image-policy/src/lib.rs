//! Shared image limits and signature validation for the native and Web hosts.

pub const MAX_BASE64_IMAGE_BYTES: usize = 10_000_000;
pub const MAX_IMAGE_REQUEST_BYTES: usize = 32_000_000;
pub const MAX_RAW_IMAGE_BYTES: usize = MAX_BASE64_IMAGE_BYTES / 4 * 3;

pub const SUPPORTED_MEDIA_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

pub fn normalize_media_type(value: &str) -> &str {
    value.split(';').next().unwrap_or(value).trim()
}

pub fn is_supported_media_type(value: &str) -> bool {
    SUPPORTED_MEDIA_TYPES.contains(&value)
}

pub fn encoded_len(raw_len: usize) -> usize {
    raw_len.div_ceil(3) * 4
}

pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
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

#[cfg(test)]
mod tests {
    use super::detect_media_type;

    #[test]
    fn detects_supported_image_signatures() {
        for (bytes, media_type) in [
            (&b"\xff\xd8\xff"[..], "image/jpeg"),
            (&b"\x89PNG\r\n\x1a\n"[..], "image/png"),
            (&b"GIF87a"[..], "image/gif"),
            (&b"GIF89a"[..], "image/gif"),
            (&b"RIFFxxxxWEBP"[..], "image/webp"),
        ] {
            assert_eq!(detect_media_type(bytes), Some(media_type));
        }
        assert_eq!(detect_media_type(b"not an image"), None);
    }
}
