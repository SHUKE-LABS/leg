use super::adapter::{
    self, ConnectionError, HostAdapter, HostConnection, HostExchange, HostOutcome, HostRequest,
};
use super::protocol::{self, BridgeResponse, BridgeStatus};
use super::service::Bridge;
use super::state::{AuditLog, BridgeState, RequestRecord};

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A scratch directory removed when the test ends.
struct Scratch {
    /// The scratch directory itself.
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Creates a scratch directory named after the test.
fn scratch(name: &str) -> Scratch {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "leg-bridge-tests-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch directory");
    Scratch { path }
}

/// A host the test drives, with one answer per request.
struct FakeHost {
    /// The answers the host gives, in order.
    answers: Vec<HostOutcome>,
    /// The requests the host received.
    seen: Vec<HostRequest>,
    /// Whether the request reached the host.
    reached: bool,
}

impl FakeHost {
    /// A host that answers every request the same way.
    ///
    /// The request reached the host, so its id is used.
    fn always(answer: HostOutcome) -> Self {
        FakeHost {
            answers: vec![answer],
            seen: Vec::new(),
            reached: true,
        }
    }

    /// A host the request never reached.
    ///
    /// Nothing was carried to the host, so the request id stays free.
    fn never_reached(answer: HostOutcome) -> Self {
        FakeHost {
            answers: vec![answer],
            seen: Vec::new(),
            reached: false,
        }
    }
}

impl HostAdapter for FakeHost {
    fn dispatch(&mut self, request: &HostRequest, _deadline: Instant) -> HostExchange {
        self.seen.push(request.clone());
        let outcome = if self.answers.len() > 1 {
            self.answers.remove(0)
        } else {
            self.answers
                .first()
                .cloned()
                .unwrap_or(HostOutcome::Cancelled)
        };
        HostExchange {
            outcome,
            reached: self.reached,
        }
    }
}

/// A connection the host answers over, without a socket.
struct FakeConnection {
    /// The frames the host sends, in order.
    frames: Vec<Vec<u8>>,
    /// The frames the bridge sent.
    sent: Vec<String>,
    /// Whether the connection stays silent instead of closing.
    silent: bool,
    /// The failure the write ends in, when the write fails.
    send_error: Option<ConnectionError>,
}

impl FakeConnection {
    /// A connection that answers with `frames`.
    fn answering(frames: Vec<String>) -> Self {
        Self::answering_bytes(frames.into_iter().map(|frame| frame.into_bytes()).collect())
    }

    /// A connection that answers with raw frames.
    fn answering_bytes(frames: Vec<Vec<u8>>) -> Self {
        Self {
            frames,
            sent: Vec::new(),
            silent: false,
            send_error: None,
        }
    }

    /// A connection that never answers.
    fn silent() -> Self {
        Self {
            frames: Vec::new(),
            sent: Vec::new(),
            silent: true,
            send_error: None,
        }
    }

    /// A connection whose write ends in `error`.
    fn failing(error: ConnectionError) -> Self {
        Self {
            frames: Vec::new(),
            sent: Vec::new(),
            silent: false,
            send_error: Some(error),
        }
    }
}

