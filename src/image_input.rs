use std::fs::File;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::error::{LegError, Result};
use crate::model::{ContentBlock, ImageSource, Message};

pub(crate) const MAX_BASE64_IMAGE_BYTES: usize = 10_000_000;
pub(crate) const MAX_IMAGE_REQUEST_BYTES: usize = 32_000_000;
const MAX_RAW_IMAGE_BYTES: usize = MAX_BASE64_IMAGE_BYTES / 4 * 3;

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
        if file_size > MAX_RAW_IMAGE_BYTES as u64 {
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

        let media_type = detect_media_type(&bytes).ok_or_else(|| {
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
        if let ContentBlock::Image {
            source: ImageSource::Base64 { data, .. },
        } = block
        {
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
            assert_eq!(detect_media_type(bytes), Some(media_type));
        }
        assert_eq!(detect_media_type(b"not an image"), None);
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
}
