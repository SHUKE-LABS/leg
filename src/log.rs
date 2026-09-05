//! Reading and rendering the JSONL exchange-event trail.
//!
//! [`crate::events`] owns the write path: each `ask`/`session` exchange emits
//! a `request` line followed by exactly one outcome line, with `session`
//! additionally framing its run between `session_start`/`session_end`
//! markers. This module owns the read path — turning that trail back into
//! typed, paired [`Exchange`] values (`leg log show`/`leg log replay`) or
//! whole [`SessionRecord`]s (`leg session --resume`).
//!
//! Ported from baton's `crate::log`, trimmed to leg's non-goals: no
//! `message_id`-based correlation for concurrent writers (leg has no
//! multi-process writer to one trail — that is baton's `serve`/mailbox path),
//! and no `baton.message/v1` cross-trail merge (`log merge`).
//!
//! Unknown `event` tags are skipped (forward-compatibility with a newer
//! writer). A line that is not valid JSON is a hard parse error — except a
//! trailing partial line, one with no terminating newline left behind when a
//! `leg ask`/`session` process is killed mid-write: that is tolerated and
//! reported as a warning, so an unclean shutdown never bricks the whole trail.

use std::io::{BufRead, BufReader, Read};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{LegError, Result};
use crate::events::{Exchange, Outcome, RequestRecord};

/// The outcome of parsing a JSONL exchange trail: the complete [`Exchange`]
/// pairs and any non-fatal diagnostics collected along the way.
///
/// [`parse_jsonl`] is pure over its reader — it returns warnings here rather
/// than printing them, leaving stderr emission to the caller.
#[derive(Debug, Default)]
pub struct ParseReport {
    /// Complete request/outcome pairs, in file order.
    pub exchanges: Vec<Exchange>,
    /// Non-fatal diagnostics, in the order they were encountered.
    pub warnings: Vec<String>,
}

/// Parses a JSONL exchange trail into a [`ParseReport`] of paired [`Exchange`]
/// values plus any non-fatal warnings.
///
/// Each non-blank line is parsed as a standalone JSON object and dispatched on
/// its `event` tag. A `request` opens the single pending exchange; the next
/// `response_ok`/`response_error` line closes it — leg writes one trail per
/// process, so file-order pairing (no `message_id` correlation) is sufficient.
/// Behaviour at the edges:
///
/// - **Unknown `event` tag** (or a line with no `event`): skipped without
///   error, so a log written by a newer `leg` still parses.
/// - **Malformed JSON line**, or a known event missing required fields: a hard
///   [`LegError::Log`] naming the 1-based line number — *unless* the
///   offending line is the final one and was read without a terminating
///   newline (see below).
/// - **Trailing partial line**: the final line of the file with no
///   terminating `\n` is the signature of an unclean shutdown. A UTF-8 or
///   JSON-syntax failure there is not fatal: the line is skipped and recorded
///   in [`ParseReport::warnings`] so the caller can surface it, and the
///   exchanges already parsed are still yielded. The same failure on any
///   newline-terminated line is genuine corruption and stays a hard error.
/// - **Dangling outcome** (no matching pending request): not yielded, and
///   records a [`ParseReport::warnings`] entry rather than dropping silently.
/// - **Trailing request** with no outcome (a torn tail or an in-flight call):
///   not yielded and not warned — only complete pairs become an [`Exchange`].
pub fn parse_jsonl<R: Read>(reader: R) -> Result<ParseReport> {
    let mut buffered = BufReader::new(reader);
    let mut report = ParseReport::default();
    let mut pending: Option<RequestRecord> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut line_no = 0usize;

    loop {
        buf.clear();
        let read = buffered
            .read_until(b'\n', &mut buf)
            .map_err(|err| LegError::Io(format!("reading log line {}: {err}", line_no + 1)))?;
        if read == 0 {
            break;
        }
        line_no += 1;

        let terminated = buf.last() == Some(&b'\n');
        if buf.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }

        let value = match parse_line_value(&buf) {
            Ok(value) => value,
            Err(detail) if !terminated => {
                report.warnings.push(format!(
                    "skipped partial trailing line {line_no} of the event log \
                     (no terminating newline — likely an unclean shutdown): {detail}"
                ));
                continue;
            }
            Err(detail) => return Err(LegError::Log(format!("line {line_no}: {detail}"))),
        };

        match value.get("event").and_then(Value::as_str) {
            Some("request") => {
                let record: RequestRecord = from_value(value, line_no, "request")?;
                pending = Some(record);
            }
            Some("response_ok") | Some("response_error") => {
                let event = value
                    .get("event")
                    .and_then(Value::as_str)
                    .expect("matched above")
                    .to_string();
                let outcome: Outcome = from_value(value, line_no, &event)?;
                match pending.take() {
                    Some(request) => report.exchanges.push(Exchange { request, outcome }),
                    None => report
                        .warnings
                        .push(dangling_outcome_warning(line_no, &event)),
                }
            }
            _ => {}
        }
    }

    Ok(report)
}

