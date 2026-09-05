//! Hermetic proof of the `leg exchange` <-> `ExternalAgentParticipant` contract.
//!
//! Spawns the *compiled* `leg` binary against a hand-rolled HTTP/1.1 mock
//! standing in for the Anthropic endpoint, driving it exactly the way
//! baton's `capture_child_output` does: write the request body to the
//! child's stdin, close stdin, drain stdout to EOF, then check the exit
//! code. No baton checkout is required for this proof — see
//! `scripts/external-agent-proof.sh` for the real cross-repo run.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Starts a one-shot HTTP/1.1 mock server on an OS-assigned port, returning
/// its base URL. It accepts exactly one connection, reads the request until
/// the end of its headers (the body content is irrelevant to this proof),
/// and replies with `status`/`body`.
fn spawn_mock_server(status: u16, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");

    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 8192];
        let mut received = Vec::new();
        while !received.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
        }
        let reason = if status == 200 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });

    format!("http://{addr}")
}

/// Spawns `leg exchange` against `base_url`, feeds `stdin_body` on stdin
/// exactly as `ExternalAgentParticipant` does (write, then close), and
/// returns `(exit_success, stdout, stderr)`.
fn run_leg_exchange(base_url: &str, stdin_body: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_leg"))
        .arg("exchange")
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("LEG_MODEL", "claude-test-model")
        .env("LEG_TIMEOUT_SECS", "5")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn leg exchange");

    // The taken `ChildStdin` is a temporary: it drops (closing the pipe,
    // signalling EOF) at the end of this statement, mirroring
    // `capture_child_output`'s write-then-close-stdin sequencing.
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin_body.as_bytes())
        .expect("write stdin");

    let output = child
        .wait_timeout_or_kill(Duration::from_secs(10))
        .expect("child did not hang");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Minimal `wait-with-timeout`: `std::process::Child` has no built-in
/// deadline, and pulling in a crate for one test is not warranted, so this
/// polls `try_wait` — the child is a short-lived local process, not a hot
/// loop concern.
trait WaitTimeoutOrKill {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<std::process::Output>;
}

impl WaitTimeoutOrKill for std::process::Child {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<std::process::Output> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.try_wait()?.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.kill();
                self.wait()?;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        let mut stdout = Vec::new();
        if let Some(mut out) = self.stdout.take() {
            let _ = out.read_to_end(&mut stdout);
        }
        let mut stderr = Vec::new();
        if let Some(mut err) = self.stderr.take() {
            let _ = err.read_to_end(&mut stderr);
        }
        let status = self.wait()?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }
}

#[test]
fn plain_text_success_writes_only_the_raw_reply_body() {
    let base_url = spawn_mock_server(
        200,
        r#"{"content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn"}"#,
    );

    let (success, stdout, _stderr) = run_leg_exchange(&base_url, "hello");

    assert!(success, "leg exchange should exit 0 on a successful reply");
    assert_eq!(stdout, "hi there\n");
}

#[test]
fn plain_text_provider_failure_leaves_stdout_empty_for_batons_machinery_error_path() {
    let base_url = spawn_mock_server(
        401,
        r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    );

    let (success, stdout, stderr) = run_leg_exchange(&base_url, "hello");

    assert!(
        success,
        "a provider failure must still exit 0 — it is a delivered outcome, not a process error"
    );
    assert!(
        stdout.is_empty(),
        "empty stdout on exit 0 is what makes ExternalAgentParticipant synthesize its own \
         kind:\"error\" envelope; got {stdout:?}"
    );
    assert!(
        stderr.contains("invalid x-api-key"),
        "the diagnostic must still be observable on stderr; got {stderr:?}"
    );
}
