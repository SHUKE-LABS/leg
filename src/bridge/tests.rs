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

#[cfg(unix)]
#[test]
fn a_write_to_a_real_socket_that_stalls_times_out() {
    use std::os::unix::net::UnixListener;
    use std::thread;

    let scratch = scratch("write-stall");
    let path = scratch.path.join("host.sock");
    let listener = UnixListener::bind(&path).expect("bind the host socket");

    // The host takes the connection and never reads from it, so the socket
    // buffer fills and the write stalls.
    let host = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(30));
            drop(stream);
        }
    });

    let error = adapter::write_over_a_socket(
        &path,
        4 * 1024 * 1024,
        Instant::now() + Duration::from_millis(300),
    )
    .expect_err("the write went through");

    assert_eq!(error, ConnectionError::Timeout);
    drop(host);
}

#[cfg(unix)]
#[test]
fn a_write_to_a_real_socket_whose_host_is_gone_is_a_transport_error() {
    use std::os::unix::net::UnixListener;
    use std::thread;

    let scratch = scratch("write-broken");
    let path = scratch.path.join("host.sock");
    let listener = UnixListener::bind(&path).expect("bind the host socket");

    // The host takes the connection and closes it again.
    let host = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });

    let error = adapter::write_over_a_socket(
        &path,
        4 * 1024 * 1024,
        Instant::now() + Duration::from_secs(30),
    )
    .expect_err("the write went through");

    assert!(
        matches!(error, ConnectionError::Io(_)),
        "the broken write was not a transport failure: {error:?}"
    );
    drop(host);
}

#[test]
fn a_request_to_a_host_that_never_connected_keeps_its_id() {
    let scratch = scratch("unreachable-retry");
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut first = Bridge::new(
        Box::new(adapter::SocketHostAdapter::new(
            scratch.path.join("nothing.sock"),
        )),
        state,
        AuditLog::new(None),
    );

    let response = first.handle(&request("r-1", "files.list", json!({})));
    assert_eq!(response.status, BridgeStatus::Unavailable);

    // The transport never connected, so nothing happened and the caller may
    // try again with the same request id.
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

/// The environment variable that turns a test into the child that writes
/// over a real socket while it is cancelled.
#[cfg(unix)]
const INTERRUPT_CHILD_ENV: &str = "LEG_BRIDGE_TEST_INTERRUPT_CHILD";

/// The environment variable that names the socket the child writes to.
#[cfg(unix)]
const INTERRUPT_CHILD_SOCKET_ENV: &str = "LEG_BRIDGE_TEST_INTERRUPT_SOCKET";

/// The environment variable that names the file the child writes its result to.
#[cfg(unix)]
const INTERRUPT_CHILD_RESULT_ENV: &str = "LEG_BRIDGE_TEST_INTERRUPT_RESULT";

/// The name of the test that runs as the child.
#[cfg(unix)]
const INTERRUPT_CHILD_TEST: &str = "bridge::tests::the_write_over_a_real_socket_is_cancelled";

#[cfg(unix)]
#[test]
fn a_write_over_a_real_socket_that_is_cancelled_answers_cancelled() {
    use std::os::unix::net::UnixListener;
    use std::process::Command;

    let scratch = scratch("write-cancelled");
    let path = scratch.path.join("host.sock");
    // The host takes no connection and reads nothing: the socket buffer
    // fills and the write stalls.
    let _listener = UnixListener::bind(&path).expect("bind the host socket");
    let result = scratch.path.join("result.txt");

    // The interrupt flag is process-global, so the child raises it in a
    // process of its own; it never reaches the tests beside this one.
    let status = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", INTERRUPT_CHILD_TEST, "--nocapture"])
        .env(INTERRUPT_CHILD_ENV, "1")
        .env(INTERRUPT_CHILD_SOCKET_ENV, &path)
        .env(INTERRUPT_CHILD_RESULT_ENV, &result)
        .status()
        .expect("the child ran");

    assert!(status.success(), "the child failed: {status}");
    let answer = fs::read_to_string(&result).expect("the child wrote its result");
    assert_eq!(
        answer.trim(),
        "cancelled",
        "the stalled write was not cancelled: {answer}"
    );
}

#[cfg(unix)]
#[test]
fn the_write_over_a_real_socket_is_cancelled() {
    if std::env::var_os(INTERRUPT_CHILD_ENV).is_none() {
        // The parent runs this test in a child process; in the parent there
        // is nothing to do.
        return;
    }
    let path = std::env::var_os(INTERRUPT_CHILD_SOCKET_ENV).expect("the socket path");
    let result = std::env::var_os(INTERRUPT_CHILD_RESULT_ENV).expect("the result path");

    // The interrupt arrives while the write is stalled.
    let interrupt = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        crate::interrupt::raise_for_tests();
    });

    let error = adapter::write_over_a_socket(
        std::path::Path::new(&path),
        4 * 1024 * 1024,
        Instant::now() + Duration::from_secs(30),
    )
    .expect_err("the write went through");
    interrupt.join().expect("the interrupt thread finished");

    let answer = match error {
        ConnectionError::Cancelled => "cancelled",
        ConnectionError::Timeout => "timeout",
        ConnectionError::Io(_) => "transport_error",
        _ => "other",
    };
    fs::write(&result, answer).expect("write the result");
}