impl HostConnection for FakeConnection {
    fn send(&mut self, frame: &str, _deadline: Instant) -> Result<(), ConnectionError> {
        self.sent.push(frame.to_string());
        match &self.send_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn receive(&mut self, wait: Duration) -> Result<Option<Vec<u8>>, ConnectionError> {
        if self.frames.is_empty() {
            if self.silent {
                std::thread::sleep(wait.min(Duration::from_millis(5)));
                return Ok(None);
            }
            return Err(ConnectionError::Closed);
        }
        Ok(Some(self.frames.remove(0)))
    }
}

/// Builds a bridge over a fake host and a scratch state directory.
fn bridge(host: FakeHost, scratch: &Scratch) -> Bridge {
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let audit = AuditLog::new(Some(scratch.path.join("audit.log")));
    Bridge::new(Box::new(host), state, audit)
}

/// The audit log of a bridge run.
fn audit_log(scratch: &Scratch) -> String {
    fs::read_to_string(scratch.path.join("audit.log")).unwrap_or_default()
}

/// A request line the bridge accepts.
fn request(id: &str, capability: &str, input: Value) -> String {
    json!({
        "version": 1,
        "request_id": id,
        "capability": capability,
        "input": input,
    })
    .to_string()
}

/// The frame the host answers with.
fn reply(request: &HostRequest, status: &str, result: Value) -> String {
    json!({
        "version": 1,
        "nonce": request.nonce,
        "request_id": request.request_id,
        "status": status,
        "result": result,
        "error": null,
    })
    .to_string()
}

/// A host request the exchange sends.
fn host_request() -> HostRequest {
    HostRequest {
        version: protocol::PROTOCOL_VERSION,
        request_id: "r-1".to_string(),
        nonce: "nonce-0123456789abcdef".to_string(),
        capability: "files.list".to_string(),
        input: json!({"limit": 1}),
        input_sha256: protocol::sha256_hex(b"{}"),
        approval_token: None,
        timeout_ms: 1000,
    }
}

/// A host every bridge in the test shares.
struct SharedHost {
    /// The requests the host received.
    seen: Arc<Mutex<Vec<HostRequest>>>,
}

impl HostAdapter for SharedHost {
    fn dispatch(&mut self, request: &HostRequest, _deadline: Instant) -> HostExchange {
        self.seen
            .lock()
            .expect("the host lock")
            .push(request.clone());
        HostExchange {
            outcome: HostOutcome::Ok(json!({"items": []})),
            reached: true,
        }
    }
}

#[test]
fn two_invocations_cannot_dispatch_the_same_request_id() {
    let scratch = scratch("concurrent");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let start = Arc::new(Barrier::new(2));

    let mut threads = Vec::new();
    for _ in 0..2 {
        let seen = Arc::clone(&seen);
        let start = Arc::clone(&start);
        let dir = scratch.path.clone();
        threads.push(std::thread::spawn(move || {
            let state = BridgeState::load(&dir).expect("load the bridge state");
            let mut bridge = Bridge::new(
                Box::new(SharedHost {
                    seen: Arc::clone(&seen),
                }),
                state,
                AuditLog::new(None),
            );
            start.wait();
            bridge.handle(&request("r-1", "files.list", json!({})))
        }));
    }

    let answers: Vec<BridgeResponse> = threads
        .into_iter()
        .map(|thread| thread.join().expect("the thread finished"))
        .collect();

    assert_eq!(
        seen.lock().expect("the host lock").len(),
        1,
        "the request id reached the host twice"
    );
    assert_eq!(
        answers
            .iter()
            .filter(|answer| answer.status == BridgeStatus::Ok)
            .count(),
        1,
        "both invocations dispatched the request"
    );
    let reasons: Vec<String> = answers
        .iter()
        .filter_map(|answer| answer.error.as_ref().map(|error| error.reason.clone()))
        .collect();
    assert!(
        reasons.contains(&"duplicate_request_id".to_string()),
        "the second invocation was not refused: {reasons:?}"
    );
}

#[test]
fn a_host_that_sends_a_huge_failure_does_not_grow_the_answer() {
    let scratch = scratch("huge-error");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Denied {
            reason: "r".repeat(10_000),
            message: "m".repeat(100_000),
        }),
        &scratch,
    );

    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Denied);
    let error = response.error.clone().expect("the request failed");
    assert_eq!(error.reason, "host_error");
    assert!(
        error.message.chars().count() <= protocol::MAX_ERROR_MESSAGE_LEN,
        "the failure message was not bounded"
    );
    assert!(
        response.serialized_len() <= protocol::MAX_RESPONSE_BYTES,
        "the answer was not bounded"
    );
}

#[test]
fn a_result_the_host_makes_too_large_is_refused() {
    let scratch = scratch("huge-result");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": ["x".repeat(70 * 1024)]}))),
        &scratch,
    );

    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.as_ref().expect("the request failed").reason,
        "result_too_large"
    );
    assert!(response.serialized_len() <= protocol::MAX_RESPONSE_BYTES);
}

