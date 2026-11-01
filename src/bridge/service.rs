//! The bridge lifecycle: validate, bind, dispatch, and answer.
//!
//! The order of the steps is the contract:
//!
//! 1. the request is read and validated — a malformed, oversized, unknown, or
//!    path-hostile request never reaches the host;
//! 2. the request id is checked against the ledger, so a request id is used
//!    once and an approval token is only accepted for the capability and the
//!    input it was issued for;
//! 3. the request is handed to the host, which owns the permission prompt, the
//!    dispatch, and the cancellation;
//! 4. the answer is written back and summarized in the audit trail.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::adapter::{self, HostAdapter, HostOutcome, HostRequest};
use super::protocol::{
    self, BridgeError, BridgeRequest, BridgeResponse, BridgeStatus, DEFAULT_TIMEOUT_MS,
    MAX_REQUEST_BYTES, MAX_RESULT_BYTES, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, PROTOCOL_VERSION,
};
use super::state::{AuditEntry, AuditLog, BridgeState, audit_timestamp};

/// The longest capability name the audit summary records.
const AUDIT_CAPABILITY_CHARS: usize = 64;

/// The bridge: the protocol, the host seam, the ledger, and the audit trail.
pub struct Bridge {
    /// The transport that reaches the host.
    adapter: Box<dyn HostAdapter>,
    /// The guest-local request records and approval bindings.
    state: BridgeState,
    /// The audit summary the bridge writes.
    audit: AuditLog,
}

impl Bridge {
    /// Creates a bridge that talks to the host through `adapter`.
    pub fn new(adapter: Box<dyn HostAdapter>, state: BridgeState, audit: AuditLog) -> Self {
        Self {
            adapter,
            state,
            audit,
        }
    }

    /// The guest-local ledger of the bridge.
    pub fn state(&self) -> &BridgeState {
        &self.state
    }