/// A host that answers in order and lets the test read what it received.
struct RecordingHost {
    /// The answers the host gives, in order; the last one repeats.
    answers: Vec<HostOutcome>,
    /// The requests the host received.
    seen: Arc<Mutex<Vec<HostRequest>>>,
}

impl HostAdapter for RecordingHost {
    fn dispatch(&mut self, request: &HostRequest, _deadline: Instant) -> HostExchange {
        self.seen
            .lock()
            .expect("the host lock")
            .push(request.clone());
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
            reached: true,
        }
    }
}

/// A bridge over a recording host, and the requests the host received.
fn recording_bridge(
    answers: Vec<HostOutcome>,
    scratch: &Scratch,
) -> (Bridge, Arc<Mutex<Vec<HostRequest>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let bridge = Bridge::new(
        Box::new(RecordingHost {
            answers,
            seen: Arc::clone(&seen),
        }),
        state,
        AuditLog::new(None),
    );
    (bridge, seen)
}

/// The approval token the fake hosts issue.
const TOKEN: &str = "token-0123456789abcdef";

/// A digest the fake hosts return.
fn checksum() -> String {
    "ab".repeat(32)
}

/// The host's request for an approval.
fn approval_required() -> HostOutcome {
    HostOutcome::ApprovalRequired {
        approval_token: TOKEN.to_string(),
        review: json!({"summary": "copies one video to the guest workspace"}),
    }
}

/// A `photos.search` page of `count` videos, longest first.
fn search_page(count: usize, truncated: bool) -> Value {
    let assets: Vec<Value> = (0..count)
        .map(|index| {
            json!({
                "asset_id": format!("asset-{index:03}"),
                "media_type": "video",
                "duration_ms": 1_000_000 - index as u64,
                "created_at": "2025-06-01T12:00:00Z",
                "width": 1920,
                "height": 1080,
            })
        })
        .collect();
    json!({"authorization": "full", "assets": assets, "truncated": truncated})
}

/// The `photos.export` result for `destination`.
fn photos_export_result(destination: &str) -> Value {
    let relative_path = if destination == "files_picker" {
        Value::Null
    } else {
        json!("c0ffee/clip.mov")
    };
    json!({
        "authorization": "limited",
        "copy_id": "c0ffee",
        "relative_path": relative_path,
        "name": "clip.mov",
        "media_type": "video",
        "size_bytes": 1024,
        "sha256": checksum(),
    })
}

/// The `files.import` result.
fn files_import_result() -> Value {
    json!({
        "copy_id": "c0ffee",
        "relative_path": "c0ffee/notes.txt",
        "name": "notes.txt",
        "content_type": "text/plain",
        "size_bytes": 12,
        "sha256": checksum(),
    })
}

/// The `files.export` result.
fn files_export_result() -> Value {
    json!({
        "export_id": "e-1",
        "name": "notes.txt",
        "content_type": "text/plain",
        "size_bytes": 12,
        "sha256": checksum(),
    })
}

/// A request line that replays the approval token.
fn continuation(id: &str, capability: &str, input: Value) -> String {
    json!({
        "version": 1,
        "request_id": id,
        "capability": capability,
        "input": input,
        "approval_token": TOKEN,
    })
    .to_string()
}

/// The reason of a failed answer.
fn reason(response: &BridgeResponse) -> String {
    response
        .error
        .as_ref()
        .map(|error| error.reason.clone())
        .unwrap_or_default()
}