#[test]
fn sha256_matches_the_published_vectors() {
    assert_eq!(
        protocol::sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        protocol::sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        protocol::sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn only_relative_normalized_paths_are_allowed() {
    assert!(protocol::validate_path("photos/2024/summer.jpg").is_ok());
    for path in [
        "",
        "/etc/passwd",
        "../secrets",
        "photos/../../secrets",
        "photos//summer.jpg",
        "photos/",
        "photos/./summer.jpg",
        "C:\\Windows\\win.ini",
        "photos\\summer.jpg",
        "~/notes.txt",
        "https://example.com/photo.jpg",
        "photos/summer\u{0}.jpg",
    ] {
        assert!(
            protocol::validate_path(path).is_err(),
            "the path {path:?} was allowed"
        );
    }
}

#[test]
fn requests_are_validated_before_they_reach_the_host() {
    let cases: [(&str, &str); 7] = [
        (
            r#"{"version":2,"request_id":"r-1","capability":"photos.list","input":{}}"#,
            "unsupported_version",
        ),
        (
            r#"{"version":1,"request_id":"r-1","capability":"photos.list","input":{},"shell":"rm -rf /"}"#,
            "unknown_field",
        ),
        (
            r#"{"version":1,"request_id":"r-1","capability":"photos.delete","input":{}}"#,
            "unknown_capability",
        ),
        (
            r#"{"version":1,"request_id":"r-1","capability":"files.read","input":{}}"#,
            "missing_field",
        ),
        (
            r#"{"version":1,"request_id":"r-1","capability":"files.read","input":{"path":"/etc/passwd"}}"#,
            "invalid_path",
        ),
        (
            r#"{"version":1,"request_id":"r-1","capability":"photos.list","input":{"limit":"many"}}"#,
            "invalid_input",
        ),
        (
            r#"{"version":1,"request_id":"r-1","capability":"photos.list","input":{"offset":1}}"#,
            "unknown_field",
        ),
    ];

    for (line, reason) in cases {
        let error = protocol::validate_request(&serde_json::from_str::<Value>(line).unwrap())
            .expect_err("the request was accepted");
        assert_eq!(error.reason, reason, "the request {line} was accepted");
    }
}

#[test]
fn malformed_and_oversized_requests_are_refused() {
    let scratch = scratch("malformed");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": []}))),
        &scratch,
    );

    let malformed = bridge.handle("{not json");
    assert_eq!(malformed.status, BridgeStatus::Error);
    assert_eq!(
        malformed
            .error
            .expect("the malformed request failed")
            .reason,
        "malformed_request"
    );

    let oversized = bridge.handle(&"x".repeat(protocol::MAX_REQUEST_BYTES + 1));
    assert_eq!(oversized.status, BridgeStatus::Error);
    assert_eq!(
        oversized
            .error
            .expect("the oversized request failed")
            .reason,
        "request_too_large"
    );
}

#[test]
fn a_successful_request_answers_with_the_result_and_the_request_id() {
    let scratch = scratch("success");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": ["a.jpg"]}))),
        &scratch,
    );

    let response = bridge.handle(&request("r-1", "files.list", json!({"limit": 1})));
    assert_eq!(response.status, BridgeStatus::Ok);
    assert_eq!(response.request_id, "r-1");
    assert_eq!(response.version, protocol::PROTOCOL_VERSION);
    assert_eq!(response.result, Some(json!({"items": ["a.jpg"]})));
    assert!(response.error.is_none());
}

#[test]
fn an_approval_is_required_before_the_capability_runs() {
    let scratch = scratch("approval");
    let mut bridge = bridge(
        FakeHost {
            answers: vec![
                HostOutcome::ApprovalRequired {
                    approval_token: "token-0123456789abcdef".to_string(),
                    review: json!({"summary": "reads the photo library"}),
                },
                HostOutcome::Ok(json!({"items": []})),
            ],
            seen: Vec::new(),
            reached: true,
        },
        &scratch,
    );

    let pending = bridge.handle(&request("r-1", "photos.list", json!({"limit": 1})));
    assert_eq!(pending.status, BridgeStatus::ApprovalRequired);
    let result = pending.result.expect("the answer carries a result");
    let token = result["approval_token"]
        .as_str()
        .expect("the answer carries a token")
        .to_string();
    assert_eq!(token, "token-0123456789abcdef");
    assert_eq!(result["review"]["summary"], "reads the photo library");

    // The follow-up request carries a fresh request id, the same capability
    // and input, and the token.
    let follow_up = json!({
        "version": 1,
        "request_id": "r-2",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": token,
    })
    .to_string();
    let response = bridge.handle(&follow_up);
    assert_eq!(response.status, BridgeStatus::Ok);
    assert_eq!(response.request_id, "r-2");
}

