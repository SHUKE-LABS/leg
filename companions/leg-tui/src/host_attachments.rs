//! The receiver for images a native host (the iOS `LegHost`) hands to the
//! composer.
//!
//! The host validates and stages each image in its own session directory,
//! publishes it into one guest directory scoped to the running host session,
//! and then writes a bracketed paste whose text is a notice: a fixed prefix
//! plus JSON carrying only the opaque handoff id, the session generation, the
//! source marker, a basename label, MIME, byte length, and SHA-256. Image
//! bytes and host paths never cross the terminal. The receiver resolves the
//! id inside its own generation directory only, verifies the bytes, hands
//! them to the catalog's validated staging, and removes the handoff file.
//!
//! The receiver is active only when the host names the directory in
//! `LEG_HOST_ATTACHMENT_DIR`; otherwise every paste stays text. Its ready
//! marker lives as long as the inbox, so the host stops handing images over
//! once this program is gone.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use leg_ui_client::{MAX_IMAGE_ENCODED_BYTES, attachment_label, base64_encoded_len};

use crate::attachments::AcquiredImage;

/// The guest directory the host publishes images into for this session.
pub(crate) const DIRECTORY_ENV: &str = "LEG_HOST_ATTACHMENT_DIR";
/// Written into the directory so the host knows this program reads notices.
pub(crate) const READY_MARKER: &str = ".receiver-v1";
/// The paste text that marks a host attachment notice.
pub(crate) const NOTICE_PREFIX: &str = "leghost-attachment/v1 ";
const MAX_NOTICE_BYTES: usize = 2048;
const MAX_IMAGE_BYTES: u64 = MAX_IMAGE_ENCODED_BYTES / 4 * 3;

/// One image the host announced. Every field is opaque metadata.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HostNotice {
    pub generation: String,
    pub id: String,
    pub source: &'static str,
    pub label: String,
    pub media_type: String,
    pub byte_len: u64,
    pub sha256: String,
}

/// The directory of the host session this program runs in.
pub(crate) struct HostAttachmentInbox {
    directory: PathBuf,
    generation: String,
}

impl HostAttachmentInbox {
    /// Opens the directory the host named and announces the receiver in it.
    /// Without the variable, or without a usable directory, there is no
    /// receiver and the host keeps its images.
    pub(crate) fn from_env() -> Option<Self> {
        let directory = PathBuf::from(std::env::var_os(DIRECTORY_ENV)?);
        Self::open(directory).ok()
    }

    pub(crate) fn open(directory: PathBuf) -> io::Result<Self> {
        let generation = directory
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| safe_token(name))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsafe generation"))?
            .to_string();
        if !fs::symlink_metadata(&directory)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a directory",
            ));
        }
        fs::write(directory.join(READY_MARKER), b"1\n")?;
        Ok(Self {
            directory,
            generation,
        })
    }

    /// Reads the announced image and removes its handoff file, whatever the
    /// outcome: a handoff is taken once. Errors name only the label.
    pub(crate) fn take(&self, notice: &HostNotice) -> Result<AcquiredImage, String> {
        let label = notice_label(notice);
        if notice.generation != self.generation {
            return Err(format!(
                "{label}: belongs to an earlier host session; attach it again"
            ));
        }
        let path = self.directory.join(&notice.id);
        let read = read_handoff(&path, notice.byte_len);
        let _ = fs::remove_file(&path);
        let bytes = read.map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => {
                format!("{label}: is no longer available; attach it again")
            }
            _ => format!("{label}: could not be read from the host"),
        })?;
        Ok(AcquiredImage {
            bytes,
            declared_media_type: declared_media_type(&notice.media_type),
            label,
            expected_sha256: Some(notice.sha256.clone()),
        })
    }

    /// Removes the handoff file of a notice that cannot be attached now.
    pub(crate) fn discard(&self, notice: &HostNotice) {
        if notice.generation == self.generation {
            let _ = fs::remove_file(self.directory.join(&notice.id));
        }
    }
}

impl Drop for HostAttachmentInbox {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join(READY_MARKER));
    }
}

/// Parses a paste as a host notice. `None` means ordinary text; a malformed
/// notice is an error, so its text never lands in the prompt.
pub(crate) fn parse_notice(pasted: &str) -> Option<Result<HostNotice, String>> {
    let json = pasted.strip_prefix(NOTICE_PREFIX)?;
    let invalid = || Err("The host sent an image notice this program cannot read".to_string());
    if pasted.len() > MAX_NOTICE_BYTES {
        return Some(invalid());
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Some(invalid());
    };
    let text = |key: &str| fields.get(key).and_then(serde_json::Value::as_str);
    let source = match text("source") {
        Some("photos") => "photos",
        Some("files") => "files",
        Some("pasteboard") => "pasteboard",
        _ => return Some(invalid()),
    };
    let (Some(generation), Some(id), Some(label), Some(media_type), Some(byte_len), Some(sha256)) = (
        text("generation"),
        text("id"),
        text("label"),
        text("media_type"),
        fields.get("byte_len").and_then(serde_json::Value::as_u64),
        text("sha256"),
    ) else {
        return Some(invalid());
    };
    if !safe_token(generation)
        || !safe_token(id)
        || declared_media_type(media_type).is_none()
        || byte_len == 0
        || base64_encoded_len(byte_len) > MAX_IMAGE_ENCODED_BYTES
        || sha256.len() != 64
        || !sha256.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return Some(invalid());
    }
    Some(Ok(HostNotice {
        generation: generation.to_string(),
        id: id.to_string(),
        source,
        label: attachment_label(label),
        media_type: media_type.to_string(),
        byte_len,
        sha256: sha256.to_ascii_lowercase(),
    }))
}