#[test]
fn the_photos_and_files_capabilities_carry_their_approval_flags() {
    let flags = [
        ("photos.search", false),
        ("photos.export", true),
        ("files.import", false),
        ("files.export", true),
    ];
    for (name, requires_approval) in flags {
        let capability = protocol::capability(name).expect("the capability is known");
        assert_eq!(capability.requires_approval, requires_approval, "{name}");
    }
}

#[test]
fn a_capability_without_an_approval_runs_without_a_token() {
    let cases = [
        (
            "photos.search",
            json!({"filter": {"media_type": "video"}, "sort": "duration_desc", "limit": 2}),
            search_page(2, false),
        ),
        (
            "files.import",
            json!({"destination": "guest_workspace"}),
            files_import_result(),
        ),
    ];
    for (index, (capability, input, result)) in cases.into_iter().enumerate() {
        let scratch = scratch(&format!("no-approval-{index}"));
        let (mut bridge, seen) = recording_bridge(vec![HostOutcome::Ok(result.clone())], &scratch);
        let response = bridge.handle(&request("r-1", capability, input));
        assert_eq!(
            response.status,
            BridgeStatus::Ok,
            "{capability}: {response:?}"
        );
        assert_eq!(response.result, Some(result));
        assert!(seen.lock().unwrap()[0].approval_token.is_none());
    }
}

#[test]
fn a_preview_the_host_ran_is_a_protocol_violation() {
    let cases = [
        (
            "photos.export",
            json!({"asset_id": "A1", "destination": "guest_workspace"}),
            photos_export_result("guest_workspace"),
        ),
        (
            "files.export",
            json!({"copy_id": "c0ffee", "relative_path": "c0ffee/notes.txt", "destination": "files_picker"}),
            files_export_result(),
        ),
    ];
    for (index, (capability, input, result)) in cases.into_iter().enumerate() {
        let scratch = scratch(&format!("preview-ok-{index}"));
        let (mut bridge, _) = recording_bridge(vec![HostOutcome::Ok(result)], &scratch);
        let response = bridge.handle(&request("r-1", capability, input.clone()));
        assert_eq!(response.status, BridgeStatus::Error, "{capability}");
        assert_eq!(reason(&response), "host_protocol_violation", "{capability}");
        assert!(response.result.is_none());
        assert!(
            bridge.state().approvals.is_empty(),
            "a preview the host ran recorded an approval"
        );

        // A preview the host answers with an approval request is the
        // contract.
        let scratch = self::scratch(&format!("preview-approval-{index}"));
        let (mut bridge, _) = recording_bridge(vec![approval_required()], &scratch);
        let response = bridge.handle(&request("r-1", capability, input));
        assert_eq!(
            response.status,
            BridgeStatus::ApprovalRequired,
            "{capability}"
        );
    }
}

