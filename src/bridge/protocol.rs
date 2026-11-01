//! The wire protocol of the capability bridge.
//!
//! A request is one line of JSON and an answer is one line of JSON. The
//! shapes are fixed by the bridge contract:
//!
//! ```json
//! {"version":1,"request_id":"r-1","capability":"photos.list","input":{}}
//! {"version":1,"request_id":"r-1","status":"ok","result":{},"error":null}
//! ```
//!
//! Everything in this module runs before a request reaches the host, so a
//! malformed, oversized, unknown, duplicated, or path-hostile request never
//! reaches a platform adapter.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// The largest request line the bridge reads, in bytes.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The largest serialized `input` object the bridge accepts, in bytes.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

/// The largest serialized result the bridge relays back, in bytes.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The largest review summary the bridge relays back, in bytes.
///
/// A host that asks for an approval has to say what it would do, and that
/// summary is shown to a human: it is bounded, so it cannot become a
/// channel of its own.
pub const MAX_REVIEW_BYTES: usize = 8 * 1024;

/// The largest answer the bridge writes.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// The request deadline the bridge applies when a request names none.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// The shortest deadline a request may ask for, in milliseconds.
pub const MIN_TIMEOUT_MS: u64 = 100;

/// The longest deadline a request may ask for, in milliseconds.
pub const MAX_TIMEOUT_MS: u64 = 30_000;

/// The longest request id the bridge accepts, in characters.
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// The longest approval token the bridge accepts, in characters.
pub const MAX_APPROVAL_TOKEN_LEN: usize = 256;

/// The longest relative path the bridge accepts, in bytes.
pub const MAX_PATH_BYTES: usize = 256;

/// The longest text value an input field may carry, in bytes.
pub const MAX_FIELD_TEXT_BYTES: usize = 4_096;

/// The largest serialized object an input field may carry, in bytes.
pub const MAX_FIELD_OBJECT_BYTES: usize = 4 * 1024;

/// The largest item count an input field may ask for.
pub const MAX_FIELD_COUNT: u64 = 100;

/// The fields a request may carry. Anything else is an unknown field.
pub const REQUEST_FIELDS: [&str; 6] = [
    "version",
    "request_id",
    "capability",
    "input",
    "approval_token",
    "timeout_ms",
];

/// The outcome of one bridge request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeStatus {
    /// The capability ran and the answer carries its result.
    Ok,
    /// The capability needs a host approval; the request performed no action.
    ApprovalRequired,
    /// The host refused the request.
    Denied,
    /// The request was cancelled before it finished.
    Cancelled,
    /// The host cannot serve the capability, or the permission is not granted.
    Unavailable,
    /// The request or the host failed.
    Error,
}

impl BridgeStatus {
    /// The stable machine-readable status string.
    pub fn as_str(self) -> &'static str {
        match self {
            BridgeStatus::Ok => "ok",
            BridgeStatus::ApprovalRequired => "approval_required",
            BridgeStatus::Denied => "denied",
            BridgeStatus::Cancelled => "cancelled",
            BridgeStatus::Unavailable => "unavailable",
            BridgeStatus::Error => "error",
        }
    }
}

/// A structured failure detail.
///
/// The reason is a stable machine-readable string, the message says what went
/// wrong, and the remediation says what the caller can do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeError {
    /// The stable, machine-readable failure reason.
    pub reason: String,
    /// The human-readable failure detail.
    #[serde(default)]
    pub message: String,
    /// The remediation hint for the caller.
    #[serde(default)]
    pub remediation: String,
}

impl BridgeError {
    /// Builds a structured failure.
    ///
    /// Every field is bounded here, so a failure the bridge writes is bounded
    /// whatever the caller put into it: a request that names a long unknown
    /// field cannot make the answer grow.
    pub fn new(
        reason: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            reason: bounded_reason(&reason.into(), "bridge_error"),
            message: truncate(&message.into(), MAX_ERROR_MESSAGE_LEN),
            remediation: truncate(&remediation.into(), MAX_ERROR_MESSAGE_LEN),
        }
    }

    /// Builds a bounded failure out of the host's own strings.
    ///
    /// The host is a separate program, so its failure detail may be any
    /// length and any shape. The reason is kept when it reads like a reason
    /// — short and made of the characters a reason is made of — and the
    /// message is cut at a fixed length. Neither can grow the answer.
    pub fn from_host(reason: &str, message: &str, remediation: &str) -> Self {
        Self {
            reason: bounded_reason(reason, "host_error"),
            message: truncate(message, MAX_ERROR_MESSAGE_LEN),
            remediation: truncate(remediation, MAX_ERROR_MESSAGE_LEN),
        }
    }
}

