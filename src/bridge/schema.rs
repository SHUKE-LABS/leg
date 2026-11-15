//! The exact schemas of the Photos and Files capabilities.
//!
//! The early capabilities name a flat table of fields; the Photos and Files
//! capabilities that LegHost serves have nested, exact inputs and results.
//! Every field here is checked against its bound before the request reaches
//! the host, and every result the host returns is checked against its schema
//! before it reaches the caller: a field the schema does not name is refused
//! at any depth.
//!
//! The host checks the same rules again (`ios/LegHost/BridgeListener.swift`),
//! with one addition the guest cannot make without Unicode tables: a file
//! name must already be NFC.

use serde_json::{Map, Value};

use super::protocol::BridgeError;

/// The largest file a transfer may carry, in bytes.
pub const MAX_TRANSFER_BYTES: u64 = 268_435_456;

/// The page size `photos.search` uses when the input names none.
pub const DEFAULT_SEARCH_LIMIT: u64 = 20;

/// The largest page `photos.search` returns.
pub const MAX_SEARCH_LIMIT: u64 = 100;

/// The longest photo asset id, in bytes.
pub const MAX_ASSET_ID_BYTES: usize = 128;

/// The longest copy or export id the host generates, in bytes.
pub const MAX_GENERATED_ID_BYTES: usize = 64;

/// The longest date-time, in bytes.
pub const MAX_DATE_BYTES: usize = 64;

/// The longest content type, in bytes.
pub const MAX_CONTENT_TYPE_BYTES: usize = 128;

/// The largest width or height, in pixels.
pub const MAX_DIMENSION: u64 = 32_768;

/// The longest video duration, in milliseconds: seven days.
pub const MAX_DURATION_MS: u64 = 604_800_000;

/// The longest file name, in bytes.
pub const MAX_BASENAME_BYTES: usize = 128;

/// The longest `copy_id/name` path, in bytes.
pub const MAX_MANAGED_PATH_BYTES: usize = 256;

/// How a capability's input is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSchema {
    /// The flat field table of the capability.
    Fields,
    /// `photos.search`.
    PhotosSearch,
    /// `photos.export`.
    PhotosExport,
    /// `files.import`.
    FilesImport,
    /// `files.export`.
    FilesExport,
}

/// Checks the input of a capability with an exact schema.
pub fn validate_input(schema: InputSchema, input: &Value) -> Result<(), BridgeError> {
    match schema {
        InputSchema::Fields => Ok(()),
        InputSchema::PhotosSearch => {
            let fields = members(
                input,
                &["filter", "sort", "limit"],
                &["filter", "sort"],
                "input",
            )?;
            let filter = members(
                &fields["filter"],
                &["media_type", "created_after", "created_before"],
                &["media_type"],
                "filter",
            )?;
            if filter["media_type"].as_str() != Some("video") {
                return Err(invalid("filter.media_type must be \"video\""));
            }
            let after = filter
                .get("created_after")
                .map(|value| date(value, "filter.created_after"))
                .transpose()?;
            let before = filter
                .get("created_before")
                .map(|value| date(value, "filter.created_before"))
                .transpose()?;
            if let (Some(after), Some(before)) = (after, before)
                && after > before
            {
                return Err(invalid(
                    "filter.created_after is later than filter.created_before",
                ));
            }
            if !matches!(
                fields["sort"].as_str(),
                Some("duration_desc" | "created_desc")
            ) {
                return Err(invalid(
                    "sort must be \"duration_desc\" or \"created_desc\"",
                ));
            }
            if let Some(limit) = fields.get("limit")
                && !limit
                    .as_u64()
                    .is_some_and(|limit| (1..=MAX_SEARCH_LIMIT).contains(&limit))
            {
                return Err(invalid(&format!(
                    "limit must be an integer from 1 to {MAX_SEARCH_LIMIT}"
                )));
            }
            Ok(())
        }
        InputSchema::PhotosExport => {
            let fields = members(
                input,
                &["asset_id", "destination", "filename"],
                &["asset_id", "destination"],
                "input",
            )?;
            if !fields["asset_id"]
                .as_str()
                .is_some_and(|id| is_printable_id(id, MAX_ASSET_ID_BYTES))
            {
                return Err(invalid(&format!(
                    "asset_id must be printable text of 1 to {MAX_ASSET_ID_BYTES} bytes"
                )));
            }
            if !matches!(
                fields["destination"].as_str(),
                Some("guest_workspace" | "files_picker")
            ) {
                return Err(invalid(
                    "destination must be \"guest_workspace\" or \"files_picker\"",
                ));
            }
            filename(fields)
        }
        InputSchema::FilesImport => {
            let fields = members(input, &["destination"], &["destination"], "input")?;
            if fields["destination"].as_str() != Some("guest_workspace") {
                return Err(invalid("destination must be \"guest_workspace\""));
            }
            Ok(())
        }
        InputSchema::FilesExport => {
            let fields = members(
                input,
                &["copy_id", "relative_path", "destination", "filename"],
                &["copy_id", "relative_path", "destination"],
                "input",
            )?;
            let Some(copy_id) = fields["copy_id"].as_str().filter(|id| is_generated_id(id)) else {
                return Err(invalid("copy_id is not an id the host issued"));
            };
            let Some(path) = fields["relative_path"].as_str() else {
                return Err(invalid("relative_path must be a string"));
            };
            if managed_name(path, copy_id).is_none() {
                return Err(BridgeError::new(
                    "invalid_path",
                    "relative_path must be copy_id/name, as the import returned it",
                    "send the relative_path the import returned",
                ));
            }
            if fields["destination"].as_str() != Some("files_picker") {
                return Err(invalid("destination must be \"files_picker\""));
            }
            filename(fields)
        }
    }
}

