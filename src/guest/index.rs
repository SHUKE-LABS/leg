//! Parsing the Alpine package index that apk downloads and verifies.
//!
//! The index is the `APKINDEX` member of the `APKINDEX.tar.gz` file apk keeps
//! in its cache directory. apk verifies the archive's signature before it
//! keeps it, so the entries parsed here are the ones the install would use.

use base64::Engine as _;
use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;

/// One package entry from an Alpine repository index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// The package name.
    pub name: String,
    /// The package version.
    pub version: String,
    /// The architecture the package is built for.
    pub architecture: String,
    /// The size of the package file, in bytes.
    pub download_bytes: u64,
    /// The installed size of the package, in bytes.
    pub install_bytes: u64,
    /// The checksum the index records for the package file.
    pub checksum: String,
}

/// The name of the index member inside the archive.
const INDEX_MEMBER: &str = "APKINDEX";

/// The size of one tar header or data block.
const TAR_BLOCK: usize = 512;

/// Parses the `APKINDEX.tar.gz` bytes apk downloaded.
pub fn parse_index_archive(bytes: &[u8]) -> Result<Vec<IndexEntry>, String> {
    let mut tar = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut tar)
        .map_err(|error| format!("the repository index could not be decompressed: {error}"))?;

    let index = tar_member(&tar, INDEX_MEMBER)
        .ok_or_else(|| format!("the repository index has no {INDEX_MEMBER} member"))?;
    let text = String::from_utf8(index)
        .map_err(|_| "the repository index is not valid UTF-8".to_string())?;
    Ok(parse_index_text(&text))
}

/// Parses the text of an `APKINDEX` file into its package entries.
pub fn parse_index_text(text: &str) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    let mut fields: BTreeMap<&str, String> = BTreeMap::new();

    for line in text.lines() {
        if line.trim().is_empty() {
            if let Some(entry) = entry_from_fields(&fields) {
                entries.push(entry);
            }
            fields.clear();
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            fields.entry(key).or_insert_with(|| value.to_string());
        }
    }
    if let Some(entry) = entry_from_fields(&fields) {
        entries.push(entry);
    }
    entries
}

/// Builds an entry from the fields of one index block.
fn entry_from_fields(fields: &BTreeMap<&str, String>) -> Option<IndexEntry> {
    let name = fields.get("P")?.clone();
    let version = fields.get("V")?.clone();
    Some(IndexEntry {
        name,
        version,
        architecture: fields.get("A").cloned().unwrap_or_default(),
        download_bytes: fields
            .get("S")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        install_bytes: fields
            .get("I")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        checksum: fields
            .get("C")
            .map(|value| format_checksum(value))
            .unwrap_or_default(),
    })
}

/// Renders an index checksum as `sha1:<hex>`.
///
/// The index records the checksum as a digest name and a base64 payload, for
/// example `Q1UxMwfAt66SiGgvZAKt4Cy/RQQrc=`.
pub fn format_checksum(raw: &str) -> String {
    let (algorithm, payload) = match raw.split_at_checked(2) {
        Some(("Q1", payload)) => ("sha1", payload),
        Some(("Q2", payload)) => ("sha256", payload),
        _ => return raw.to_string(),
    };
    match base64::engine::general_purpose::STANDARD.decode(payload) {
        Ok(digest) => {
            let hex = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("{algorithm}:{hex}")
        }
        Err(_) => raw.to_string(),
    }
}

/// Extracts one member from an uncompressed tar archive.
fn tar_member(tar: &[u8], wanted: &str) -> Option<Vec<u8>> {
    let mut offset = 0usize;
    while offset + TAR_BLOCK <= tar.len() {
        let header = &tar[offset..offset + TAR_BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }

        let size = octal_field(&header[124..136])? as usize;
        let type_flag = header[156];
        let name = cstr(&header[0..100]);
        let prefix = cstr(&header[345..500]);
        let full_name = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };

        let data_start = offset + TAR_BLOCK;
        let data_end = data_start.checked_add(size)?;
        if type_flag == b'0' && full_name == wanted {
            return tar.get(data_start..data_end).map(|data| data.to_vec());
        }

        offset = data_start + size.div_ceil(TAR_BLOCK) * TAR_BLOCK;
    }
    None
}

/// Reads a NUL-terminated string field.
fn cstr(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Reads a tar numeric field, which is octal ASCII.
fn octal_field(field: &[u8]) -> Option<u64> {
    let text = cstr(field);
    let text = text.trim();
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}
