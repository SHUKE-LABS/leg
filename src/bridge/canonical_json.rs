//! The RFC 8785 JSON Canonicalization Scheme the approval digest is made of.
//!
//! The guest bridge and the host compute `input_sha256` over the canonical
//! bytes of the request input, so a reordered input continues an approval
//! and a changed value does not. Both sides read the raw token stream rather
//! than a generic value, because a map keeps the last of two members with the
//! same name and the two sides would then read one request two ways:
//!
//! - a member name that occurs twice in one object, also after the escapes
//!   are undone, is refused;
//! - a number is read as an IEEE-754 binary64; a number that is not finite,
//!   that overflows, or that is a non-zero literal underflowing to zero is
//!   refused (I-JSON), and `-0` reads as `0`;
//! - object members are sorted by the UTF-16 code units of their names;
//! - strings use the RFC 8785 escapes, and numbers the ECMAScript shortest
//!   round-trip form.
//!
//! The host side is `ios/LegHost/BridgeCanonicalJSON.swift`; the two are
//! held to the same vectors.

use std::fmt;

use serde_json::{Map, Number, Value};

use super::protocol::sha256_hex;

/// The deepest nesting the parser follows.
pub const MAX_DEPTH: usize = 64;

/// One JSON value, kept in the order the bytes carried it.
#[derive(Debug, Clone, PartialEq)]
pub enum JcsValue {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A finite binary64 number.
    Number(f64),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<JcsValue>),
    /// An object, with its members in wire order and no duplicate name.
    Object(Vec<(String, JcsValue)>),
}

/// Why text is not a JSON document the bridge accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalError {
    /// The text is not JSON.
    Malformed(String),
    /// An object names the same member twice, after the escapes are undone.
    DuplicateKey(String),
    /// A number is not a finite binary64 value, or does not survive the
    /// conversion to one.
    InvalidNumber(String),
    /// The document nests deeper than the limit.
    TooDeep,
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonicalError::Malformed(detail) => write!(formatter, "{detail}"),
            CanonicalError::DuplicateKey(key) => write!(
                formatter,
                "an object names the member `{}` twice",
                key.chars().take(64).collect::<String>()
            ),
            CanonicalError::InvalidNumber(literal) => write!(
                formatter,
                "the number `{}` is not a finite binary64 value",
                literal.chars().take(64).collect::<String>()
            ),
            CanonicalError::TooDeep => {
                write!(
                    formatter,
                    "the document nests deeper than {MAX_DEPTH} levels"
                )
            }
        }
    }
}

/// Reads one JSON document.
pub fn parse(text: &str) -> Result<JcsValue, CanonicalError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        index: 0,
    };
    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();
    if parser.index != parser.bytes.len() {
        return Err(CanonicalError::Malformed(
            "trailing bytes after the document".to_string(),
        ));
    }
    Ok(value)
}

/// The canonical text of the JSON document in `text`.
pub fn canonicalize(text: &str) -> Result<String, CanonicalError> {
    Ok(parse(text)?.canonical())
}

/// The lowercase hex SHA-256 of the canonical text of a value.
pub fn digest(value: &Value) -> String {
    sha256_hex(canonical_value(value).as_bytes())
}