/// One turn of a session, read back from the trail: the turn's `request`
/// (which carries `session_id` and `turn_index`) paired with its terminal
/// outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTurn {
    /// The request that opened this turn.
    pub request: RequestRecord,
    /// The turn's outcome, or `None` when the run was killed after the
    /// request line but before its outcome landed (a torn tail) — the request
    /// still counts as a turn; its answer just never arrived.
    pub outcome: Option<Outcome>,
}

/// A whole session reconstructed from the trail, keyed on `session_id`.
///
/// Partitioning keys on `session_id` alone, not on a matched start/end pair,
/// so a session killed mid-run (a `session_start` and turns but no
/// `session_end`) still forms one complete [`SessionRecord`] with
/// `ended == false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The id shared by this session's start marker and every turn's request.
    pub session_id: String,
    /// Whether a `session_start` marker for this id was seen on the trail.
    pub started: bool,
    /// Whether a `session_end` marker for this id was seen — `false` for a
    /// session killed mid-run.
    pub ended: bool,
    /// The turn count declared on the `session_end` marker, or `None` for a
    /// session with no end marker.
    pub declared_turns: Option<u64>,
    /// The session's turns, in file (== `turn_index`) order.
    pub turns: Vec<SessionTurn>,
}

/// The outcome of partitioning a trail into sessions: the [`SessionRecord`]s
/// in first-seen order plus any non-fatal diagnostics, mirroring
/// [`ParseReport`]'s shape.
#[derive(Debug, Default)]
pub struct SessionParseReport {
    /// Reconstructed sessions, in the order their `session_id` was first seen.
    pub sessions: Vec<SessionRecord>,
    /// Non-fatal diagnostics, in encounter order.
    pub warnings: Vec<String>,
}

/// Deserialization mirror of a `session_start` line.
#[derive(Deserialize)]
struct SessionStartRecord {
    session_id: String,
}

/// Deserialization mirror of a `session_end` line.
#[derive(Deserialize)]
struct SessionEndRecord {
    session_id: String,
    turns: u64,
}

