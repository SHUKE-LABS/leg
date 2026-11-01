//! `leg-bridge`: the guest-side capability bridge.
//!
//! The command reads one capability-bridge request from standard input and
//! writes one response to standard output. The host owns the transport, the
//! permission prompt, the dispatch, the cancellation, and the user
//! confirmation; the bridge owns the protocol, the approval binding, and the
//! audit summary.
//!
//! ```text
//! printf '%s\n' '{"version":1,"request_id":"r-1","capability":"photos.list","input":{}}' \
//!   | leg-bridge
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::process::ExitCode;

use leg::bridge::adapter::{SocketHostAdapter, UnavailableAdapter};
use leg::bridge::protocol::{BridgeError, BridgeResponse, BridgeStatus, MAX_REQUEST_BYTES};
use leg::bridge::service::Bridge;
use leg::bridge::state::{AuditLog, BridgeState};

/// The environment variable that names the Unix socket the host listens on.
const HOST_SOCKET_ENV: &str = "LEG_BRIDGE_HOST_SOCKET";

/// The usage of the bridge command.
const USAGE: &str = "\
usage: leg-bridge [--version|-V] [--help|-h]

Reads one capability-bridge request from standard input and writes one
response to standard output. The request is one line of JSON:

  {\"version\":1,\"request_id\":\"r-1\",\"capability\":\"photos.list\",\"input\":{}}

The response is one line of JSON with exactly one status:

  {\"version\":1,\"request_id\":\"r-1\",\"status\":\"ok\",\"result\":{},\"error\":null}

The statuses are ok, approval_required, denied, cancelled, unavailable, and
error. The host owns the transport, the permission prompt, the dispatch, and
the cancellation; the bridge owns the protocol, the approval binding, and the
audit summary.

Environment:
  LEG_BRIDGE_HOST_SOCKET the Unix socket the host listens on. The bridge
                        connects to it, writes the request, and reads the
                        host's answer from it. Without it the bridge answers
                        \"unavailable\".
  LEG_BRIDGE_STATE_DIR  the directory the request records and the audit
                        summary live in. Else XDG_STATE_HOME/leg/bridge,
                        else ~/.local/state/leg/bridge.
  LEG_BRIDGE_AUDIT_FILE the file the audit summary is appended to. Else
                        audit.log in the state directory.
  LEG_BRIDGE_AUDIT      \"off\" turns the audit summary off.";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("leg-bridge {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("leg-bridge: unrecognised argument {other:?}\n{USAGE}");
            return ExitCode::from(2);
        }
        None => {}
    }

    // The bridge cancels a request when the host stops it, so it arms the
    // interrupt handler before it waits for the host.
    if let Err(error) = leg::bridge::install_interrupt_handler() {
        eprintln!("leg-bridge: the interrupt handler could not be installed: {error}");
    }

    let request = match read_request() {
        Ok(request) => request,
        Err(error) => {
            write_response(&BridgeResponse::failure("", BridgeStatus::Error, error));
            return ExitCode::SUCCESS;
        }
    };

    let response = match build_bridge() {
        Ok(mut bridge) => bridge.handle(&request),
        Err(error) => BridgeResponse::failure(
            "",
            BridgeStatus::Error,
            BridgeError::new(
                "state_unavailable",
                error,
                "make the bridge state directory writable",
            ),
        ),
    };
    write_response(&response);
    ExitCode::SUCCESS
}

/// Reads the request line from standard input.
///
/// The request is one line: the bridge reads the first line and ignores what
/// follows it. A request that is longer than the limit is refused.
fn read_request() -> Result<String, BridgeError> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock()).take((MAX_REQUEST_BYTES + 1) as u64);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).map_err(|error| {
        BridgeError::new(
            "unreadable_request",
            format!("standard input could not be read: {error}"),
            "pipe the request into the bridge",
        )
    })?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err(BridgeError::new(
            "request_too_large",
            format!("the request is larger than the {MAX_REQUEST_BYTES} byte limit"),
            "send a smaller request",
        ));
    }
    // The request is decoded strictly: bytes that are not UTF-8 are a
    // malformed request, not a request with replacement characters in it.
    let request = match String::from_utf8(line) {
        Ok(text) => text.trim_end_matches(['\n', '\r']).to_string(),
        Err(_) => {
            return Err(BridgeError::new(
                "malformed_request",
                "the request is not valid UTF-8",
                "pipe one request of UTF-8 JSON into the bridge",
            ));
        }
    };
    if request.trim().is_empty() {
        return Err(BridgeError::new(
            "empty_request",
            "standard input carried no request",
            "pipe one request into the bridge",
        ));
    }
    Ok(request)
}

/// Builds the bridge the command runs.
fn build_bridge() -> Result<Bridge, String> {
    let state_dir = leg::bridge::state::state_dir()?;
    let state = BridgeState::load(&state_dir)?;
    let audit = AuditLog::from_env(&state_dir);

    let adapter: Box<dyn leg::bridge::adapter::HostAdapter> =
        match std::env::var_os(HOST_SOCKET_ENV).filter(|path| !path.is_empty()) {
            Some(path) => Box::new(SocketHostAdapter::new(path)),
            None => Box::new(UnavailableAdapter),
        };

    Ok(Bridge::new(adapter, state, audit))
}

/// Writes the response to standard output.
fn write_response(response: &BridgeResponse) {
    let mut text = match serde_json::to_string(response) {
        Ok(text) => text,
        Err(error) => format!(
            "{{\"version\":1,\"request_id\":\"\",\"status\":\"error\",\"result\":null,\
             \"error\":{{\"reason\":\"serialization_failed\",\"message\":\"the answer could not be \
             serialized: {error}\",\"remediation\":\"report the failure to the host\"}}}}"
        ),
    };
    text.push('\n');
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(text.as_bytes());
    let _ = handle.flush();
}