/// Checks the result the host returned for a capability with an exact
/// schema, against the input the request carried.
pub fn validate_result(schema: InputSchema, input: &Value, result: &Value) -> Result<(), String> {
    match schema {
        InputSchema::Fields => Ok(()),
        InputSchema::PhotosSearch => {
            let fields = exact(result, &["authorization", "assets", "truncated"], "result")?;
            authorization(fields)?;
            let limit = input
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_SEARCH_LIMIT);
            let Some(assets) = fields["assets"].as_array() else {
                return Err("assets is not an array".to_string());
            };
            let Some(truncated) = fields["truncated"].as_bool() else {
                return Err("truncated is not a boolean".to_string());
            };
            if assets.len() as u64 > limit {
                return Err(format!("the result holds more than {limit} assets"));
            }
            // A page is truncated only when a match was left out of it, and
            // a match is left out only of a full page.
            if truncated && (assets.len() as u64) < limit {
                return Err("a page that is not full is truncated".to_string());
            }
            let filter = &input["filter"];
            let after = filter
                .get("created_after")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339);
            let before = filter
                .get("created_before")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339);
            let sort = input["sort"].as_str().unwrap_or_default();
            let mut previous: Option<(u64, (i64, u32), &str)> = None;
            for asset in assets {
                let asset = exact(
                    asset,
                    &[
                        "asset_id",
                        "media_type",
                        "duration_ms",
                        "created_at",
                        "width",
                        "height",
                    ],
                    "asset",
                )?;
                let Some(id) = asset["asset_id"]
                    .as_str()
                    .filter(|id| is_printable_id(id, MAX_ASSET_ID_BYTES))
                else {
                    return Err("an asset id is not a printable id".to_string());
                };
                if asset["media_type"].as_str() != Some("video") {
                    return Err("an asset is not a video".to_string());
                }
                let Some(duration) = asset["duration_ms"]
                    .as_u64()
                    .filter(|duration| *duration <= MAX_DURATION_MS)
                else {
                    return Err("an asset duration is out of range".to_string());
                };
                let Some(created) = asset["created_at"].as_str().and_then(parse_rfc3339) else {
                    return Err("an asset date is not an RFC 3339 date-time".to_string());
                };
                for side in ["width", "height"] {
                    if !asset[side]
                        .as_u64()
                        .is_some_and(|size| (1..=MAX_DIMENSION).contains(&size))
                    {
                        return Err(format!("an asset {side} is out of range"));
                    }
                }
                if after.is_some_and(|after| created < after)
                    || before.is_some_and(|before| created > before)
                {
                    return Err("an asset is outside the date filter".to_string());
                }
                if let Some((last_duration, last_created, last_id)) = previous {
                    let order = if sort == "duration_desc" {
                        last_duration.cmp(&duration).reverse()
                    } else {
                        last_created.cmp(&created).reverse()
                    };
                    let in_order = match order {
                        std::cmp::Ordering::Less => true,
                        std::cmp::Ordering::Equal => last_id < id,
                        std::cmp::Ordering::Greater => false,
                    };
                    if !in_order {
                        return Err("the assets are not in the requested order".to_string());
                    }
                }
                previous = Some((duration, created, id));
            }
            Ok(())
        }
        InputSchema::PhotosExport => {
            let fields = exact(
                result,
                &[
                    "authorization",
                    "copy_id",
                    "relative_path",
                    "name",
                    "media_type",
                    "size_bytes",
                    "sha256",
                ],
                "result",
            )?;
            authorization(fields)?;
            let (copy_id, name) = copy(fields, "copy_id")?;
            if fields["media_type"].as_str() != Some("video") {
                return Err("media_type is not video".to_string());
            }
            transfer(fields)?;
            match (input["destination"].as_str(), &fields["relative_path"]) {
                (Some("files_picker"), Value::Null) => Ok(()),
                (Some("guest_workspace"), Value::String(path))
                    if managed_name(path, copy_id) == Some(name) =>
                {
                    Ok(())
                }
                _ => Err("relative_path does not match the destination".to_string()),
            }
        }
        InputSchema::FilesImport => {
            let fields = exact(
                result,
                &[
                    "copy_id",
                    "relative_path",
                    "name",
                    "content_type",
                    "size_bytes",
                    "sha256",
                ],
                "result",
            )?;
            let (copy_id, name) = copy(fields, "copy_id")?;
            if fields["relative_path"]
                .as_str()
                .and_then(|path| managed_name(path, copy_id))
                != Some(name)
            {
                return Err("relative_path is not copy_id/name".to_string());
            }
            content_type(fields)?;
            transfer(fields)
        }
        InputSchema::FilesExport => {
            let fields = exact(
                result,
                &["export_id", "name", "content_type", "size_bytes", "sha256"],
                "result",
            )?;
            copy(fields, "export_id")?;
            content_type(fields)?;
            transfer(fields)
        }
    }
}