#[test]
fn an_approval_is_bound_to_the_capability_and_its_input() {
    let scratch = scratch("approval-binding");
    let mut bridge = bridge(
        FakeHost {
            answers: vec![
                HostOutcome::ApprovalRequired {
                    approval_token: "token-0123456789abcdef".to_string(),
                    review: json!({"summary": "reads the photo library"}),
                },
                HostOutcome::Ok(json!({"items": []})),
            ],
            seen: Vec::new(),
            reached: true,
        },
        &scratch,
    );

    bridge.handle(&request("r-1", "photos.list", json!({"limit": 1})));

    // A different input may not reuse the approval.
    let changed = json!({
        "version": 1,
        "request_id": "r-2",
        "capability": "photos.list",
        "input": {"limit": 2},
        "approval_token": "token-0123456789abcdef",
    })
    .to_string();
    let response = bridge.handle(&changed);
    assert_eq!(response.status, BridgeStatus::Denied);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "approval_mismatch"
    );

    // Another capability may not reuse it either.
    let other = json!({
        "version": 1,
        "request_id": "r-3",
        "capability": "shortcuts.invoke",
        "input": {"name": "morning"},
        "approval_token": "token-0123456789abcdef",
    })
    .to_string();
    let response = bridge.handle(&other);
    assert_eq!(response.status, BridgeStatus::Denied);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "approval_mismatch"
    );

    // A token the bridge never issued is refused as well.
    let forged = json!({
        "version": 1,
        "request_id": "r-4",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": "forged-token-0123456789",
    })
    .to_string();
    let response = bridge.handle(&forged);
    assert_eq!(response.status, BridgeStatus::Denied);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "approval_unknown"
    );
}

#[test]
fn an_approval_is_redeemed_once() {
    let scratch = scratch("approval-redeemed");
    let mut bridge = bridge(
        FakeHost {
            answers: vec![
                HostOutcome::ApprovalRequired {
                    approval_token: "token-0123456789abcdef".to_string(),
                    review: json!({"summary": "reads the photo library"}),
                },
                HostOutcome::Ok(json!({"items": []})),
                HostOutcome::Ok(json!({"items": []})),
            ],
            seen: Vec::new(),
            reached: true,
        },
        &scratch,
    );

    bridge.handle(&request("r-1", "photos.list", json!({"limit": 1})));

    let follow_up = json!({
        "version": 1,
        "request_id": "r-2",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": "token-0123456789abcdef",
    })
    .to_string();
    let response = bridge.handle(&follow_up);
    assert_eq!(response.status, BridgeStatus::Ok);

    // The token cannot be redeemed a second time.
    let replay = json!({
        "version": 1,
        "request_id": "r-3",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": "token-0123456789abcdef",
    })
    .to_string();
    let response = bridge.handle(&replay);
    assert_eq!(response.status, BridgeStatus::Denied);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "approval_used"
    );
}

#[test]
fn a_request_id_is_used_once() {
    let scratch = scratch("duplicate");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": []}))),
        &scratch,
    );

    let first = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(first.status, BridgeStatus::Ok);

    let second = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(second.status, BridgeStatus::Error);
    assert_eq!(
        second.error.expect("the request failed").reason,
        "duplicate_request_id"
    );
}

#[test]
fn a_request_id_is_used_once_across_invocations() {
    let scratch = scratch("duplicate-invocations");

    let mut first = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": []}))),
        &scratch,
    );
    let response = first.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Ok);

    // A second invocation over the same state remembers the request id.
    let mut second = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": []}))),
        &scratch,
    );
    let response = second.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "duplicate_request_id"
    );
}

#[test]
fn a_request_that_never_reached_the_host_keeps_its_id() {
    let scratch = scratch("unreachable-id");
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut bridge = Bridge::new(
        Box::new(adapter::UnavailableAdapter),
        state,
        AuditLog::new(Some(scratch.path.join("audit.log"))),
    );

    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Unavailable);

    // Nothing happened, so the caller may try again with the same id.
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut bridge = Bridge::new(
        Box::new(FakeHost::always(HostOutcome::Ok(json!({"items": []})))),
        state,
        AuditLog::new(Some(scratch.path.join("audit.log"))),
    );
    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Ok);
}

