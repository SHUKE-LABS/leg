use super::adapter::{self, HostAdapter, HostOutcome, HostRequest};
use super::protocol::{self, BridgeStatus};
use super::service::Bridge;
use super::state::{AuditLog, BridgeState};

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

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
}

impl FakeHost {
    /// A host that answers every request the same way.
    fn always(answer: HostOutcome) -> Self {
        FakeHost {
            answers: vec![answer],
            seen: Vec::new(),
        }
    }
}

impl HostAdapter for FakeHost {
    fn dispatch(&mut self, request: &HostRequest, _deadline: Instant) -> HostOutcome {
        self.seen.push(request.clone());
        if self.answers.len() > 1 {
            return self.answers.remove(0);
        }
        self.answers
            .first()
            .cloned()
            .unwrap_or(HostOutcome::Cancelled)
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
        },
        &scratch,
    );

    let preview = bridge.handle(&request("r-1", "photos.list", json!({"limit": 1})));
    assert_eq!(preview.status, BridgeStatus::ApprovalRequired);
    let token = preview.result.expect("the answer carries a result")["approval_token"]
        .as_str()
        .expect("the answer carries a token")
        .to_string();
    assert_eq!(token, "token-0123456789abcdef");

    // The replay carries the same request id and the same input.
    let replay = json!({
        "version": 1,
        "request_id": "r-1",
        "capability": "photos.list",
        "input": {"limit": 1},
        "approval_token": token,
    })
    .to_string();
    let response = bridge.handle(&replay);
    assert_eq!(response.status, BridgeStatus::Ok);
    assert_eq!(response.request_id, "r-1");
}

#[test]
fn an_approval_is_bound_to_the_request_and_its_input() {
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
        },
        &scratch,
    );

    bridge.handle(&request("r-1", "photos.list", json!({"limit": 1})));

    // A different input may not reuse the approval.
    let changed = json!({
        "version": 1,
        "request_id": "r-1",
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

    // A token the bridge never issued is refused as well.
    let forged = json!({
        "version": 1,
        "request_id": "r-2",
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
fn the_file_transport_answers_through_the_shared_directory() {
    let scratch = scratch("transport");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");

    let host = std::thread::spawn({
        let host_dir = host_dir.clone();
        move || {
            let request_path = host_dir.join(adapter::request_file("r-1"));
            let response_path = host_dir.join(adapter::response_file("r-1"));
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while Instant::now() < deadline && !request_path.exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let request: Value =
                serde_json::from_str(&fs::read_to_string(&request_path).unwrap()).unwrap();
            let nonce = request["nonce"].as_str().unwrap().to_string();
            let reply = json!({
                "version": 1,
                "nonce": nonce,
                "request_id": request["request_id"],
                "status": "ok",
                "result": {"items": ["a.jpg"]},
                "error": null,
            });
            fs::write(&response_path, reply.to_string()).unwrap();
        }
    });

    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut bridge = Bridge::new(
        Box::new(adapter::FileHostAdapter::new(&host_dir)),
        state,
        AuditLog::new(Some(scratch.path.join("audit.log"))),
    );
    let response = bridge.handle(&request("r-1", "files.list", json!({})));
    host.join().expect("the host thread finished");

    assert_eq!(response.status, BridgeStatus::Ok);
    assert_eq!(response.result, Some(json!({"items": ["a.jpg"]})));
}

#[test]
fn the_file_transport_times_out_when_the_host_stays_silent() {
    let scratch = scratch("transport-timeout");
    let host_dir = scratch.path.join("host");
    fs::create_dir_all(&host_dir).expect("create the host directory");

    let state = BridgeState::load(&scratch.path).expect("load the bridge state");
    let mut bridge = Bridge::new(
        Box::new(adapter::FileHostAdapter::new(&host_dir)),
        state,
        AuditLog::new(Some(scratch.path.join("audit.log"))),
    );
    let started = Instant::now();
    let line = json!({
        "version": 1,
        "request_id": "r-1",
        "capability": "files.list",
        "input": {},
        "timeout_ms": 300,
    })
    .to_string();
    let response = bridge.handle(&line);
    assert_eq!(response.status, BridgeStatus::Error);
    assert_eq!(
        response.error.expect("the request failed").reason,
        "timeout"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(
        host_dir.join(adapter::cancel_file("r-1")).exists(),
        "the bridge did not tell the host that it stopped waiting"
    );
}