/// Keeps a failure reason only when it reads like a reason.
///
/// A reason names a cause in a fixed vocabulary, so it is short and made of
/// lowercase letters, digits, and underscores. Anything else is replaced by
/// the fallback, which keeps the reason table of the contract true.
fn bounded_reason(reason: &str, fallback: &str) -> String {
    if !reason.is_empty()
        && reason.len() <= MAX_ERROR_REASON_LEN
        && reason.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
    {
        reason.to_string()
    } else {
        fallback.to_string()
    }
}

/// The longest failure reason the bridge copies from the host.
pub const MAX_ERROR_REASON_LEN: usize = 64;

/// The longest failure message the bridge copies from the host.
pub const MAX_ERROR_MESSAGE_LEN: usize = 1024;

/// Cuts a string at `limit` characters, on a character boundary.
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

/// The answer the bridge writes back to its caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeResponse {
    /// The protocol version the answer speaks.
    pub version: u64,
    /// The request id the answer belongs to.
    pub request_id: String,
    /// The outcome of the request.
    pub status: BridgeStatus,
    /// The capability result, when there is one.
    #[serde(default)]
    pub result: Option<Value>,
    /// The structured failure, when there is one.
    #[serde(default)]
    pub error: Option<BridgeError>,
}

impl BridgeResponse {
    /// Builds an answer that carries a failure.
    pub fn failure(
        request_id: impl Into<String>,
        status: BridgeStatus,
        error: BridgeError,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            status,
            result: None,
            error: Some(error),
        }
    }

    /// Builds an answer that carries a result.
    pub fn success(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            status: BridgeStatus::Ok,
            result: Some(result),
            error: None,
        }
    }

    /// The serialized size of the answer, in bytes.
    pub fn serialized_len(&self) -> usize {
        serde_json::to_string(self)
            .map(|text| text.len())
            .unwrap_or(0)
    }
}

/// A validated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeRequest {
    /// The request id the caller chose.
    pub request_id: String,
    /// The capability the request names.
    pub capability: String,
    /// The structured input the request carries.
    pub input: Value,
    /// The approval token a previous answer issued, when the caller replays it.
    pub approval_token: Option<String>,
    /// The deadline the caller asked for.
    pub timeout_ms: Option<u64>,
}

impl BridgeRequest {
    /// The digest of the request input, used to bind an approval to it.
    pub fn input_digest(&self) -> String {
        sha256_hex(self.input.to_string().as_bytes())
    }
}

/// A capability the protocol version 1 knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// The capability name a request uses.
    pub name: &'static str,
    /// What the capability asks the host for.
    pub summary: &'static str,
    /// Whether the capability needs a host approval before it may run.
    pub requires_approval: bool,
    /// The input fields the capability accepts.
    pub fields: &'static [Field],
}

/// One accepted input field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The field name.
    pub name: &'static str,
    /// The kind of value the field carries.
    pub kind: FieldKind,
    /// Whether the field must be present.
    pub required: bool,
}

/// The kind of value an input field carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// A bounded text value.
    Text,
    /// A bounded item count.
    Count,
    /// A relative path inside the host's own sandbox.
    Path,
    /// A bounded JSON object.
    Object,
}

/// The capabilities the protocol version 1 knows.
///
/// The names are the contract the mobile slices implement; the bridge itself
/// runs none of them.
pub const CAPABILITIES: [Capability; 5] = [
    Capability {
        name: "photos.list",
        summary: "List the photo assets the host exposes.",
        requires_approval: true,
        fields: &[
            Field {
                name: "limit",
                kind: FieldKind::Count,
                required: false,
            },
            Field {
                name: "cursor",
                kind: FieldKind::Text,
                required: false,
            },
        ],
    },
    Capability {
        name: "photos.read",
        summary: "Read one photo asset the host exposes.",
        requires_approval: true,
        fields: &[Field {
            name: "id",
            kind: FieldKind::Text,
            required: true,
        }],
    },
    Capability {
        name: "files.list",
        summary: "List the files the host exposes.",
        requires_approval: false,
        fields: &[
            Field {
                name: "path",
                kind: FieldKind::Path,
                required: false,
            },
            Field {
                name: "limit",
                kind: FieldKind::Count,
                required: false,
            },
        ],
    },
    Capability {
        name: "files.read",
        summary: "Read one file the host exposes.",
        requires_approval: true,
        fields: &[Field {
            name: "path",
            kind: FieldKind::Path,
            required: true,
        }],
    },
    Capability {
        name: "shortcuts.invoke",
        summary: "Run one named shortcut on the host.",
        requires_approval: true,
        fields: &[
            Field {
                name: "name",
                kind: FieldKind::Text,
                required: true,
            },
            Field {
                name: "input",
                kind: FieldKind::Object,
                required: false,
            },
        ],
    },
];