#[test]
fn denial_cancellation_timeout_and_host_failure_stay_distinct() {
    let cases: [(HostOutcome, BridgeStatus, &str); 4] = [
        (
            HostOutcome::Denied {
                reason: "user_refused".to_string(),
                message: "the user refused the request".to_string(),
            },
            BridgeStatus::Denied,
            "user_refused",
        ),
        (HostOutcome::Cancelled, BridgeStatus::Cancelled, "cancelled"),
        (HostOutcome::TimedOut, BridgeStatus::Error, "timeout"),
        (
            HostOutcome::Failed {
                reason: "host_crashed".to_string(),
                message: "the host died mid-answer".to_string(),
            },
            BridgeStatus::Error,
            "host_crashed",
        ),
    ];

    for (index, (outcome, status, reason)) in cases.into_iter().enumerate() {
        let scratch = scratch(&format!("lifecycle-{index}"));
        let mut bridge = bridge(FakeHost::always(outcome), &scratch);
        let response = bridge.handle(&request("r-1", "files.list", json!({})));
        assert_eq!(response.status, status);
        assert_eq!(response.error.expect("the request failed").reason, reason);
    }
}

#[test]
fn a_host_that_is_not_there_answers_unavailable() {
    let scratch = scratch("unavailable");
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut bridge = Bridge::new(
        Box::new(adapter::UnavailableAdapter),
        state,
        AuditLog::new(Some(scratch.path.join("audit.log"))),
    );
    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Unavailable);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "no_host_transport"
    );
}

#[test]
fn the_audit_summary_keeps_the_input_shape_and_not_its_values() {
    let scratch = scratch("audit");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::ApprovalRequired {
            approval_token: "token-0123456789abcdef".to_string(),
            review: json!({"summary": "reads the photo library"}),
        }),
        &scratch,
    );

    bridge.handle(&request(
        "r-1",
        "shortcuts.invoke",
        json!({"name": "morning", "input": {"token": "s3cr3t-photo-contents"}}),
    ));

    let log = audit_log(&scratch);
    assert!(log.contains("r-1"));
    assert!(log.contains("shortcuts.invoke"));
    assert!(log.contains("input_fields"));
    assert!(
        !log.contains("s3cr3t-photo-contents"),
        "the audit summary carried an input value: {log}"
    );
    assert!(
        !log.contains("morning"),
        "the audit summary carried an input value: {log}"
    );
}

#[test]
fn the_ledger_keeps_no_request_value() {
    let scratch = scratch("ledger");
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": []}))),
        &scratch,
    );

    bridge.handle(&request(
        "r-1",
        "shortcuts.invoke",
        json!({"name": "morning", "input": {"token": "s3cr3t-photo-contents"}}),
    ));

    let ledger = fs::read_to_string(scratch.path.join("state.json")).expect("read the ledger");
    assert!(ledger.contains("r-1"));
    assert!(
        !ledger.contains("s3cr3t-photo-contents"),
        "the ledger carried an input value: {ledger}"
    );
    assert!(
        !ledger.contains("morning"),
        "the ledger carried an input value: {ledger}"
    );
}

