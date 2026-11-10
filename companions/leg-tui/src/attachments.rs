//! Draft image acquisition for the composer: local image files and the host
//! clipboard. Bytes are only handed to the catalog's validated private
//! staging; status text names a basename or the clipboard marker, never a
//! path.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use leg_ui_client::{
    AttachmentError, CLIPBOARD_IMAGE_LABEL, MAX_IMAGE_ENCODED_BYTES, attachment_label,
    base64_encoded_len,
};

/// Composer text holds one Supplementary Private Use Area-A character per
/// draft image, so images keep their position between text segments.
const MARKER_FIRST: u32 = 0xF0000;
const MARKER_LAST: u32 = 0xFFFFD;
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest raw image whose base64 encoding fits the per-image limit.
const MAX_IMAGE_BYTES: u64 = MAX_IMAGE_ENCODED_BYTES / 4 * 3;
pub(crate) const IMAGE_FALLBACK: &str = "Attach a local file instead, or use Web/iOS.";

pub(crate) fn is_image_marker(character: char) -> bool {
    (MARKER_FIRST..=MARKER_LAST).contains(&(character as u32))
}

pub(crate) fn image_marker(sequence: u32) -> Option<char> {
    MARKER_FIRST
        .checked_add(sequence)
        .filter(|value| *value <= MARKER_LAST)
        .and_then(char::from_u32)
}

/// Decimal size label matching the documented MB limits.
pub(crate) fn format_byte_size(bytes: u64) -> String {
    if bytes < 1_000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

/// Image bytes read for staging, with the MIME implied by the file name.
pub(crate) struct AcquiredImage {
    pub bytes: Vec<u8>,
    pub declared_media_type: Option<&'static str>,
    pub label: String,
}

/// Reads one local image file with a bounded read. The size limit is checked
/// before reading; MIME and magic validation happen in the catalog.
pub(crate) fn read_image_file(path: &Path) -> Result<AcquiredImage, String> {
    let label = attachment_label(&path.to_string_lossy());
    let unreadable = |error: io::Error| match error.kind() {
        io::ErrorKind::NotFound => format!("{label}: file not found"),
        io::ErrorKind::PermissionDenied => format!("{label}: permission denied"),
        _ => format!("{label}: could not read the file"),
    };
    let mut file = File::open(path).map_err(unreadable)?;
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(format!("{label}: not a regular file"));
    }
    if base64_encoded_len(metadata.len()) > MAX_IMAGE_ENCODED_BYTES {
        return Err(AttachmentError::TooLarge { label }.to_string());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(AcquiredImage {
        bytes,
        declared_media_type: declared_media_type(path),
        label,
    })
}

fn declared_media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) enum ClipboardError {
    Unavailable(String),
    Empty,
    Cancelled,
}

/// Host clipboard image reader; tests inject a fixed source.
pub(crate) trait ClipboardImageSource {
    /// Returns encoded image bytes; their MIME comes from magic validation.
    fn read_image(&mut self) -> Result<Vec<u8>, ClipboardError>;
}

/// Converts a clipboard read into staging input or a bounded status line.
pub(crate) fn clipboard_image(
    source: &mut dyn ClipboardImageSource,
) -> Result<AcquiredImage, String> {
    let bytes = match source.read_image() {
        Ok(bytes) => bytes,
        Err(ClipboardError::Cancelled) => return Err("Clipboard image paste cancelled".into()),
        Err(ClipboardError::Empty) => {
            return Err(format!("The clipboard has no image. {IMAGE_FALLBACK}"));
        }
        Err(ClipboardError::Unavailable(reason)) => {
            return Err(format!(
                "Clipboard images are unavailable ({reason}). {IMAGE_FALLBACK}"
            ));
        }
    };
    Ok(AcquiredImage {
        bytes,
        declared_media_type: None,
        label: CLIPBOARD_IMAGE_LABEL.to_string(),
    })
}

/// Reads a PNG from the host clipboard through the platform's own tool:
/// PowerShell on Windows, osascript on macOS, and wl-paste or xclip on Linux.
/// Any other host, a missing tool, or a timeout fails closed.
pub(crate) struct NativeClipboard;