/// The capability with this name.
pub fn capability(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|entry| entry.name == name)
}

/// The names of the known capabilities.
pub fn capability_names() -> Vec<&'static str> {
    CAPABILITIES.iter().map(|entry| entry.name).collect()
}

/// Reads the request id out of an unparsed request, when it has one.
///
/// A request that fails validation still answers with the request id the
/// caller used, so the caller can correlate the failure with its request.
pub fn peek_request_id(value: &Value) -> String {
    value
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| valid_request_id(id))
        .unwrap_or_default()
        .to_string()
}

/// Whether a request id is well formed.
///
/// A request id is a short, printable, path-safe token: it names the request
/// in the audit trail and in the host transport's file names.
pub fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_LEN
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

/// Whether an approval token is well formed.
///
/// The token is a host-side credential that the bridge relays; it never
/// interprets it beyond matching it against the one it recorded.
pub fn valid_approval_token(token: &str) -> bool {
    (16..=MAX_APPROVAL_TOKEN_LEN).contains(&token.len())
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// Validates one request document.
///
/// Every check runs before the request reaches the host.
pub fn validate_request(value: &Value) -> Result<BridgeRequest, BridgeError> {
    let Some(object) = value.as_object() else {
        return Err(BridgeError::new(
            "invalid_request",
            "a request must be a JSON object",
            "send one line of JSON that follows the bridge request contract",
        ));
    };

    for field in object.keys() {
        if !REQUEST_FIELDS.contains(&field.as_str()) {
            return Err(BridgeError::new(
                "unknown_field",
                format!("the request carries the unknown field `{field}`"),
                format!("a request carries only {}", REQUEST_FIELDS.join(", ")),
            ));
        }
    }

    let version = match object.get("version") {
        Some(Value::Number(number)) => match number.as_u64() {
            Some(version) => version,
            None => {
                return Err(BridgeError::new(
                    "invalid_request",
                    "the request field `version` must be a non-negative integer",
                    "send the protocol version as a number",
                ));
            }
        },
        Some(_) => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request field `version` must be a non-negative integer",
                "send the protocol version as a number",
            ));
        }
        None => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request is missing the field `version`",
                "send the protocol version as a number",
            ));
        }
    };
    if version != PROTOCOL_VERSION {
        return Err(BridgeError::new(
            "unsupported_version",
            format!("the request speaks version {version}, not version {PROTOCOL_VERSION}"),
            "send a request that speaks version 1",
        ));
    }

    let request_id = match object.get("request_id") {
        Some(Value::String(id)) => id.clone(),
        Some(_) => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request field `request_id` must be a string",
                "send a short, printable request id",
            ));
        }
        None => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request is missing the field `request_id`",
                "send a short, printable request id",
            ));
        }
    };
    if !valid_request_id(&request_id) {
        return Err(BridgeError::new(
            "invalid_request_id",
            format!("the request id `{request_id}` is not a usable request id"),
            "use 1 to 128 letters, digits, dots, underscores, or dashes",
        ));
    }

    let capability_name = match object.get("capability") {
        Some(Value::String(name)) => name.clone(),
        Some(_) => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request field `capability` must be a string",
                "name one of the known capabilities",
            ));
        }
        None => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request is missing the field `capability`",
                "name one of the known capabilities",
            ));
        }
    };
    let Some(capability) = capability(&capability_name) else {
        return Err(BridgeError::new(
            "unknown_capability",
            format!("`{capability_name}` is not a known capability"),
            format!("ask for one of {}", capability_names().join(", ")),
        ));
    };

    let input = match object.get("input") {
        Some(Value::Object(fields)) => Value::Object(fields.clone()),
        Some(_) => {
            return Err(BridgeError::new(
                "invalid_input",
                "the request field `input` must be a JSON object",
                "send the capability input as a JSON object",
            ));
        }
        None => {
            return Err(BridgeError::new(
                "invalid_request",
                "the request is missing the field `input`",
                "send the capability input as a JSON object",
            ));
        }
    };
    if input.to_string().len() > MAX_INPUT_BYTES {
        return Err(BridgeError::new(
            "input_too_large",
            format!("the request input is larger than the {MAX_INPUT_BYTES} byte limit"),
            "send a smaller input",
        ));
    }
    validate_input(capability, &input)?;

    let approval_token = match object.get("approval_token") {
        None | Some(Value::Null) => None,
        Some(Value::String(token)) => {
            if !valid_approval_token(token) {
                return Err(BridgeError::new(
                    "invalid_approval_token",
                    "the request carries an approval token that is not a token",
                    "replay the token a previous answer issued",
                ));
            }
            Some(token.clone())
        }
        Some(_) => {
            return Err(BridgeError::new(
                "invalid_approval_token",
                "the request field `approval_token` must be a string",
                "replay the token a previous answer issued",
            ));
        }
    };

    let timeout_ms = match object.get("timeout_ms") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => {
            let Some(milliseconds) = number.as_u64() else {
                return Err(BridgeError::new(
                    "invalid_timeout",
                    "the request field `timeout_ms` must be a non-negative integer",
                    "ask for a deadline between 100 and 30000 milliseconds",
                ));
            };
            if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&milliseconds) {
                return Err(BridgeError::new(
                    "invalid_timeout",
                    format!(
                        "the request asks for {milliseconds} milliseconds, outside the \
                         {MIN_TIMEOUT_MS} to {MAX_TIMEOUT_MS} millisecond range"
                    ),
                    "ask for a deadline between 100 and 30000 milliseconds",
                ));
            }
            Some(milliseconds)
        }
        Some(_) => {
            return Err(BridgeError::new(
                "invalid_timeout",
                "the request field `timeout_ms` must be a non-negative integer",
                "ask for a deadline between 100 and 30000 milliseconds",
            ));
        }
    };

    Ok(BridgeRequest {
        request_id,
        capability: capability.name.to_string(),
        input,
        approval_token,
        timeout_ms,
    })
}