/// The canonical text of a value that was already read.
///
/// A `Value` cannot hold a duplicate member, so this is only sound for a
/// value that came through [`parse`] or was built by the bridge itself.
pub fn canonical_value(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

impl JcsValue {
    /// The canonical text of the value.
    pub fn canonical(&self) -> String {
        canonical_value(&self.to_value())
    }

    /// The value as a `serde_json::Value`, once no duplicate can be lost.
    ///
    /// A number without a fraction that a binary64 holds exactly becomes an
    /// integer, so the bridge's own integer checks read it as one.
    pub fn to_value(&self) -> Value {
        match self {
            JcsValue::Null => Value::Null,
            JcsValue::Bool(flag) => Value::Bool(*flag),
            JcsValue::Number(number) => number_value(*number),
            JcsValue::String(text) => Value::String(text.clone()),
            JcsValue::Array(items) => Value::Array(items.iter().map(JcsValue::to_value).collect()),
            JcsValue::Object(members) => {
                let mut map = Map::new();
                for (key, value) in members {
                    map.insert(key.clone(), value.to_value());
                }
                Value::Object(map)
            }
        }
    }
}

/// The largest integer a binary64 holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_992.0;

/// A number as a `serde_json::Value`.
fn number_value(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER {
        if number >= 0.0 {
            return Value::Number(Number::from(number as u64));
        }
        return Value::Number(Number::from(number as i64));
    }
    Number::from_f64(number)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&format_number(number.as_f64().unwrap_or(0.0))),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            members.sort_by(|left, right| left.0.encode_utf16().cmp(right.0.encode_utf16()));
            out.push('{');
            for (index, (key, member)) in members.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(member, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// The ECMAScript `Number.prototype.toString` form of a finite number.
///
/// Rust prints the shortest digits that round-trip, as ECMAScript does; only
/// the layout differs, so the digits and the decimal exponent are taken out
/// of the scientific form and laid out again.
pub fn format_number(number: f64) -> String {
    if number == 0.0 {
        return "0".to_string();
    }
    let negative = number < 0.0;
    let scientific = format!("{:e}", number.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let k = digits.len() as i64;
    // The decimal point sits after `n` digits.
    let n = exponent + 1;
    let text = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        let mantissa = if k == 1 {
            digits.to_string()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{mantissa}e{sign}{}", e.abs())
    };
    if negative { format!("-{text}") } else { text }
}

/// The reader of one document.
struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Parser<'_> {
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.index += 1;
        }
    }

    fn malformed(detail: &str) -> CanonicalError {
        CanonicalError::Malformed(detail.to_string())
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn value(&mut self, depth: usize) -> Result<JcsValue, CanonicalError> {
        if depth > MAX_DEPTH {
            return Err(CanonicalError::TooDeep);
        }
        match self.peek() {
            None => Err(Self::malformed("unexpected end of the document")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(JcsValue::String(self.string()?)),
            Some(b't') => self.literal("true", JcsValue::Bool(true)),
            Some(b'f') => self.literal("false", JcsValue::Bool(false)),
            Some(b'n') => self.literal("null", JcsValue::Null),
            Some(_) => Ok(JcsValue::Number(self.number()?)),
        }
    }

    fn literal(&mut self, word: &str, value: JcsValue) -> Result<JcsValue, CanonicalError> {
        if self.bytes[self.index..].starts_with(word.as_bytes()) {
            self.index += word.len();
            Ok(value)
        } else {
            Err(Self::malformed("unexpected token"))
        }
    }

    fn object(&mut self, depth: usize) -> Result<JcsValue, CanonicalError> {
        self.index += 1;
        let mut members: Vec<(String, JcsValue)> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.index += 1;
            return Ok(JcsValue::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(Self::malformed("an object member needs a name"));
            }
            let key = self.string()?;
            // The names are compared after the escapes are undone: `"a"` and
            // `"\u0061"` are the same name.
            if members.iter().any(|(existing, _)| *existing == key) {
                return Err(CanonicalError::DuplicateKey(key));
            }
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(Self::malformed("an object member needs a colon"));
            }
            self.index += 1;
            self.skip_whitespace();
            let member = self.value(depth + 1)?;
            members.push((key, member));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.index += 1,
                Some(b'}') => {
                    self.index += 1;
                    return Ok(JcsValue::Object(members));
                }
                None => return Err(Self::malformed("unexpected end of the document")),
                Some(_) => return Err(Self::malformed("unexpected token in an object")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<JcsValue, CanonicalError> {
        self.index += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.index += 1;
            return Ok(JcsValue::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.index += 1,
                Some(b']') => {
                    self.index += 1;
                    return Ok(JcsValue::Array(items));
                }
                None => return Err(Self::malformed("unexpected end of the document")),
                Some(_) => return Err(Self::malformed("unexpected token in an array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, CanonicalError> {
        self.index += 1;
        let mut out = String::new();
        loop {
            let start = self.index;
            while matches!(self.peek(), Some(byte) if byte != b'"' && byte != b'\\' && byte >= 0x20)
            {
                self.index += 1;
            }
            // The text is a `&str` and the run stops only at ASCII bytes, so
            // the run is valid UTF-8 on its own.
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.index])
                    .map_err(|_| Self::malformed("a string is not valid UTF-8"))?,
            );
            match self.peek() {
                None => return Err(Self::malformed("an unterminated string")),
                Some(b'"') => {
                    self.index += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.index += 1;
                    let escape = self
                        .peek()
                        .ok_or_else(|| Self::malformed("an unterminated escape"))?;
                    self.index += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(Self::malformed("an unknown escape")),
                    }
                }
                Some(_) => {
                    return Err(Self::malformed("a string carries a raw control character"));
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, CanonicalError> {
        let unit = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&unit) {
            // A high surrogate needs its low half right after it.
            if !self.bytes[self.index..].starts_with(b"\\u") {
                return Err(Self::malformed("a lone surrogate"));
            }
            self.index += 2;
            let low = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(Self::malformed("a lone surrogate"));
            }
            let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(code).ok_or_else(|| Self::malformed("a lone surrogate"));
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            return Err(Self::malformed("a lone surrogate"));
        }
        char::from_u32(unit).ok_or_else(|| Self::malformed("a bad escape"))
    }

    fn hex4(&mut self) -> Result<u32, CanonicalError> {
        let Some(digits) = self.bytes.get(self.index..self.index + 4) else {
            return Err(Self::malformed("a short escape"));
        };
        let mut value = 0u32;
        for byte in digits {
            let digit = (*byte as char)
                .to_digit(16)
                .ok_or_else(|| Self::malformed("a bad escape"))?;
            value = value * 16 + digit;
        }
        self.index += 4;
        Ok(value)
    }

    fn digits(&mut self) -> usize {
        let start = self.index;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.index += 1;
        }
        self.index - start
    }

    fn number(&mut self) -> Result<f64, CanonicalError> {
        let start = self.index;
        if self.peek() == Some(b'-') {
            self.index += 1;
        }
        match self.peek() {
            Some(b'0') => self.index += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(Self::malformed("unexpected token")),
        }
        let mut significant = self.bytes[start..self.index]
            .iter()
            .any(|byte| matches!(byte, b'1'..=b'9'));
        if self.peek() == Some(b'.') {
            self.index += 1;
            let fraction = self.index;
            if self.digits() == 0 {
                return Err(Self::malformed("a fraction needs digits"));
            }
            if self.bytes[fraction..self.index]
                .iter()
                .any(|byte| *byte != b'0')
            {
                significant = true;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if self.digits() == 0 {
                return Err(Self::malformed("an exponent needs digits"));
            }
        }
        // The slice holds only ASCII digits, signs, dots, and exponents.
        let literal = std::str::from_utf8(&self.bytes[start..self.index]).unwrap_or_default();
        let value: f64 = literal
            .parse()
            .map_err(|_| CanonicalError::InvalidNumber(literal.to_string()))?;
        if !value.is_finite() {
            return Err(CanonicalError::InvalidNumber(literal.to_string()));
        }
        // A non-zero literal that reads as zero lost all of its value: it is
        // not a number a binary64 holds.
        if value == 0.0 && significant {
            return Err(CanonicalError::InvalidNumber(literal.to_string()));
        }
        Ok(if value == 0.0 { 0.0 } else { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vectors the guest bridge and the host share.
    const VECTORS: [(&str, &str, &str); 4] = [
        (
            r#"{"sort":"duration_desc","filter":{"media_type":"video"},"limit":20}"#,
            r#"{"filter":{"media_type":"video"},"limit":20,"sort":"duration_desc"}"#,
            "a9a83ee45388e790690d5f5683782ecf738d5f703189c18b139a01f29c8bce4a",
        ),
        (
            r#"{"destination":"guest_workspace"}"#,
            r#"{"destination":"guest_workspace"}"#,
            "8045fd38b79820c6825bd8096a324dea1431cfe9e3e17870b1592de88295a750",
        ),
        (
            r#"{"\uE000":"bmp","\uD834\uDD1E":"supp"}"#,
            "{\"\u{1D11E}\":\"supp\",\"\u{E000}\":\"bmp\"}",
            "73e96d65d5acff2a00259e91342ce76055ab13ce7809e00e9920246333341f59",
        ),
        (
            r#"{"n":1.0,"small":1e-6,"tiny":1e-7,"huge":1e21,"negzero":-0}"#,
            r#"{"huge":1e+21,"n":1,"negzero":0,"small":0.000001,"tiny":1e-7}"#,
            "c25482e66991d36fc4ac2b9d6aa6ede849b011530dbed02a79fda81c5974dd24",
        ),
    ];

    #[test]
    fn the_shared_vectors_canonicalize_and_digest() {
        for (wire, canonical, expected) in VECTORS {
            let text = canonicalize(wire).expect("the vector is canonical JSON");
            assert_eq!(text, canonical, "canonical text of {wire}");
            assert_eq!(sha256_hex(text.as_bytes()), expected, "digest of {wire}");
            let value = parse(wire).expect("parse").to_value();
            assert_eq!(digest(&value), expected, "digest of the value of {wire}");
        }
    }

    #[test]
    fn a_duplicate_member_is_refused_before_materialization() {
        for wire in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":1,"\u0061":2}"#,
            r#"{"o":{"b":1,"b":1}}"#,
        ] {
            assert!(
                matches!(parse(wire), Err(CanonicalError::DuplicateKey(_))),
                "{wire} was accepted"
            );
        }
    }

    #[test]
    fn a_number_a_binary64_cannot_hold_is_refused() {
        for wire in ["1e400", "-1e400", "1e-400", "-2.5e-999"] {
            assert!(
                matches!(parse(wire), Err(CanonicalError::InvalidNumber(_))),
                "{wire} was accepted"
            );
        }
        // A zero literal is zero, however small its exponent.
        assert_eq!(parse("0.000e-999"), Ok(JcsValue::Number(0.0)));
        assert_eq!(parse("-0"), Ok(JcsValue::Number(0.0)));
        for wire in ["NaN", "Infinity", "-", "01", "1.", ".5", "1e", "+1"] {
            assert!(parse(wire).is_err(), "{wire} was accepted");
        }
    }

    #[test]
    fn malformed_strings_are_refused() {
        for wire in [
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""\uD800A""#,
            "\"a\u{1}\"",
            r#""\x""#,
            r#""open"#,
            "[1,]",
            "{\"a\":1,}",
            "1 2",
        ] {
            assert!(parse(wire).is_err(), "{wire} was accepted");
        }
    }

    #[test]
    fn deep_nesting_is_refused() {
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert_eq!(parse(&deep), Err(CanonicalError::TooDeep));
    }

    #[test]
    fn numbers_take_the_ecmascript_form() {
        let cases: [(f64, &str); 14] = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (123.0, "123"),
            (0.1, "0.1"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (0.000001234, "0.000001234"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (5e-324, "5e-324"),
            (123456789012345680000.0, "123456789012345680000"),
        ];
        for (number, expected) in cases {
            assert_eq!(format_number(number), expected, "{number:e}");
        }
    }

    #[test]
    fn strings_take_the_rfc_8785_escapes() {
        let text = canonicalize(r#""\u0001\b\t\n\f\r\"\\\/\u00e9\u2028""#).expect("canonical");
        assert_eq!(text, "\"\\u0001\\b\\t\\n\\f\\r\\\"\\\\/\u{e9}\u{2028}\"");
    }

    #[test]
    fn members_sort_by_utf16_code_units() {
        // U+FF61 sorts before U+1F600 by scalar value, after it by UTF-16.
        let text = canonicalize(r#"{"\uFF61":1,"\uD83D\uDE00":2,"b":3,"a":4}"#).expect("canonical");
        assert_eq!(text, "{\"a\":4,\"b\":3,\"\u{1F600}\":2,\"\u{FF61}\":1}");
    }

    #[test]
    fn an_integral_number_reads_as_an_integer() {
        let value = parse(r#"{"limit":20.0,"big":1e21,"neg":-3}"#)
            .expect("parse")
            .to_value();
        assert_eq!(value["limit"].as_u64(), Some(20));
        assert_eq!(value["neg"].as_i64(), Some(-3));
        assert_eq!(value["big"].as_u64(), None);
    }
}