impl ClipboardImageSource for NativeClipboard {
    fn read_image(&mut self) -> Result<Vec<u8>, ClipboardError> {
        let mut command = native_clipboard_command()?;
        let output = run_bounded(&mut command)?;
        if cfg!(target_os = "macos") {
            return decode_osascript_png(&output);
        }
        if output.is_empty() {
            return Err(ClipboardError::Empty);
        }
        Ok(output)
    }
}

fn native_clipboard_command() -> Result<Command, ClipboardError> {
    if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
             $image = [System.Windows.Forms.Clipboard]::GetImage(); \
             if ($null -eq $image) { exit 3 }; \
             $buffer = New-Object System.IO.MemoryStream; \
             $image.Save($buffer, [System.Drawing.Imaging.ImageFormat]::Png); \
             $stdout = [Console]::OpenStandardOutput(); \
             $buffer.WriteTo($stdout); $stdout.Flush()",
        ]);
        Ok(command)
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command.args(["-e", "the clipboard as «class PNGf»"]);
        Ok(command)
    } else if cfg!(unix) && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut command = Command::new("wl-paste");
        command.args(["--no-newline", "--type", "image/png"]);
        Ok(command)
    } else if cfg!(unix) && std::env::var_os("DISPLAY").is_some() {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-target", "image/png", "-out"]);
        Ok(command)
    } else {
        Err(ClipboardError::Unavailable(
            "no supported clipboard on this host".into(),
        ))
    }
}

/// Runs a clipboard tool with a bounded stdout and a timeout. A tool that was
/// interrupted is a cancellation; any other non-zero exit means the
/// clipboard holds no image.
fn run_bounded(command: &mut Command) -> Result<Vec<u8>, ClipboardError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            ClipboardError::Unavailable(if error.kind() == io::ErrorKind::NotFound {
                "clipboard tool not installed".into()
            } else {
                "clipboard tool could not start".into()
            })
        })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    // osascript prints hex, so allow twice the raw image limit.
    let limit = MAX_IMAGE_BYTES * 2 + 64;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ClipboardError::Unavailable(
                    "clipboard tool timed out".into(),
                ));
            }
        }
    };
    let bytes = reader
        .join()
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| ClipboardError::Unavailable("clipboard read failed".into()))?;
    if interrupted(&status) {
        return Err(ClipboardError::Cancelled);
    }
    if !status.success() {
        return Err(ClipboardError::Empty);
    }
    if bytes.len() as u64 > limit {
        return Err(ClipboardError::Unavailable(
            "clipboard image exceeds the 10 MB per-image limit".into(),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn interrupted(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal().is_some()
}

#[cfg(windows)]
fn interrupted(status: &std::process::ExitStatus) -> bool {
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
    status.code() == Some(STATUS_CONTROL_C_EXIT as i32)
}

#[cfg(not(any(unix, windows)))]
fn interrupted(_status: &std::process::ExitStatus) -> bool {
    false
}

/// Decodes osascript's `«data PNGf<hex>»` rendering of clipboard PNG data.
fn decode_osascript_png(output: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let text = std::str::from_utf8(output).map_err(|_| ClipboardError::Empty)?;
    let hex = text
        .trim()
        .strip_prefix("«data PNGf")
        .and_then(|rest| rest.strip_suffix('»'))
        .ok_or(ClipboardError::Empty)?;
    if hex.len() % 2 != 0 {
        return Err(ClipboardError::Empty);
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ClipboardError::Empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osascript_png_data_is_decoded() {
        assert_eq!(
            decode_osascript_png("«data PNGf89504E47»\n".as_bytes()).unwrap(),
            [0x89, 0x50, 0x4E, 0x47]
        );
        assert!(decode_osascript_png(b"missing value").is_err());
    }

    #[test]
    fn markers_and_sizes_are_bounded() {
        assert!(is_image_marker(image_marker(0).unwrap()));
        assert_eq!(image_marker(MARKER_LAST - MARKER_FIRST + 1), None);
        assert!(!is_image_marker('a'));
        assert_eq!(format_byte_size(999), "999 B");
        assert_eq!(format_byte_size(12_345), "12.3 KB");
        assert_eq!(format_byte_size(7_500_000), "7.5 MB");
    }
}