/// Validates the input object against the capability's field table.
fn validate_input(capability: &Capability, input: &Value) -> Result<(), BridgeError> {
    let Some(fields) = input.as_object() else {
        return Err(BridgeError::new(
            "invalid_input",
            "the request input must be a JSON object",
            "send the capability input as a JSON object",
        ));
    };

    for (name, value) in fields {
        let Some(field) = capability.fields.iter().find(|field| field.name == name) else {
            return Err(BridgeError::new(
                "unknown_field",
                format!(
                    "the capability `{}` does not accept the input field `{name}`",
                    capability.name
                ),
                format!(
                    "`{}` accepts {}",
                    capability.name,
                    capability
                        .fields
                        .iter()
                        .map(|field| field.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        };
        validate_field(capability, field, value)?;
    }

    for field in capability.fields.iter().filter(|field| field.required) {
        if !fields.contains_key(field.name) {
            return Err(BridgeError::new(
                "missing_field",
                format!(
                    "the capability `{}` needs the input field `{}`",
                    capability.name, field.name
                ),
                format!("add the `{}` field to the request input", field.name),
            ));
        }
    }

    Ok(())
}

/// Validates one input value against its field.
fn validate_field(
    capability: &Capability,
    field: &Field,
    value: &Value,
) -> Result<(), BridgeError> {
    match field.kind {
        FieldKind::Text => {
            let Some(text) = value.as_str() else {
                return Err(field_error(
                    capability,
                    field,
                    "must be a string",
                    "send a short text value",
                ));
            };
            if text.len() > MAX_FIELD_TEXT_BYTES {
                return Err(field_error(
                    capability,
                    field,
                    &format!("is longer than the {MAX_FIELD_TEXT_BYTES} byte limit"),
                    "send a shorter text value",
                ));
            }
            if text.chars().any(|character| character.is_control()) {
                return Err(field_error(
                    capability,
                    field,
                    "carries control characters",
                    "send a value without control characters",
                ));
            }
        }
        FieldKind::Count => {
            let Some(count) = value.as_u64() else {
                return Err(field_error(
                    capability,
                    field,
                    "must be a non-negative integer",
                    "ask for at most 100 items",
                ));
            };
            if count == 0 || count > MAX_FIELD_COUNT {
                return Err(field_error(
                    capability,
                    field,
                    &format!("must be between 1 and {MAX_FIELD_COUNT}"),
                    "ask for at most 100 items",
                ));
            }
        }
        FieldKind::Path => {
            let Some(path) = value.as_str() else {
                return Err(field_error(
                    capability,
                    field,
                    "must be a string",
                    "send a relative path",
                ));
            };
            if let Err(reason) = validate_path(path) {
                return Err(field_error(
                    capability,
                    field,
                    &format!("is not an allowed path: {reason}"),
                    "send a relative path inside the host's own sandbox",
                ));
            }
        }
        FieldKind::Object => {
            let Some(object) = value.as_object() else {
                return Err(field_error(
                    capability,
                    field,
                    "must be a JSON object",
                    "send an object",
                ));
            };
            if object.is_empty() {
                return Err(field_error(
                    capability,
                    field,
                    "must not be empty",
                    "send an object with the shortcut's inputs",
                ));
            }
            if value.to_string().len() > MAX_FIELD_OBJECT_BYTES {
                return Err(field_error(
                    capability,
                    field,
                    &format!("is larger than the {MAX_FIELD_OBJECT_BYTES} byte limit"),
                    "send a smaller object",
                ));
            }
        }
    }
    Ok(())
}

/// Builds a structured failure for one input field.
fn field_error(
    capability: &Capability,
    field: &Field,
    detail: &str,
    remediation: &str,
) -> BridgeError {
    BridgeError::new(
        if field.kind == FieldKind::Path {
            "invalid_path"
        } else {
            "invalid_input"
        },
        format!(
            "the input field `{}` of the capability `{}` {detail}",
            field.name, capability.name
        ),
        remediation.to_string(),
    )
}

/// Whether a path is one the bridge may forward to the host.
///
/// The bridge forwards relative paths only: a request never carries a host
/// path, a parent traversal, a URL, or a control character.
pub fn validate_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("the path is empty");
    }
    if path.len() > MAX_PATH_BYTES {
        return Err("the path is longer than the 256 byte limit");
    }
    if path.chars().any(|character| character.is_control()) {
        return Err("the path carries control characters");
    }
    if path.contains('\\') {
        return Err("the path carries a backslash");
    }
    if path.starts_with('/') {
        return Err("the path is absolute");
    }
    if path.starts_with('~') {
        return Err("the path starts with a home reference");
    }
    if path.contains("://") {
        return Err("the path carries a URL scheme");
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err("the path carries a drive letter");
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err("the path carries an empty segment");
        }
        if segment == "." || segment == ".." {
            return Err("the path carries a parent or current directory segment");
        }
    }
    Ok(())
}

/// The SHA-256 round constants.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The SHA-256 initial state.
const SHA256_H: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Computes the SHA-256 digest of `data` and returns it as lowercase hex.
///
/// The bridge digests the request input so an approval can be bound to the
/// exact input it was granted for, without keeping the input itself.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Computes the SHA-256 digest of `data`.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = SHA256_H;
    let mut block = [0u8; 64];
    let mut offset = 0;

    while offset + 64 <= data.len() {
        block.copy_from_slice(&data[offset..offset + 64]);
        sha256_compress(&mut state, &block);
        offset += 64;
    }

    let remaining = data.len() - offset;
    block = [0u8; 64];
    block[..remaining].copy_from_slice(&data[offset..]);
    block[remaining] = 0x80;
    if remaining + 9 > 64 {
        sha256_compress(&mut state, &block);
        block = [0u8; 64];
    }
    let bit_length = (data.len() as u64).wrapping_mul(8);
    block[56..].copy_from_slice(&bit_length.to_be_bytes());
    sha256_compress(&mut state, &block);

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Runs one SHA-256 compression round over a 64-byte block.
fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0u32; 64];
    for (index, word) in schedule.iter_mut().take(16).enumerate() {
        *word = u32::from_be_bytes([
            block[index * 4],
            block[index * 4 + 1],
            block[index * 4 + 2],
            block[index * 4 + 3],
        ]);
    }
    for index in 16..64 {
        let previous = schedule[index - 15];
        let recent = schedule[index - 2];
        let s0 = previous.rotate_right(7) ^ previous.rotate_right(18) ^ (previous >> 3);
        let s1 = recent.rotate_right(17) ^ recent.rotate_right(19) ^ (recent >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(choose)
            .wrapping_add(SHA256_K[index])
            .wrapping_add(schedule[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(majority);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}