/// The members of an input object, after the names are checked.
fn members<'a>(
    value: &'a Value,
    allowed: &[&str],
    required: &[&str],
    name: &str,
) -> Result<&'a Map<String, Value>, BridgeError> {
    let Some(fields) = value.as_object() else {
        return Err(invalid(&format!("{name} must be an object")));
    };
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(BridgeError::new(
                "unknown_field",
                format!(
                    "{name} carries the unknown field `{}`",
                    key.chars().take(64).collect::<String>()
                ),
                format!("{name} accepts {}", allowed.join(", ")),
            ));
        }
    }
    for key in required {
        if !fields.contains_key(*key) {
            return Err(BridgeError::new(
                "missing_field",
                format!("{name} needs the field `{key}`"),
                format!("add `{key}` to {name}"),
            ));
        }
    }
    Ok(fields)
}

/// The members of a result object that names exactly `names`.
fn exact<'a>(
    value: &'a Value,
    names: &[&str],
    name: &str,
) -> Result<&'a Map<String, Value>, String> {
    let Some(fields) = value.as_object() else {
        return Err(format!("the {name} is not an object"));
    };
    if fields.len() != names.len() || !names.iter().all(|key| fields.contains_key(*key)) {
        return Err(format!(
            "the {name} does not carry exactly {}",
            names.join(", ")
        ));
    }
    Ok(fields)
}

fn invalid(message: &str) -> BridgeError {
    BridgeError::new(
        "invalid_input",
        message,
        "send the input the capability schema names",
    )
}

fn date(value: &Value, name: &str) -> Result<(i64, u32), BridgeError> {
    value
        .as_str()
        .and_then(parse_rfc3339)
        .ok_or_else(|| invalid(&format!("{name} must be an RFC 3339 date-time")))
}

fn filename(fields: &Map<String, Value>) -> Result<(), BridgeError> {
    match fields.get("filename") {
        None => Ok(()),
        Some(value) if value.as_str().is_some_and(is_safe_basename) => Ok(()),
        Some(_) => Err(BridgeError::new(
            "invalid_path",
            "filename is not a safe file name",
            "send a plain file name without separators, control characters, or leading dots",
        )),
    }
}