#[test]
fn the_photos_and_files_inputs_are_exact() {
    let video = json!({"media_type": "video"});
    let refused: Vec<(&str, Value, &str)> = vec![
        ("photos.search", json!({"filter": video}), "missing_field"),
        (
            "photos.search",
            json!({"sort": "duration_desc"}),
            "missing_field",
        ),
        (
            "photos.search",
            json!({"filter": {}, "sort": "duration_desc"}),
            "missing_field",
        ),
        (
            "photos.search",
            json!({"filter": video, "sort": "duration_desc", "page": 1}),
            "unknown_field",
        ),
        (
            "photos.search",
            json!({"filter": {"media_type": "video", "album": "x"}, "sort": "duration_desc"}),
            "unknown_field",
        ),
        (
            "photos.search",
            json!({"filter": [], "sort": "duration_desc"}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": {"media_type": "photo"}, "sort": "duration_desc"}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": video, "sort": "size_desc"}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": video, "sort": "duration_desc", "limit": 0}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": video, "sort": "duration_desc", "limit": 101}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": video, "sort": "duration_desc", "limit": 1.5}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": {"media_type": "video", "created_after": "yesterday"}, "sort": "created_desc"}),
            "invalid_input",
        ),
        (
            "photos.search",
            json!({"filter": {"media_type": "video", "created_after": "2025-01-02T00:00:00Z", "created_before": "2025-01-01T00:00:00Z"}, "sort": "created_desc"}),
            "invalid_input",
        ),
        ("photos.export", json!({"asset_id": "A1"}), "missing_field"),
        (
            "photos.export",
            json!({"asset_id": "", "destination": "guest_workspace"}),
            "invalid_input",
        ),
        (
            "photos.export",
            json!({"asset_id": "a".repeat(129), "destination": "guest_workspace"}),
            "invalid_input",
        ),
        (
            "photos.export",
            json!({"asset_id": "A\n1", "destination": "guest_workspace"}),
            "invalid_input",
        ),
        (
            "photos.export",
            json!({"asset_id": "A1", "destination": "camera_roll"}),
            "invalid_input",
        ),
        (
            "photos.export",
            json!({"asset_id": "A1", "destination": "guest_workspace", "filename": "../x.mov"}),
            "invalid_path",
        ),
        (
            "photos.export",
            json!({"asset_id": "A1", "destination": "guest_workspace", "filename": ".hidden"}),
            "invalid_path",
        ),
        (
            "photos.export",
            json!({"asset_id": "A1", "destination": "guest_workspace", "share": true}),
            "unknown_field",
        ),
        ("files.import", json!({}), "missing_field"),
        (
            "files.import",
            json!({"destination": "files_picker"}),
            "invalid_input",
        ),
        (
            "files.import",
            json!({"destination": "guest_workspace", "path": "/etc"}),
            "unknown_field",
        ),
        (
            "files.export",
            json!({"copy_id": "C0FFEE", "relative_path": "C0FFEE/a.txt", "destination": "files_picker"}),
            "invalid_input",
        ),
        (
            "files.export",
            json!({"copy_id": "c0ffee", "relative_path": "other/a.txt", "destination": "files_picker"}),
            "invalid_path",
        ),
        (
            "files.export",
            json!({"copy_id": "c0ffee", "relative_path": "c0ffee/../a.txt", "destination": "files_picker"}),
            "invalid_path",
        ),
        (
            "files.export",
            json!({"copy_id": "c0ffee", "relative_path": format!("c0ffee/{}", "a".repeat(129)), "destination": "files_picker"}),
            "invalid_path",
        ),
        (
            "files.export",
            json!({"copy_id": "c0ffee", "relative_path": "c0ffee/a.txt", "destination": "guest_workspace"}),
            "invalid_input",
        ),
        (
            "files.export",
            json!({"copy_id": "c0ffee", "relative_path": "c0ffee/a.txt", "destination": "files_picker", "filename": "a:b"}),
            "invalid_path",
        ),
    ];
    for (capability, input, expected) in refused {
        let line = request("r-1", capability, input.clone());
        let error = protocol::validate_request(&serde_json::from_str::<Value>(&line).unwrap())
            .expect_err(&format!("{capability} accepted {input}"));
        assert_eq!(error.reason, expected, "{capability} {input}: {error:?}");
    }

    // The bounds are inclusive.
    let accepted: Vec<(&str, Value)> = vec![
        (
            "photos.search",
            json!({"filter": video, "sort": "duration_desc", "limit": 1}),
        ),
        (
            "photos.search",
            json!({"filter": video, "sort": "created_desc", "limit": 100}),
        ),
        (
            "photos.search",
            json!({"filter": {"media_type": "video", "created_after": "2025-01-01T08:00:00+08:00", "created_before": "2025-01-01T00:00:00Z"}, "sort": "created_desc"}),
        ),
        (
            "photos.export",
            json!({"asset_id": "a".repeat(128), "destination": "files_picker", "filename": "x".repeat(128)}),
        ),
        (
            "files.export",
            json!({"copy_id": "a".repeat(64), "relative_path": format!("{}/{}", "a".repeat(64), "b".repeat(128)), "destination": "files_picker"}),
        ),
    ];
    for (capability, input) in accepted {
        let line = request("r-1", capability, input.clone());
        protocol::validate_request(&serde_json::from_str::<Value>(&line).unwrap())
            .unwrap_or_else(|error| panic!("{capability} refused {input}: {error:?}"));
    }
}