#[test]
fn the_exchange_sends_the_request_and_reads_the_answer() {
    let request = host_request();
    let mut connection =
        FakeConnection::answering(vec![reply(&request, "ok", json!({"items": ["a.jpg"]}))]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    assert_eq!(outcome, HostOutcome::Ok(json!({"items": ["a.jpg"]})));
    assert_eq!(connection.sent.len(), 1);
    let sent: Value = serde_json::from_str(&connection.sent[0]).expect("the frame is JSON");
    assert_eq!(sent["request_id"], "r-1");
    assert_eq!(sent["nonce"], request.nonce);
    assert_eq!(sent["capability"], "files.list");
}

#[test]
fn the_exchange_reports_a_host_that_closed_the_connection() {
    let request = host_request();
    let mut connection = FakeConnection::answering(Vec::new());

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_crashed"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_reports_an_answer_that_belongs_to_another_request() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec![
        json!({
            "version": 1,
            "nonce": "another-nonce",
            "request_id": "r-9",
            "status": "ok",
            "result": {},
            "error": null,
        })
        .to_string(),
    ]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_reports_an_answer_in_another_version() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec![
        json!({
            "version": 2,
            "nonce": request.nonce,
            "request_id": request.request_id,
            "status": "ok",
            "result": {},
            "error": null,
        })
        .to_string(),
    ]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_reports_an_answer_it_cannot_read() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec!["{not json".to_string()]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_crashed"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_times_out_when_the_host_stays_silent() {
    let request = host_request();
    let mut connection = FakeConnection::silent();

    let started = Instant::now();
    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_millis(200),
    );

    assert_eq!(outcome, HostOutcome::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn the_exchange_answers_the_hosts_approval_with_a_review_summary() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec![reply(
        &request,
        "approval_required",
        json!({
            "approval_token": "token-0123456789abcdef",
            "review": {"summary": "reads the photo library"},
        }),
    )]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::ApprovalRequired {
            approval_token,
            review,
        } => {
            assert_eq!(approval_token, "token-0123456789abcdef");
            assert_eq!(review["summary"], "reads the photo library");
        }
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_refuses_an_answer_that_is_not_text() {
    let request = host_request();
    let mut connection = FakeConnection::answering_bytes(vec![vec![0xff, 0xfe, b'}']]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_crashed"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_refuses_an_answer_that_is_too_large() {
    let request = host_request();
    let mut connection =
        FakeConnection::answering_bytes(vec![vec![b'x'; adapter::MAX_HOST_FRAME_BYTES + 1]]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_refuses_an_approval_without_a_review_summary() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec![reply(
        &request,
        "approval_required",
        json!({"approval_token": "token-0123456789abcdef"}),
    )]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_refuses_an_approval_with_an_unreviewable_summary() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec![reply(
        &request,
        "approval_required",
        json!({
            "approval_token": "token-0123456789abcdef",
            "review": 42,
        }),
    )]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_refuses_an_approval_with_a_huge_summary() {
    let request = host_request();
    let mut connection = FakeConnection::answering(vec![reply(
        &request,
        "approval_required",
        json!({
            "approval_token": "token-0123456789abcdef",
            "review": {"summary": "x".repeat(protocol::MAX_REVIEW_BYTES + 1)},
        }),
    )]);

    let outcome = adapter::exchange(
        &mut connection,
        &request,
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn the_exchange_refuses_an_approval_with_an_empty_review_summary() {
    for review in [json!(""), json!("   "), json!({}), json!({"summary": ""})] {
        let request = host_request();
        let mut connection = FakeConnection::answering(vec![reply(
            &request,
            "approval_required",
            json!({
                "approval_token": "token-0123456789abcdef",
                "review": review,
            }),
        )]);

        let outcome = adapter::exchange(
            &mut connection,
            &request,
            Instant::now() + Duration::from_secs(5),
        );

        match outcome {
            HostOutcome::Failed { reason, .. } => assert_eq!(reason, "host_protocol_violation"),
            other => panic!("the outcome was {other:?}"),
        }
    }
}

#[test]
fn a_write_that_runs_past_the_deadline_is_answered_timeout() {
    let mut connection = FakeConnection::failing(ConnectionError::Timeout);

    let outcome = adapter::exchange(
        &mut connection,
        &host_request(),
        Instant::now() + Duration::from_secs(5),
    );

    assert_eq!(outcome, HostOutcome::TimedOut);
}

#[test]
fn a_write_that_was_cancelled_is_answered_cancelled() {
    let mut connection = FakeConnection::failing(ConnectionError::Cancelled);

    let outcome = adapter::exchange(
        &mut connection,
        &host_request(),
        Instant::now() + Duration::from_secs(5),
    );

    assert_eq!(outcome, HostOutcome::Cancelled);
}

#[test]
fn a_broken_connection_is_answered_transport_error() {
    let mut connection =
        FakeConnection::failing(ConnectionError::Io("the socket broke".to_string()));

    let outcome = adapter::exchange(
        &mut connection,
        &host_request(),
        Instant::now() + Duration::from_secs(5),
    );

    match outcome {
        HostOutcome::Failed { reason, .. } => assert_eq!(reason, "transport_error"),
        other => panic!("the outcome was {other:?}"),
    }
}

#[test]
fn a_request_that_never_connected_keeps_its_id() {
    for outcome in [HostOutcome::TimedOut, HostOutcome::Cancelled] {
        let scratch = scratch("never-connected");
        let mut first = bridge(FakeHost::never_reached(outcome.clone()), &scratch);

        let response = first.handle(&request("r-1", "files.list", json!({})));
        assert_ne!(response.status, BridgeStatus::Ok);

        // Nothing reached the host, so the caller may try again with the same
        // request id.
        let mut second = bridge(
            FakeHost::always(HostOutcome::Ok(json!({"items": []}))),
            &scratch,
        );
        let response = second.handle(&request("r-1", "files.list", json!({})));
        assert_eq!(
            response.status,
            BridgeStatus::Ok,
            "the request id was not free again: {response:?}"
        );
    }
}

#[test]
fn a_ledger_with_an_unknown_nested_field_is_refused() {
    let scratch = scratch("nested-field");
    fs::write(
        scratch.path.join("state.json"),
        r#"{"schema":"leg.bridge.state/v1","requests":{"r-1":{"input_sha256":"aa",
           "terminal_state":"ok","issued_at":1,"extra":"surprise"}}}"#,
    )
    .expect("write the ledger");

    let error = BridgeState::load(&scratch.path).expect_err("the ledger was read");
    assert!(
        error.contains("unknown field"),
        "the refusal did not name the unknown field: {error}"
    );
}

#[test]
fn a_ledger_with_a_record_that_is_not_the_documented_shape_is_refused() {
    let scratch = scratch("record-shape");
    fs::write(
        scratch.path.join("state.json"),
        r#"{"schema":"leg.bridge.state/v1","requests":{"r-1":{"input_sha256":"not-a-digest",
           "terminal_state":"ok","issued_at":1}}}"#,
    )
    .expect("write the ledger");

    let error = BridgeState::load(&scratch.path).expect_err("the ledger was read");
    assert!(
        error.contains("digest"),
        "the refusal did not name the record: {error}"
    );
}