fn authorization(fields: &Map<String, Value>) -> Result<(), String> {
    match fields["authorization"].as_str() {
        Some("full" | "limited") => Ok(()),
        _ => Err("authorization is not full or limited".to_string()),
    }
}

/// The generated id under `id_field` and the safe name of a result.
fn copy<'a>(fields: &'a Map<String, Value>, id_field: &str) -> Result<(&'a str, &'a str), String> {
    let Some(id) = fields[id_field].as_str().filter(|id| is_generated_id(id)) else {
        return Err(format!("{id_field} is not a generated id"));
    };
    let Some(name) = fields["name"]
        .as_str()
        .filter(|name| is_safe_basename(name))
    else {
        return Err("name is not a safe file name".to_string());
    };
    Ok((id, name))
}

fn content_type(fields: &Map<String, Value>) -> Result<(), String> {
    if fields["content_type"].as_str().is_some_and(is_content_type) {
        Ok(())
    } else {
        Err("content_type is not a lowercase type/subtype".to_string())
    }
}

fn transfer(fields: &Map<String, Value>) -> Result<(), String> {
    if fields["size_bytes"]
        .as_u64()
        .is_none_or(|size| size > MAX_TRANSFER_BYTES)
    {
        return Err("size_bytes is out of range".to_string());
    }
    if !fields["sha256"].as_str().is_some_and(is_checksum) {
        return Err("sha256 is not a lowercase hex digest".to_string());
    }
    Ok(())
}

/// Whether `text` is 1 to `max_bytes` bytes without a control character.
pub fn is_printable_id(text: &str, max_bytes: usize) -> bool {
    (1..=max_bytes).contains(&text.len()) && !text.chars().any(char::is_control)
}

/// Whether `text` has the shape of an id the host generates:
/// `[a-z0-9][a-z0-9-]{0,63}`.
pub fn is_generated_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=MAX_GENERATED_ID_BYTES).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Whether `name` is a file name the host writes as is.
///
/// A name that would need a change to be safe is refused, not changed: 1 to
/// 128 bytes, without a separator, a colon, or a control character, without
/// a leading or trailing space or dot, and not `.` or `..`.
pub fn is_safe_basename(name: &str) -> bool {
    if !(1..=MAX_BASENAME_BYTES).contains(&name.len()) || name == "." || name == ".." {
        return false;
    }
    if name.chars().any(|character| {
        matches!(character, '/' | '\\' | ':' | '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}')
    }) {
        return false;
    }
    let edge = |byte: u8| byte == b' ' || byte == b'.';
    let bytes = name.as_bytes();
    !edge(bytes[0]) && !edge(bytes[bytes.len() - 1])
}

/// The name in `path`, when the path is exactly `copy_id/name` with a safe
/// name and fits the path bound.
pub fn managed_name<'a>(path: &'a str, copy_id: &str) -> Option<&'a str> {
    if path.len() > MAX_MANAGED_PATH_BYTES {
        return None;
    }
    let name = path.strip_prefix(copy_id)?.strip_prefix('/')?;
    is_safe_basename(name).then_some(name)
}

/// Whether `text` is a lowercase MIME `type/subtype` without parameters.
pub fn is_content_type(text: &str) -> bool {
    if !(3..=MAX_CONTENT_TYPE_BYTES).contains(&text.len()) {
        return false;
    }
    let Some((kind, subtype)) = text.split_once('/') else {
        return false;
    };
    let token = |part: &str| {
        part.as_bytes()
            .first()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"!#$&-^_.+".contains(&byte)
            })
    };
    token(kind) && token(subtype)
}