#[test]
fn an_approval_continues_with_reordered_input_keys() {
    let scratch = scratch("reordered");
    let (mut bridge, seen) = recording_bridge(
        vec![
            approval_required(),
            HostOutcome::Ok(photos_export_result("guest_workspace")),
        ],
        &scratch,
    );

    let preview = bridge.handle(
        r#"{"version":1,"request_id":"r-1","capability":"photos.export","input":{"asset_id":"A1","destination":"guest_workspace"}}"#,
    );
    assert_eq!(preview.status, BridgeStatus::ApprovalRequired);

    let response = bridge.handle(&format!(
        r#"{{"version":1,"request_id":"r-2","capability":"photos.export","input":{{"destination":"guest_workspace","asset_id":"A1"}},"approval_token":"{TOKEN}"}}"#
    ));
    assert_eq!(response.status, BridgeStatus::Ok, "{response:?}");

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].input_sha256, seen[1].input_sha256);
    assert_eq!(
        seen[0].input_sha256,
        protocol::sha256_hex(br#"{"asset_id":"A1","destination":"guest_workspace"}"#)
    );
}

#[test]
fn the_host_receives_the_canonical_digest_of_the_input() {
    let scratch = scratch("canonical-digest");
    let (mut bridge, seen) =
        recording_bridge(vec![HostOutcome::Ok(search_page(0, false))], &scratch);
    let response = bridge.handle(
        r#"{"version":1,"request_id":"r-1","capability":"photos.search","input":{"sort":"duration_desc","filter":{"media_type":"video"},"limit":20}}"#,
    );
    assert_eq!(response.status, BridgeStatus::Ok, "{response:?}");
    assert_eq!(
        seen.lock().unwrap()[0].input_sha256,
        "a9a83ee45388e790690d5f5683782ecf738d5f703189c18b139a01f29c8bce4a"
    );
}

#[test]
fn an_approval_refuses_a_changed_value_a_duplicate_key_and_a_bad_number() {
    let scratch = scratch("approval-refusals");
    let (mut bridge, seen) = recording_bridge(
        vec![
            approval_required(),
            HostOutcome::Ok(photos_export_result("guest_workspace")),
        ],
        &scratch,
    );
    let input = json!({"asset_id": "A1", "destination": "guest_workspace"});
    let preview = bridge.handle(&request("r-1", "photos.export", input));
    assert_eq!(preview.status, BridgeStatus::ApprovalRequired);

    let changed = bridge.handle(&continuation(
        "r-2",
        "photos.export",
        json!({"asset_id": "A2", "destination": "guest_workspace"}),
    ));
    assert_eq!(changed.status, BridgeStatus::Denied);
    assert_eq!(reason(&changed), "approval_mismatch");

    let duplicate = bridge.handle(&format!(
        r#"{{"version":1,"request_id":"r-3","capability":"photos.export","input":{{"asset_id":"A2","asset_id":"A1","destination":"guest_workspace"}},"approval_token":"{TOKEN}"}}"#
    ));
    assert_eq!(reason(&duplicate), "malformed_request");

    // The second name is `asset_id` once its escape is undone.
    let escaped_name = format!("{}u0061sset_id", '\\');
    let escaped = bridge.handle(&format!(
        r#"{{"version":1,"request_id":"r-4","capability":"photos.export","input":{{"asset_id":"A1","{escaped_name}":"A1","destination":"guest_workspace"}},"approval_token":"{TOKEN}"}}"#
    ));
    assert_eq!(reason(&escaped), "malformed_request");

    let number = bridge.handle(
        r#"{"version":1,"request_id":"r-5","capability":"photos.search","input":{"filter":{"media_type":"video"},"sort":"duration_desc","limit":1e400}}"#,
    );
    assert_eq!(reason(&number), "malformed_request");

    // A digest the caller supplies is not a field a request may carry: the
    // bridge computes it.
    let supplied = bridge.handle(&format!(
        r#"{{"version":1,"request_id":"r-6","capability":"photos.export","input":{{"asset_id":"A1","destination":"guest_workspace"}},"input_sha256":"{}","approval_token":"{TOKEN}"}}"#,
        "0".repeat(64)
    ));
    assert_eq!(reason(&supplied), "unknown_field");

    // Only the preview reached the host.
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn an_approval_expires_after_six_hundred_seconds() {
    use super::state::ApprovalRecord;

    let record = ApprovalRecord {
        capability: "photos.export".to_string(),
        input_sha256: "0".repeat(64),
        token_sha256: "1".repeat(64),
        issued_at: 1_000,
        expires_at: 1_600,
        redeemed: false,
    };
    assert!(BridgeState::approval_is_fresh_at(&record, 1_000));
    assert!(BridgeState::approval_is_fresh_at(&record, 1_600));
    assert!(!BridgeState::approval_is_fresh_at(&record, 1_601));

    let scratch = scratch("approval-expiry");
    let (mut bridge, seen) = recording_bridge(
        vec![
            approval_required(),
            HostOutcome::Ok(photos_export_result("guest_workspace")),
        ],
        &scratch,
    );
    let input = json!({"asset_id": "A1", "destination": "guest_workspace"});
    bridge.handle(&request("r-1", "photos.export", input.clone()));

    // The approval was issued 601 seconds ago.
    let mut state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(state.approvals.len(), 1);
    for record in state.approvals.values_mut() {
        assert_eq!(record.expires_at, record.issued_at + 600);
        record.issued_at = now - 601;
        record.expires_at = now - 1;
    }
    state.save().expect("save the bridge state");

    let response = bridge.handle(&continuation("r-2", "photos.export", input));
    assert_eq!(response.status, BridgeStatus::Denied);
    assert_eq!(reason(&response), "approval_expired");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the expired token reached the host"
    );
}