/// The draft label: the source marker and the basename.
fn notice_label(notice: &HostNotice) -> String {
    let marker = match notice.source {
        "photos" => "Photos",
        "files" => "Files",
        _ => "Clipboard",
    };
    attachment_label(&format!("{marker} · {}", notice.label))
}

fn declared_media_type(value: &str) -> Option<&'static str> {
    match value {
        "image/jpeg" => Some("image/jpeg"),
        "image/png" => Some("image/png"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// An opaque id or generation: no path separators, no dot names.
fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('.')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

/// Reads exactly `byte_len` bytes from a regular file that is not a link.
fn read_handoff(path: &Path, byte_len: u64) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() != byte_len || byte_len > MAX_IMAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected file",
        ));
    }
    let mut bytes = Vec::with_capacity(byte_len as usize);
    File::open(path)?
        .take(byte_len + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != byte_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "length changed"));
    }
    Ok(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn notice_text(generation: &str, id: &str, bytes: &[u8], sha256: &str) -> String {
        format!(
            "{NOTICE_PREFIX}{}",
            serde_json::json!({
                "generation": generation,
                "id": id,
                "source": "photos",
                "label": "IMG_0001.png",
                "media_type": "image/png",
                "byte_len": bytes.len(),
                "sha256": sha256,
            })
        )
    }

    /// The notice as the iOS host writes it, byte for byte; LegHost asserts
    /// the same text in `testTheNoticeMatchesTheReceiverContract`.
    #[test]
    fn the_ios_host_notice_parses() {
        let golden = r#"leghost-attachment/v1 {"byte_len":70,"generation":"0123456789abcdef","id":"ios-00112233445566778899aabbccddeeff","label":"IMG_0001.png","media_type":"image/png","sha256":"497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581","source":"photos"}"#;
        assert_eq!(
            parse_notice(golden),
            Some(Ok(HostNotice {
                generation: "0123456789abcdef".to_string(),
                id: "ios-00112233445566778899aabbccddeeff".to_string(),
                source: "photos",
                label: "IMG_0001.png".to_string(),
                media_type: "image/png".to_string(),
                byte_len: 70,
                sha256: "497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581"
                    .to_string(),
            }))
        );
    }

    /// A label the host escaped so that no shell reads it as code.
    #[test]
    fn an_escaped_ios_label_parses_to_the_filename() {
        let golden = r#"leghost-attachment/v1 {"byte_len":70,"generation":"0123456789abcdef","id":"ios-00112233445566778899aabbccddeeff","label":"\u0024\u0028id\u0029\u0020\u0060x\u0060\u003b\u732b.png","media_type":"image/png","sha256":"497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581","source":"files"}"#;
        let notice = parse_notice(golden).unwrap().unwrap();
        assert_eq!(notice.label, "$(id) `x`;猫.png");
        assert_eq!(notice_label(&notice), "Files · $(id) `x`;猫.png");
    }

    #[test]
    fn ordinary_text_is_not_a_notice() {
        assert_eq!(parse_notice("hello"), None);
        assert_eq!(parse_notice(" leghost-attachment/v1 {}"), None);
    }

    #[test]
    fn notices_are_bounded_and_validated() {
        let sha = "a".repeat(64);
        let notice = parse_notice(&notice_text("g1", "ios-1", b"12345", &sha))
            .unwrap()
            .unwrap();
        assert_eq!(notice.generation, "g1");
        assert_eq!(notice.id, "ios-1");
        assert_eq!(notice.source, "photos");
        assert_eq!(notice.byte_len, 5);
        assert_eq!(
            notice_label(&notice),
            "Photos · IMG_0001.png",
            "the label is the source marker and a basename"
        );

        for bad in [
            notice_text("g1", "../escape", b"1", &sha),
            notice_text("../g1", "ios-1", b"1", &sha),
            notice_text("g1", ".receiver-v1", b"1", &sha),
            notice_text("g1", "ios-1", b"", &sha),
            notice_text("g1", "ios-1", b"1", "zz"),
            format!("{NOTICE_PREFIX}not json"),
            format!("{NOTICE_PREFIX}{}", "x".repeat(MAX_NOTICE_BYTES)),
        ] {
            assert!(matches!(parse_notice(&bad), Some(Err(_))), "{bad}");
        }
        let unknown_source = notice_text("g1", "ios-1", b"1", &sha).replace("photos", "camera");
        assert!(matches!(parse_notice(&unknown_source), Some(Err(_))));
        let path_label = notice_text("g1", "ios-1", b"1", &sha)
            .replace("IMG_0001.png", "/private/var/mobile/IMG_0001.png");
        assert_eq!(
            parse_notice(&path_label).unwrap().unwrap().label,
            "IMG_0001.png"
        );
    }

    #[test]
    fn the_inbox_announces_itself_and_rejects_unsafe_directories() {
        let root = std::env::temp_dir().join(format!(
            "leg-tui-host-inbox-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = root.join("gen-1");
        fs::create_dir_all(&directory).unwrap();
        let inbox = HostAttachmentInbox::open(directory.clone()).unwrap();
        assert_eq!(inbox.generation, "gen-1");
        assert!(directory.join(READY_MARKER).is_file());
        drop(inbox);
        assert!(
            !directory.join(READY_MARKER).exists(),
            "a closed receiver no longer announces itself"
        );
        assert!(HostAttachmentInbox::open(root.join("missing")).is_err());
        assert!(HostAttachmentInbox::open(root.join(".hidden")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