#[test]
fn a_ledger_with_an_unknown_request_id_is_refused() {
    let scratch = scratch("record-id");
    fs::write(
        scratch.path.join("state.json"),
        r#"{"schema":"leg.bridge.state/v1","requests":{"not a request id":
           {"input_sha256":"aa","terminal_state":"ok","issued_at":1}}}"#,
    )
    .expect("write the ledger");

    let error = BridgeState::load(&scratch.path).expect_err("the ledger was read");
    assert!(
        error.contains("request id"),
        "the refusal did not name the record: {error}"
    );
}

#[test]
fn a_ledger_that_would_grow_past_its_limit_is_not_published() {
    let scratch = scratch("oversized-save");
    let mut state = BridgeState::load(&scratch.path).expect("load the bridge state");
    state.requests.insert(
        "r-1".to_string(),
        RequestRecord {
            input_sha256: "x".repeat(200 * 1024),
            terminal_state: "ok".to_string(),
            issued_at: 1,
        },
    );

    let error = state
        .save()
        .expect_err("the oversized ledger was published");
    assert!(
        error.contains("limit"),
        "the refusal did not name the limit: {error}"
    );
    assert!(
        !scratch.path.join("state.json").exists(),
        "the oversized ledger reached the disk"
    );
}

#[test]
fn an_answer_that_grew_past_the_limit_is_replaced_by_a_fixed_one() {
    let scratch = scratch("huge-answer");
    // The result is under the result limit and over the response limit, so
    // only the final check can catch it.
    let mut bridge = bridge(
        FakeHost::always(HostOutcome::Ok(json!({"items": ["x".repeat(65_500)]}))),
        &scratch,
    );

    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.as_ref().expect("the request failed").reason,
        "response_too_large"
    );
    assert!(response.serialized_len() <= protocol::MAX_RESPONSE_BYTES);
}