#[test]
fn an_approval_is_redeemed_by_a_success_or_a_denial() {
    let outcomes = [
        HostOutcome::Ok(files_export_result()),
        HostOutcome::Denied {
            reason: "user_refused".to_string(),
            message: "the user refused".to_string(),
        },
    ];
    let input = json!({"copy_id": "c0ffee", "relative_path": "c0ffee/notes.txt", "destination": "files_picker"});
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let scratch = scratch(&format!("redeem-{index}"));
        let (mut bridge, seen) = recording_bridge(vec![approval_required(), outcome], &scratch);
        bridge.handle(&request("r-1", "files.export", input.clone()));
        bridge.handle(&continuation("r-2", "files.export", input.clone()));

        let replay = bridge.handle(&continuation("r-3", "files.export", input.clone()));
        assert_eq!(replay.status, BridgeStatus::Denied);
        assert_eq!(reason(&replay), "approval_used");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "the redeemed token reached the host"
        );
    }
}

#[test]
fn a_continuation_without_a_decision_invalidates_its_token() {
    let outcomes = [
        HostOutcome::Cancelled,
        HostOutcome::TimedOut,
        HostOutcome::Failed {
            reason: "io_failure".to_string(),
            message: "the copy failed".to_string(),
        },
    ];
    let input = json!({"asset_id": "A1", "destination": "files_picker"});
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let scratch = scratch(&format!("invalidate-{index}"));
        let (mut bridge, seen) = recording_bridge(vec![approval_required(), outcome], &scratch);
        bridge.handle(&request("r-1", "photos.export", input.clone()));
        let interrupted = bridge.handle(&continuation("r-2", "photos.export", input.clone()));
        assert_ne!(interrupted.status, BridgeStatus::Ok);

        let retry = bridge.handle(&continuation("r-3", "photos.export", input.clone()));
        assert_eq!(retry.status, BridgeStatus::Denied);
        assert_eq!(reason(&retry), "approval_unknown");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "the invalidated token reached the host"
        );
    }
}

#[test]
fn a_search_page_is_accepted_at_every_size() {
    let cases = [
        (1, search_page(1, false)),
        (1, search_page(1, true)),
        (1, search_page(0, false)),
        (100, search_page(100, false)),
        (100, search_page(100, true)),
        (20, search_page(7, false)),
    ];
    for (index, (limit, page)) in cases.into_iter().enumerate() {
        let scratch = scratch(&format!("search-page-{index}"));
        let (mut bridge, _) = recording_bridge(vec![HostOutcome::Ok(page)], &scratch);
        let response = bridge.handle(&request(
            "r-1",
            "photos.search",
            json!({"filter": {"media_type": "video"}, "sort": "duration_desc", "limit": limit}),
        ));
        assert_eq!(
            response.status,
            BridgeStatus::Ok,
            "case {index}: {response:?}"
        );
    }

    // Equal durations fall back to the asset id, ascending.
    let mut page = search_page(2, false);
    page["assets"][1]["duration_ms"] = page["assets"][0]["duration_ms"].clone();
    let scratch = scratch("search-page-tie");
    let (mut bridge, _) = recording_bridge(vec![HostOutcome::Ok(page)], &scratch);
    let response = bridge.handle(&request(
        "r-1",
        "photos.search",
        json!({"filter": {"media_type": "video"}, "sort": "duration_desc"}),
    ));
    assert_eq!(response.status, BridgeStatus::Ok, "{response:?}");
}