/// Whether `text` is a SHA-256 digest: 64 lowercase hex digits.
pub fn is_checksum(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Reads a strict RFC 3339 date-time of 1 to 64 bytes:
/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
///
/// The answer is the instant as seconds since the Unix epoch and the
/// nanoseconds within that second, so two dates compare as instants.
pub fn parse_rfc3339(text: &str) -> Option<(i64, u32)> {
    let bytes = text.as_bytes();
    if !(20..=MAX_DATE_BYTES).contains(&bytes.len()) {
        return None;
    }
    let mut index = 0;
    let number = |index: &mut usize, width: usize| -> Option<i64> {
        let digits = bytes.get(*index..*index + width)?;
        let mut value = 0i64;
        for byte in digits {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        *index += width;
        Some(value)
    };
    let expect = |index: &mut usize, allowed: &[u8]| -> Option<()> {
        let byte = bytes.get(*index)?;
        if !allowed.contains(byte) {
            return None;
        }
        *index += 1;
        Some(())
    };
    let year = number(&mut index, 4)?;
    expect(&mut index, b"-")?;
    let month = number(&mut index, 2)?;
    expect(&mut index, b"-")?;
    let day = number(&mut index, 2)?;
    expect(&mut index, b"Tt")?;
    let hour = number(&mut index, 2)?;
    expect(&mut index, b":")?;
    let minute = number(&mut index, 2)?;
    expect(&mut index, b":")?;
    let second = number(&mut index, 2)?;
    let mut nanos = 0u32;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let digits = index - start;
        if !(1..=9).contains(&digits) {
            return None;
        }
        for byte in &bytes[start..index] {
            nanos = nanos * 10 + u32::from(byte - b'0');
        }
        nanos *= 10u32.pow((9 - digits) as u32);
    }
    let offset = match bytes.get(index)? {
        b'Z' | b'z' => {
            index += 1;
            0
        }
        sign @ (b'+' | b'-') => {
            let sign = if *sign == b'+' { 1 } else { -1 };
            index += 1;
            let hours = number(&mut index, 2)?;
            expect(&mut index, b":")?;
            let minutes = number(&mut index, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    if index != bytes.len() {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month)
        || !(1..=month_days[(month - 1) as usize]).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some((seconds, nanos))
}

/// The days from 1970-01-01 to a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_strict_rfc_3339_instants() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some((0, 0)));
        assert_eq!(
            parse_rfc3339("2024-02-29T12:00:00.5+08:00"),
            Some((1_709_179_200, 500_000_000))
        );
        assert_eq!(
            parse_rfc3339("2024-02-29t04:00:00z"),
            Some((1_709_179_200, 0))
        );
        for bad in [
            "2023-02-29T12:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:60Z",
            "2024-01-01 00:00:00Z",
            "2024-01-01T00:00:00",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00.1234567890Z",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00+0800",
            "24-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad} was accepted");
        }
        let long = format!("2024-01-01T00:00:00.{}Z", "1".repeat(50));
        assert_eq!(parse_rfc3339(&long), None);
    }

    #[test]
    fn file_names_are_refused_rather_than_normalized() {
        for good in [
            "clip.mov",
            "a",
            "Ünïcode clip.mp4",
            "x".repeat(128).as_str(),
        ] {
            assert!(is_safe_basename(good), "{good} was refused");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "trailing.",
            " lead",
            "trail ",
            "a/b",
            "a\\b",
            "a:b",
            "a\u{0}b",
            "a\u{1F}b",
            "a\u{7F}b",
            "a\u{85}b",
            "x".repeat(129).as_str(),
        ] {
            assert!(!is_safe_basename(bad), "{bad:?} was accepted");
        }
    }

    #[test]
    fn ids_content_types_and_checksums_keep_their_shapes() {
        assert!(is_generated_id("0a-b"));
        assert!(is_generated_id(&"a".repeat(64)));
        for bad in ["", "-a", "A", "a_b", &"a".repeat(65)] {
            assert!(!is_generated_id(bad), "{bad} was accepted");
        }
        assert!(is_printable_id("ABC-123/L0/001", 128));
        assert!(!is_printable_id("a\nb", 128));
        assert!(!is_printable_id(&"a".repeat(129), 128));
        assert!(is_content_type("video/quicktime"));
        assert!(is_content_type("application/vnd.apple.mpegurl"));
        for bad in ["video", "Video/mp4", "video/mp4; codecs=x", "/mp4", "a/b/c"] {
            assert!(!is_content_type(bad), "{bad} was accepted");
        }
        assert!(is_checksum(&"a".repeat(64)));
        assert!(!is_checksum(&"A".repeat(64)));
        assert!(!is_checksum(&"a".repeat(63)));
        assert_eq!(managed_name("abc/clip.mov", "abc"), Some("clip.mov"));
        assert_eq!(managed_name("abc/../x", "abc"), None);
        assert_eq!(managed_name("abc/d/clip.mov", "abc"), None);
        assert_eq!(managed_name("abcd/clip.mov", "abc"), None);
        assert_eq!(managed_name("./abc/clip.mov", "abc"), None);
    }
}