    /// Answers one request line.
    pub fn handle(&mut self, line: &str) -> BridgeResponse {
        let started = Instant::now();

        if line.len() > MAX_REQUEST_BYTES {
            return self.answer(
                "",
                "",
                BridgeStatus::Error,
                None,
                Some(BridgeError::new(
                    "request_too_large",
                    format!("the request is larger than the {MAX_REQUEST_BYTES} byte limit"),
                    "send a smaller request",
                )),
                started,
                String::new(),
                Vec::new(),
                false,
            );
        }

        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                return self.answer(
                    "",
                    "",
                    BridgeStatus::Error,
                    None,
                    Some(BridgeError::new(
                        "malformed_request",
                        format!("the request is not valid JSON: {error}"),
                        "send one line of JSON that follows the bridge request contract",
                    )),
                    started,
                    String::new(),
                    Vec::new(),
                    false,
                );
            }
        };

        let request_id = protocol::peek_request_id(&value);
        let raw_capability = value
            .get("capability")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(AUDIT_CAPABILITY_CHARS)
            .collect::<String>();

        let request = match protocol::validate_request(&value) {
            Ok(request) => request,
            Err(error) => {
                return self.answer(
                    request_id,
                    raw_capability,
                    BridgeStatus::Error,
                    None,
                    Some(error),
                    started,
                    String::new(),
                    Vec::new(),
                    false,
                );
            }
        };

        self.dispatch(request, started)
    }

    /// Runs the validated request through the host.
    fn dispatch(&mut self, request: BridgeRequest, started: Instant) -> BridgeResponse {
        let digest = request.input_digest();
        let input_fields = request
            .input
            .as_object()
            .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        // The ledger must be writable before anything reaches the host: a
        // bridge that cannot remember which request ids it dispatched cannot
        // keep them unique, and must not act.
        if let Err(error) = self.state.save() {
            return self.answer(
                request.request_id.clone(),
                request.capability.clone(),
                BridgeStatus::Error,
                None,
                Some(BridgeError::new(
                    "state_unavailable",
                    error,
                    "make the bridge state directory writable",
                )),
                started,
                digest,
                input_fields,
                false,
            );
        }

        let continuation = match self.check_approval(&request, &digest, started) {
            Ok(continuation) => continuation,
            Err(response) => return *response,
        };

        let timeout_ms = request
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        let host_request = HostRequest {
            version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            nonce: adapter::new_nonce(),
            capability: request.capability.clone(),
            input: request.input.clone(),
            input_sha256: digest.clone(),
            approval_token: request.approval_token.clone(),
            timeout_ms,
        };

        let outcome = self.adapter.dispatch(&host_request, deadline);
        self.finish(
            request,
            &digest,
            input_fields,
            outcome,
            continuation,
            started,
        )
    }

    /// Checks the request against the ledger.
    ///
    /// A request id is used once: the ledger remembers the requests that
    /// reached the host, so a repeated id is refused. A request that carries
    /// an approval token must carry one the bridge issued for the same
    /// capability and the same input; the host verifies it again before it
    /// acts.
    fn check_approval(
        &mut self,
        request: &BridgeRequest,
        digest: &str,
        started: Instant,
    ) -> Result<bool, Box<BridgeResponse>> {
        let capability = request.capability.clone();
        let request_id = request.request_id.clone();
        let input_fields = request
            .input
            .as_object()
            .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        if self.state.is_known(&request_id) {
            return Err(Box::new(self.answer(
                request_id.clone(),
                capability.clone(),
                BridgeStatus::Error,
                None,
                Some(BridgeError::new(
                    "duplicate_request_id",
                    format!("the request id `{request_id}` was already used"),
                    "use a fresh request id",
                )),
                started,
                digest.to_string(),
                input_fields,
                false,
            )));
        }

        let Some(token) = request.approval_token.as_deref() else {
            return Ok(false);
        };

        let Some(record) = self.state.approval_for(token).cloned() else {
            return Err(Box::new(self.answer(
                request_id.clone(),
                capability.clone(),
                BridgeStatus::Denied,
                None,
                Some(BridgeError::new(
                    "approval_unknown",
                    "the approval token was not issued by the host for this guest",
                    "ask the host for an approval and replay the token it issues",
                )),
                started,
                digest.to_string(),
                input_fields,
                false,
            )));
        };

        if record.redeemed {
            return Err(Box::new(self.answer(
                request_id.clone(),
                capability.clone(),
                BridgeStatus::Denied,
                None,
                Some(BridgeError::new(
                    "approval_used",
                    "the approval token was already redeemed",
                    "ask the host for a fresh approval",
                )),
                started,
                digest.to_string(),
                input_fields,
                false,
            )));
        }
        if record.capability != capability {
            return Err(Box::new(self.answer(
                request_id.clone(),
                capability.clone(),
                BridgeStatus::Denied,
                None,
                Some(BridgeError::new(
                    "approval_mismatch",
                    format!(
                        "the approval was granted for `{}`, not for `{capability}`",
                        record.capability
                    ),
                    "ask the host for an approval for this capability",
                )),
                started,
                digest.to_string(),
                input_fields,
                false,
            )));
        }
        if record.input_sha256 != digest {
            return Err(Box::new(self.answer(
                request_id.clone(),
                capability.clone(),
                BridgeStatus::Denied,
                None,
                Some(BridgeError::new(
                    "approval_mismatch",
                    "the approval was granted for a different input",
                    "ask the host for an approval for this input",
                )),
                started,
                digest.to_string(),
                input_fields,
                false,
            )));
        }
        if !BridgeState::approval_is_fresh(&record) {
            return Err(Box::new(self.answer(
                request_id.clone(),
                capability.clone(),
                BridgeStatus::Denied,
                None,
                Some(BridgeError::new(
                    "approval_expired",
                    "the approval token has expired",
                    "ask the host for a fresh approval",
                )),
                started,
                digest.to_string(),
                input_fields,
                false,
            )));
        }

        Ok(true)
    }

    /// Turns the host's answer into the bridge's answer.
    fn finish(
        &mut self,
        request: BridgeRequest,
        digest: &str,
        input_fields: Vec<String>,
        outcome: HostOutcome,
        continuation: bool,
        started: Instant,
    ) -> BridgeResponse {
        let request_id = request.request_id.clone();
        let capability = request.capability.clone();
        let token = request.approval_token.clone();
        let reached = reached_host(&outcome);

        let (status, result, error) = match outcome {
            HostOutcome::Ok(result) => (BridgeStatus::Ok, Some(result), None),
            HostOutcome::ApprovalRequired {
                approval_token,
                review,
            } => {
                if !protocol::valid_approval_token(&approval_token) {
                    (
                        BridgeStatus::Error,
                        None,
                        Some(BridgeError::new(
                            "host_protocol_violation",
                            "the host issued an approval token that is not a token",
                            "the host must issue a token the bridge can relay",
                        )),
                    )
                } else {
                    // The binding names the capability and the digest of the
                    // input, so the token cannot be redeemed for another one.
                    self.state
                        .record_approval(&capability, digest, &approval_token);
                    (
                        BridgeStatus::ApprovalRequired,
                        Some(json!({
                            "approval_token": approval_token,
                            "review": review,
                        })),
                        None,
                    )
                }
            }
            HostOutcome::Denied { reason, message } => (
                BridgeStatus::Denied,
                None,
                Some(BridgeError::new(
                    reason,
                    message,
                    "the host refused the request; ask before retrying",
                )),
            ),
            HostOutcome::Cancelled => (
                BridgeStatus::Cancelled,
                None,
                Some(BridgeError::new(
                    "cancelled",
                    "the request was cancelled before it finished",
                    "send the request again if it is still wanted",
                )),
            ),
            HostOutcome::Unavailable { reason, message } => (
                BridgeStatus::Unavailable,
                None,
                Some(BridgeError::new(
                    reason,
                    message,
                    "the host cannot serve this capability right now",
                )),
            ),
            HostOutcome::Failed { reason, message } => (
                BridgeStatus::Error,
                None,
                Some(BridgeError::new(
                    reason,
                    message,
                    "the host could not serve the request; check the host and retry",
                )),
            ),
            HostOutcome::TimedOut => (
                BridgeStatus::Error,
                None,
                Some(BridgeError::new(
                    "timeout",
                    "the host did not answer in time",
                    "send the request again, or ask for a longer deadline",
                )),
            ),
        };

        let oversized = result
            .as_ref()
            .is_some_and(|result| result.to_string().len() > MAX_RESULT_BYTES);
        let (status, result, error) = if oversized {
            (
                BridgeStatus::Error,
                None,
                Some(BridgeError::new(
                    "result_too_large",
                    format!("the result is larger than the {MAX_RESULT_BYTES} byte limit"),
                    "ask the host for a smaller result",
                )),
            )
        } else {
            (status, result, error)
        };

        // The request reached the host, so its id is used. The ledger keeps
        // the last few dozen request ids, which is as long as a caller may
        // reasonably replay one. A transport that never connected leaves the
        // id free: nothing happened, so the caller may try again.
        if reached {
            self.state
                .record_dispatched(&request_id, digest, status.as_str());
        }
        // A redeemed approval cannot be redeemed twice. An answer that never
        // arrived leaves the approval pending, because the host decides
        // whether the action happened.
        if continuation
            && let Some(token) = token.as_deref()
            && matches!(status, BridgeStatus::Ok | BridgeStatus::Denied)
        {
            self.state.redeem_approval(token);
        }
        let _ = self.state.save();

        self.answer(
            request_id,
            capability,
            status,
            result,
            error,
            started,
            digest.to_string(),
            input_fields,
            continuation,
        )
    }

    /// Writes the answer and the audit summary.
    #[allow(clippy::too_many_arguments)]
    fn answer(
        &mut self,
        request_id: impl Into<String>,
        capability: impl Into<String>,
        status: BridgeStatus,
        result: Option<Value>,
        error: Option<BridgeError>,
        started: Instant,
        input_sha256: String,
        input_fields: Vec<String>,
        approved: bool,
    ) -> BridgeResponse {
        let request_id = request_id.into();
        let capability = capability.into();
        let response = BridgeResponse {
            version: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            status,
            result,
            error: error.clone(),
        };

        let summary = AuditEntry {
            ts: audit_timestamp(),
            request_id: request_id.clone(),
            capability: capability.clone(),
            status: status.as_str(),
            reason: error.map(|error| error.reason),
            duration_ms: started.elapsed().as_millis() as u64,
            input_sha256,
            input_fields,
            approved,
        };
        // A summary that cannot be written is reported, but it does not fail
        // the request: the answer is the answer.
        if let Err(error) = self.audit.append(&summary) {
            eprintln!("leg-bridge: the audit summary could not be written: {error}");
        }

        response
    }
}

/// Whether the request reached the host.
///
/// A transport that never connected leaves the request id free: nothing
/// happened, so the caller may try again with the same id.
fn reached_host(outcome: &HostOutcome) -> bool {
    match outcome {
        HostOutcome::Unavailable { reason, .. } => !matches!(
            reason.as_str(),
            "no_host_transport" | "host_unreachable" | "host_transport_unsupported"
        ),
        _ => true,
    }
}