#[test]
fn a_ledger_that_speaks_another_schema_is_refused() {
    let scratch = scratch("foreign-state");
    let mut bridge = bridge(FakeHost::always(HostOutcome::Ok(json!({}))), &scratch);
    // The ledger changed under the bridge: it is not the file the bridge
    // wrote, and the bridge refuses it rather than reading it as its own.
    fs::write(
        scratch.path.join("state.json"),
        r#"{"schema":"somebody.elses.state/v9","requests":{}}"#,
    )
    .expect("write the foreign state");

    let response = bridge.handle(&request("r-1", "files.list", json!({})));

    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.as_ref().expect("the request failed").reason,
        "state_unavailable"
    );
}

#[test]
fn a_ledger_that_grew_past_its_limit_is_refused() {
    let scratch = scratch("huge-state");
    let mut bridge = bridge(FakeHost::always(HostOutcome::Ok(json!({}))), &scratch);
    fs::write(
        scratch.path.join("state.json"),
        format!(
            r#"{{"schema":"leg.bridge.state/v1","requests":{{}},"approvals":{{}},"pad":"{}"}}"#,
            "x".repeat(200 * 1024)
        ),
    )
    .expect("write the oversized state");

    let response = bridge.handle(&request("r-1", "files.list", json!({})));

    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.as_ref().expect("the request failed").reason,
        "state_unavailable"
    );
}

#[test]
fn a_request_that_finds_the_state_busy_times_out() {
    let scratch = scratch("busy-state");
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let _held = state
        .lock_until(Instant::now() + Duration::from_secs(10))
        .expect("take the ledger lock");

    let mut bridge = bridge(FakeHost::always(HostOutcome::Ok(json!({}))), &scratch);
    let mut line = request("r-1", "files.list", json!({}));
    line = line.replace(r#""input":{}"#, r#""input":{},"timeout_ms":100"#);

    let response = bridge.handle(&line);
    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.as_ref().expect("the request failed").reason,
        "timeout"
    );
}

/// A host that breaks the ledger while the request is in flight.
struct LedgerBreakingHost {
    /// The ledger file the host takes away.
    path: PathBuf,
}

impl HostAdapter for LedgerBreakingHost {
    fn dispatch(&mut self, _request: &HostRequest, _deadline: Instant) -> HostExchange {
        // The ledger cannot be written any more: a directory stands where the
        // ledger file belongs.
        let _ = fs::remove_file(&self.path);
        let _ = fs::create_dir_all(&self.path);
        HostExchange {
            outcome: HostOutcome::Ok(json!({"items": []})),
            reached: true,
        }
    }
}

#[test]
fn a_ledger_that_cannot_be_written_after_the_host_answered_is_a_recorded_failure() {
    let scratch = scratch("broken-ledger");
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut bridge = Bridge::new(
        Box::new(LedgerBreakingHost {
            path: scratch.path.join("state.json"),
        }),
        state,
        AuditLog::new(None),
    );

    let response = bridge.handle(&request("r-1", "files.list", json!({})));

    assert_eq!(response.status, BridgeStatus::Error);
    let error = response.error.expect("the request failed");
    assert_eq!(error.reason, "state_unavailable");
    assert!(
        error.message.contains("reached the host"),
        "the failure did not say that the request ran: {error:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_host_socket_that_is_not_there_is_unreachable() {
    let scratch = scratch("no-socket");
    let mut adapter = adapter::SocketHostAdapter::new(scratch.path.join("nothing.sock"));
    let request = host_request();

    let exchange = adapter.dispatch(&request, Instant::now() + Duration::from_secs(5));

    match exchange.outcome {
        HostOutcome::Unavailable { reason, .. } => assert_eq!(reason, "host_unreachable"),
        other => panic!("the outcome was {other:?}"),
    }
    assert!(
        !exchange.reached,
        "the request reached a host that is not there"
    );
}

#[cfg(unix)]
#[test]
fn a_host_socket_that_cannot_be_reached_in_time_times_out() {
    let scratch = scratch("late-socket");
    let mut adapter = adapter::SocketHostAdapter::new(scratch.path.join("nothing.sock"));
    let request = host_request();

    let exchange = adapter.dispatch(&request, Instant::now() - Duration::from_secs(1));

    assert_eq!(exchange.outcome, HostOutcome::TimedOut);
    assert!(
        !exchange.reached,
        "the request reached a host it never connected to"
    );
}