/// Partitions a JSONL trail into whole sessions, keyed on `session_id`.
///
/// Reads the same trail as [`parse_jsonl`], but groups by session framing
/// rather than pairing bare exchanges: `session_start` / `session_end`
/// markers bound a session, and each session turn's `request` carries the
/// `session_id` + `turn_index` that place it. A sessionless (`ask`) request
/// carries neither and is skipped — it belongs to no session. Behaviour at
/// the edges mirrors [`parse_jsonl`]: a trailing partial line is tolerated
/// with a warning; any other malformed known event is a hard error.
pub fn parse_sessions<R: Read>(reader: R) -> Result<SessionParseReport> {
    let mut buffered = BufReader::new(reader);
    let mut report = SessionParseReport::default();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut line_no = 0usize;

    loop {
        buf.clear();
        let read = buffered
            .read_until(b'\n', &mut buf)
            .map_err(|err| LegError::Io(format!("reading log line {}: {err}", line_no + 1)))?;
        if read == 0 {
            break;
        }
        line_no += 1;

        let terminated = buf.last() == Some(&b'\n');
        if buf.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }

        let value = match parse_line_value(&buf) {
            Ok(value) => value,
            Err(detail) if !terminated => {
                report.warnings.push(format!(
                    "skipped partial trailing line {line_no} of the event log \
                     (no terminating newline — likely an unclean shutdown): {detail}"
                ));
                continue;
            }
            Err(detail) => return Err(LegError::Log(format!("line {line_no}: {detail}"))),
        };

        match value.get("event").and_then(Value::as_str) {
            Some("session_start") => {
                let start: SessionStartRecord = from_value(value, line_no, "session_start")?;
                let idx = session_index(&mut report.sessions, &mut index, &start.session_id);
                report.sessions[idx].started = true;
            }
            Some("session_end") => {
                let end: SessionEndRecord = from_value(value, line_no, "session_end")?;
                let idx = session_index(&mut report.sessions, &mut index, &end.session_id);
                report.sessions[idx].ended = true;
                report.sessions[idx].declared_turns = Some(end.turns);
            }
            Some("request") => {
                let record: RequestRecord = from_value(value, line_no, "request")?;
                if let Some(session_id) = record.session_id.clone() {
                    let idx = session_index(&mut report.sessions, &mut index, &session_id);
                    report.sessions[idx].turns.push(SessionTurn {
                        request: record,
                        outcome: None,
                    });
                }
                // A sessionless (`ask`) request carries no session_id and is
                // skipped — it belongs to no session.
            }
            Some("response_ok") | Some("response_error") => {
                let event = value
                    .get("event")
                    .and_then(Value::as_str)
                    .expect("matched above")
                    .to_string();
                let session_id = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let turn_index = value.get("turn_index").and_then(Value::as_u64);
                let outcome: Outcome = from_value(value, line_no, &event)?;
                // A sessionless (`ask`) outcome carries neither field and is
                // skipped — it belongs to no session.
                if let (Some(session_id), Some(turn_index)) = (session_id, turn_index) {
                    if let Some(&idx) = index.get(&session_id)
                        && let Some(turn) = report.sessions[idx]
                            .turns
                            .iter_mut()
                            .find(|t| t.request.turn_index == Some(turn_index))
                    {
                        turn.outcome = Some(outcome);
                    } else {
                        report
                            .warnings
                            .push(dangling_outcome_warning(line_no, &event));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(report)
}

/// Returns the index of the [`SessionRecord`] for `session_id`, creating an
/// empty one (in first-seen order) on first sighting.
fn session_index(
    sessions: &mut Vec<SessionRecord>,
    index: &mut std::collections::HashMap<String, usize>,
    session_id: &str,
) -> usize {
    if let Some(&idx) = index.get(session_id) {
        return idx;
    }
    let idx = sessions.len();
    sessions.push(SessionRecord {
        session_id: session_id.to_string(),
        started: false,
        ended: false,
        declared_turns: None,
        turns: Vec::new(),
    });
    index.insert(session_id.to_string(), idx);
    idx
}

/// Warning text for an outcome line with no matching pending request.
fn dangling_outcome_warning(line_no: usize, event: &str) -> String {
    format!(
        "line {line_no}: a {event} outcome had no matching pending request (a dangling \
         outcome) — its request may have been lost; the outcome is not shown"
    )
}

/// Deserializes a known event into `T`, mapping a shape mismatch onto a
/// [`LegError::Log`] that names the line and event so a corrupt trail points
/// at the offending entry.
fn from_value<T: serde::de::DeserializeOwned>(
    value: Value,
    line_no: usize,
    event: &str,
) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|err| LegError::Log(format!("line {line_no}: malformed {event} event: {err}")))
}

/// Parses one raw log line into a JSON [`Value`], returning a short detail
/// string on failure rather than a full [`LegError`].
fn parse_line_value(bytes: &[u8]) -> std::result::Result<Value, String> {
    let s = std::str::from_utf8(bytes).map_err(|err| format!("invalid UTF-8: {err}"))?;
    serde_json::from_str(s.trim()).map_err(|err| format!("invalid JSON: {err}"))
}

/// Renders one exchange as a human-readable multi-line block for `leg log show`.
///
/// `n` is the 1-based position shown to the user. The block carries the
/// timestamp, model, and call duration on its header line, then a truncated
/// prompt and either a truncated reply or the failure (`kind: message`).
pub fn format_exchange(n: usize, exchange: &Exchange) -> String {
    const MAX: usize = 120;
    let request = &exchange.request;
    let mut out = match &exchange.outcome {
        Outcome::Ok {
            duration_ms,
            reply,
            input_tokens,
            output_tokens,
            ..
        } => format!(
            "#{n}  {}  {}  ({duration_ms}ms)\n    prompt: {}\n    reply:  {}\n    tokens: {}",
            format_ts(request.ts_ms),
            request.model,
            excerpt(&request.prompt, MAX),
            excerpt(reply, MAX),
            format_tokens(*input_tokens, *output_tokens),
        ),
        Outcome::Error {
            duration_ms,
            kind,
            message,
            ..
        } => format!(
            "#{n}  {}  {}  ({duration_ms}ms)\n    prompt: {}\n    error:  {kind}: {}",
            format_ts(request.ts_ms),
            request.model,
            excerpt(&request.prompt, MAX),
            excerpt(message, MAX),
        ),
    };
    out.push('\n');
    out
}

/// Formats the reported token counts for a `response_ok` block.
fn format_tokens(input_tokens: Option<u64>, output_tokens: Option<u64>) -> String {
    match (input_tokens, output_tokens) {
        (None, None) => "unknown".to_string(),
        (input, output) => {
            let fmt = |t: Option<u64>| t.map_or_else(|| "?".to_string(), |n| n.to_string());
            format!("{} in, {} out", fmt(input), fmt(output))
        }
    }
}

/// Collapses newlines to spaces and truncates `s` to at most `max` characters,
/// appending `…` when truncation occurred.
///
/// Truncation is on `char` boundaries so a multibyte character is never split.
fn excerpt(s: &str, max: usize) -> String {
    let flattened: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if flattened.chars().count() <= max {
        return flattened;
    }
    let mut truncated: String = flattened.chars().take(max).collect();
    truncated.push('…');
    truncated
}

/// Formats Unix epoch milliseconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
///
/// Uses Howard Hinnant's civil-from-days algorithm so no date dependency is
/// pulled into the crate. Sub-second precision is dropped; the trail's
/// `ts_ms` stays available for machine consumers.
pub fn format_ts(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (hour, minute, second) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts a count of days since the Unix epoch into a `(year, month, day)`
/// civil date, via Howard Hinnant's well-known `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ExchangeMeta;
    use std::io::Cursor;

    fn meta() -> ExchangeMeta {
        ExchangeMeta {
            model: "m".to_string(),
            base_url: "u".to_string(),
        }
    }

    fn line(event: &crate::events::ExchangeEvent) -> String {
        format!("{}\n", serde_json::to_string(event).unwrap())
    }

    #[test]
    fn parses_valid_two_line_exchange() {
        let log = concat!(
            r#"{"event":"request","schema":"baton.exchange/v1","ts_ms":1700000000000,"model":"claude-sonnet-4-6","base_url":"https://api.anthropic.com","prompt":"hello"}"#,
            "\n",
            r#"{"event":"response_ok","schema":"baton.exchange/v1","ts_ms":1700000000420,"duration_ms":418,"reply":"hi there"}"#,
            "\n",
        );
        let exchanges = parse_jsonl(Cursor::new(log)).expect("parses").exchanges;
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].request.prompt, "hello");
        match &exchanges[0].outcome {
            Outcome::Ok { reply, .. } => assert_eq!(reply, "hi there"),
            other => panic!("expected Ok outcome, got {other:?}"),
        }
    }

    /// Byte-compatibility fixture: this exact JSONL — lifted verbatim from
    /// baton's own `parses_valid_two_line_exchange` test
    /// (`baton/src/log.rs`) — is both (a) parsed correctly by `leg`'s
    /// [`parse_jsonl`], and (b) exactly what `leg`'s own
    /// [`crate::events::ExchangeEvent`] serializes for a sessionless `ask`
    /// exchange with no token/stop-reason data. Field names, order, and the
    /// absence of unset optional fields must all match, since baton's own
    /// `log` reads leg's trail (and vice versa) for this ask/exchange subset.
    #[test]
    fn ask_exchange_is_byte_compatible_with_batons_trail_fixture() {
        let request_line = concat!(
            r#"{"event":"request","schema":"baton.exchange/v1","ts_ms":1700000000000,"#,
            r#""model":"claude-sonnet-4-6","base_url":"https://api.anthropic.com","prompt":"hello"}"#,
        );
        let response_line = concat!(
            r#"{"event":"response_ok","schema":"baton.exchange/v1","ts_ms":1700000000420,"#,
            r#""duration_ms":418,"reply":"hi there"}"#,
        );
        let log = format!("{request_line}\n{response_line}\n");

        let exchanges = parse_jsonl(Cursor::new(log.clone()))
            .expect("parses")
            .exchanges;
        assert_eq!(
            exchanges[0],
            Exchange {
                request: RequestRecord {
                    ts_ms: 1_700_000_000_000,
                    model: "claude-sonnet-4-6".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    prompt: "hello".to_string(),
                    session_id: None,
                    turn_index: None,
                },
                outcome: Outcome::Ok {
                    ts_ms: 1_700_000_000_420,
                    duration_ms: 418,
                    reply: "hi there".to_string(),
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    session_id: None,
                    turn_index: None,
                },
            }
        );

        let request_meta = ExchangeMeta {
            model: "claude-sonnet-4-6".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
        };
        let request_event =
            crate::events::ExchangeEvent::request(1_700_000_000_000, &request_meta, "hello");
        assert_eq!(line(&request_event), format!("{request_line}\n"));

        let response_event = crate::events::ExchangeEvent::response_ok(
            1_700_000_000_420,
            418,
            "hi there",
            None,
            None,
            None,
        );
        assert_eq!(line(&response_event), format!("{response_line}\n"));
    }

    #[test]
    fn parses_error_outcome() {
        let log = concat!(
            r#"{"event":"request","ts_ms":1,"model":"m","base_url":"u","prompt":"p"}"#,
            "\n",
            r#"{"event":"response_error","ts_ms":2,"duration_ms":7,"kind":"auth","message":"bad api key"}"#,
            "\n",
        );
        let exchanges = parse_jsonl(Cursor::new(log)).expect("parses").exchanges;
        assert_eq!(exchanges.len(), 1);
        match &exchanges[0].outcome {
            Outcome::Error { kind, message, .. } => {
                assert_eq!(kind, "auth");
                assert_eq!(message, "bad api key");
            }
            other => panic!("expected Error outcome, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_tags_are_skipped() {
        let log = concat!(
            r#"{"event":"heartbeat","ts_ms":1}"#,
            "\n",
            r#"{"event":"request","ts_ms":1,"model":"m","base_url":"u","prompt":"p"}"#,
            "\n",
            r#"{"event":"response_ok","ts_ms":2,"duration_ms":1,"reply":"r"}"#,
            "\n",
        );
        let exchanges = parse_jsonl(Cursor::new(log)).expect("parses").exchanges;
        assert_eq!(exchanges.len(), 1);
    }

    #[test]
    fn malformed_json_line_is_a_parse_error() {
        let log = concat!(
            r#"{"event":"request","ts_ms":1,"model":"m","base_url":"u","prompt":"p"}"#,
            "\n",
            "<<<not json at all>>>\n",
        );
        match parse_jsonl(Cursor::new(log)).unwrap_err() {
            LegError::Log(msg) => assert!(msg.contains("line 2"), "got: {msg}"),
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn trailing_partial_line_is_tolerated() {
        let log = concat!(
            r#"{"event":"request","ts_ms":1,"model":"m","base_url":"u","prompt":"p"}"#,
            "\n",
            r#"{"event":"response_ok","ts_ms":2,"duration_ms":1,"reply":"r"}"#,
            "\n",
            r#"{"event":"request","ts_ms":3,"model":"m","base_url":"u","prom"#,
        );
        let report = parse_jsonl(Cursor::new(log)).expect("tolerates trailing partial");
        assert_eq!(report.exchanges.len(), 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains("line 3"),
            "{}",
            report.warnings[0]
        );
    }

    #[test]
    fn dangling_outcome_with_no_pending_request_is_warned() {
        let log = concat!(
            r#"{"event":"response_ok","ts_ms":2,"duration_ms":1,"reply":"r"}"#,
            "\n",
        );
        let report = parse_jsonl(Cursor::new(log)).expect("parses");
        assert_eq!(report.exchanges.len(), 0);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn trailing_request_with_no_outcome_is_not_yielded() {
        let log = concat!(
            r#"{"event":"request","ts_ms":1,"model":"m","base_url":"u","prompt":"p"}"#,
            "\n",
        );
        let exchanges = parse_jsonl(Cursor::new(log)).expect("parses").exchanges;
        assert_eq!(exchanges.len(), 0);
    }

    #[test]
    fn parse_sessions_partitions_by_session_id_and_pairs_turns() {
        let meta = meta();
        let trail = [
            line(&crate::events::ExchangeEvent::session_start(1, "sess-1")),
            line(&crate::events::ExchangeEvent::session_request(
                2, &meta, "hi", "sess-1", 0,
            )),
            line(&crate::events::ExchangeEvent::session_response_ok(
                3, 1, "hello", None, None, None, "sess-1", 0,
            )),
            line(&crate::events::ExchangeEvent::session_end(4, "sess-1", 1)),
        ]
        .concat();

        let report = parse_sessions(Cursor::new(trail)).expect("parses");
        assert_eq!(report.sessions.len(), 1);
        let session = &report.sessions[0];
        assert_eq!(session.session_id, "sess-1");
        assert!(session.started);
        assert!(session.ended);
        assert_eq!(session.declared_turns, Some(1));
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].request.prompt, "hi");
        match &session.turns[0].outcome {
            Some(Outcome::Ok { reply, .. }) => assert_eq!(reply, "hello"),
            other => panic!("expected Ok outcome, got {other:?}"),
        }
    }

    #[test]
    fn parse_sessions_skips_sessionless_ask_lines() {
        let meta = meta();
        let trail = [
            line(&crate::events::ExchangeEvent::request(
                1,
                &meta,
                "ask prompt",
            )),
            line(&crate::events::ExchangeEvent::response_ok(
                2,
                1,
                "ask reply",
                None,
                None,
                None,
            )),
        ]
        .concat();

        let report = parse_sessions(Cursor::new(trail)).expect("parses");
        assert_eq!(report.sessions.len(), 0);
    }

    #[test]
    fn parse_sessions_leaves_torn_turn_outcome_none() {
        let trail = concat!(
            r#"{"event":"session_start","ts_ms":1,"session_id":"sess-1"}"#,
            "\n",
            r#"{"event":"request","ts_ms":2,"model":"m","base_url":"u","prompt":"hi","session_id":"sess-1","turn_index":0}"#,
            "\n",
        );
        let report = parse_sessions(Cursor::new(trail)).expect("parses");
        assert_eq!(report.sessions.len(), 1);
        assert!(!report.sessions[0].ended);
        assert_eq!(report.sessions[0].turns.len(), 1);
        assert_eq!(report.sessions[0].turns[0].outcome, None);
    }

    #[test]
    fn format_exchange_renders_ok_and_error_blocks() {
        let ok = Exchange {
            request: RequestRecord {
                ts_ms: 1_700_000_000_000,
                model: "claude-sonnet-4-6".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                prompt: "hello".to_string(),
                session_id: None,
                turn_index: None,
            },
            outcome: Outcome::Ok {
                ts_ms: 1_700_000_000_420,
                duration_ms: 418,
                reply: "hi there".to_string(),
                input_tokens: Some(12),
                output_tokens: Some(34),
                stop_reason: None,
                session_id: None,
                turn_index: None,
            },
        };
        let rendered = format_exchange(1, &ok);
        assert!(rendered.contains("#1"));
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("hi there"));
        assert!(rendered.contains("12 in, 34 out"));
    }

    #[test]
    fn format_ts_renders_utc_civil_date() {
        assert_eq!(format_ts(1_700_000_000_000), "2023-11-14T22:13:20Z");
    }
}