#[test]
fn a_result_outside_the_schema_is_a_protocol_violation() {
    let search = json!({"filter": {"media_type": "video", "created_after": "2025-01-01T00:00:00Z"}, "sort": "duration_desc", "limit": 2});
    let mut cases: Vec<(&str, Value, Value)> = Vec::new();

    let mut extra = search_page(1, false);
    extra["cursor"] = json!("next");
    cases.push(("photos.search", search.clone(), extra));
    cases.push(("photos.search", search.clone(), search_page(3, false)));
    cases.push(("photos.search", search.clone(), search_page(1, true)));
    let mut unordered = search_page(2, false);
    unordered["assets"][1]["duration_ms"] = json!(2_000_000);
    cases.push(("photos.search", search.clone(), unordered));
    let mut tie = search_page(2, false);
    tie["assets"][0]["asset_id"] = json!("z");
    tie["assets"][1]["duration_ms"] = tie["assets"][0]["duration_ms"].clone();
    cases.push(("photos.search", search.clone(), tie));
    let mut early = search_page(1, false);
    early["assets"][0]["created_at"] = json!("2024-12-31T23:59:59Z");
    cases.push(("photos.search", search.clone(), early));
    let mut photo = search_page(1, false);
    photo["assets"][0]["media_type"] = json!("photo");
    cases.push(("photos.search", search.clone(), photo));
    let mut wide = search_page(1, false);
    wide["assets"][0]["width"] = json!(32_769);
    cases.push(("photos.search", search.clone(), wide));
    let mut long = search_page(1, false);
    long["assets"][0]["duration_ms"] = json!(604_800_001u64);
    cases.push(("photos.search", search.clone(), long));
    let mut denied = search_page(1, false);
    denied["authorization"] = json!("denied");
    cases.push(("photos.search", search.clone(), denied));

    let to_workspace = json!({"asset_id": "A1", "destination": "guest_workspace"});
    let to_picker = json!({"asset_id": "A1", "destination": "files_picker"});
    cases.push((
        "photos.export",
        to_workspace.clone(),
        photos_export_result("files_picker"),
    ));
    cases.push((
        "photos.export",
        to_picker.clone(),
        photos_export_result("guest_workspace"),
    ));
    let mut path = photos_export_result("guest_workspace");
    path["relative_path"] = json!("c0ffee/other.mov");
    cases.push(("photos.export", to_workspace.clone(), path));
    let mut size = photos_export_result("guest_workspace");
    size["size_bytes"] = json!(268_435_457u64);
    cases.push(("photos.export", to_workspace.clone(), size));
    let mut digest = photos_export_result("guest_workspace");
    digest["sha256"] = json!("AB".repeat(32));
    cases.push(("photos.export", to_workspace.clone(), digest));

    let import = json!({"destination": "guest_workspace"});
    let mut native = files_import_result();
    native["relative_path"] = json!("/var/mobile/notes.txt");
    cases.push(("files.import", import.clone(), native));
    let mut content = files_import_result();
    content["content_type"] = json!("text/plain; charset=utf-8");
    cases.push(("files.import", import.clone(), content));
    let mut id = files_import_result();
    id["copy_id"] = json!("C0FFEE");
    id["relative_path"] = json!("C0FFEE/notes.txt");
    cases.push(("files.import", import.clone(), id));

    let export = json!({"copy_id": "c0ffee", "relative_path": "c0ffee/notes.txt", "destination": "files_picker"});
    let mut leaked = files_export_result();
    leaked["path"] = json!("/private/var/mobile/notes.txt");
    cases.push(("files.export", export.clone(), leaked));

    for (index, (capability, input, result)) in cases.into_iter().enumerate() {
        let scratch = scratch(&format!("result-schema-{index}"));
        let approval = protocol::capability(capability).unwrap().requires_approval;
        let mut answers = Vec::new();
        if approval {
            answers.push(approval_required());
        }
        answers.push(HostOutcome::Ok(result.clone()));
        let (mut bridge, _) = recording_bridge(answers, &scratch);
        let response = if approval {
            bridge.handle(&request("r-1", capability, input.clone()));
            bridge.handle(&continuation("r-2", capability, input))
        } else {
            bridge.handle(&request("r-1", capability, input))
        };
        assert_eq!(
            response.status,
            BridgeStatus::Error,
            "case {index}: {result}"
        );
        assert_eq!(
            reason(&response),
            "host_protocol_violation",
            "case {index}: {result}"
        );
        assert!(response.result.is_none());
    }
}
