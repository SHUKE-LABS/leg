//! HTTP execution boundary for provider clients.
//!
//! [`HttpClient`] is the seam the Claude client depends on so its request
//! building and response parsing can be unit-tested with a fake client, without
//! touching the network — mirroring the testable split in
//! [`LegConfig::from_lookup`](crate::config::LegConfig::from_lookup).
//!
//! A non-2xx status is *not* an error at this layer: it is returned as an
//! ordinary [`HttpResponse`] carrying the status and body so the caller can map
//! it onto the appropriate [`LegError`] variant. Transient connection failures
//! become [`LegError::Transport`]; setup/protocol and response-body read
//! failures use distinct variants so only eligible failures are retried.

use std::io::Read;
use std::time::{Duration, Instant};

use crate::error::{LegError, Result};
use crate::interrupt;

/// A completed HTTP response: the status code and the raw body text.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The buffered response body, or empty when successful stream chunks
    /// were delivered through [`HttpClient::post_json_streaming`].
    pub body: String,
    /// The provider's `Retry-After` header, if present.
    pub retry_after: Option<String>,
}

/// A completed HTTP call with its cumulative request-attempt count.
#[derive(Debug)]
pub struct HttpCall {
    /// The raw response or terminal transport error.
    pub result: Result<HttpResponse>,
    /// Number of HTTP attempts made, including failed connection attempts.
    pub attempts: u64,
}

impl HttpCall {
    /// Creates an HTTP call result with its attempt count.
    pub fn new(result: Result<HttpResponse>, attempts: u64) -> Self {
        Self { result, attempts }
    }
}

/// Sends a single JSON POST request and returns the raw response.
///
/// Implementations must return `Ok` for any completed HTTP exchange, including
/// non-2xx statuses, and reserve `Err(LegError::Transport(..))` for failures
/// caused by a transient connection failure before a response was received.
/// Response-body read failures must use [`LegError::ResponseRead`] and must not
/// be retried.
pub trait HttpClient {
    /// POSTs `body` to `url` with the given `headers` (name, value pairs).
    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse>;

    /// POSTs JSON and reports the attempt count.
    ///
    /// Implementations without lower-level attempt metadata count one call.
    fn post_json_with_attempts(&self, url: &str, headers: &[(&str, &str)], body: &str) -> HttpCall {
        HttpCall::new(self.post_json(url, headers, body), 1)
    }

    /// POSTs JSON and delivers successful response-body chunks as they arrive.
    ///
    /// Buffered clients keep working through this default, which forwards the
    /// completed body as one chunk. HTTP errors retain their complete body for
    /// the provider client and are not sent to `on_chunk`.
    fn post_json_streaming(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> HttpCall {
        let call = self.post_json_with_attempts(url, headers, body);
        let result = call.result.and_then(|mut response| {
            if (200..300).contains(&response.status) {
                on_chunk(response.body.as_bytes())?;
                response.body.clear();
            }
            Ok(response)
        });
        HttpCall::new(result, call.attempts)
    }
}

impl<T: HttpClient + ?Sized> HttpClient for &T {
    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse> {
        (**self).post_json(url, headers, body)
    }

    fn post_json_with_attempts(&self, url: &str, headers: &[(&str, &str)], body: &str) -> HttpCall {
        (**self).post_json_with_attempts(url, headers, body)
    }

    fn post_json_streaming(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> HttpCall {
        (**self).post_json_streaming(url, headers, body, on_chunk)
    }
}

/// A [`HttpClient`] backed by [`ureq`], with setup and stream-idle timeouts.
///
/// Blocking by design: it matches the synchronous [`Transport`] trait, so there
/// is no async runtime to manage for the single-turn first-reply path.
///
/// [`Transport`]: crate::transport::Transport
pub struct UreqHttpClient {
    agent: ureq::Agent,
    stream_agent: ureq::Agent,
    timeout: Duration,
    stream_idle_timeout: Duration,
}

impl UreqHttpClient {
    /// Creates a client with the default stream-idle timeout.
    pub fn new(timeout: Duration) -> Self {
        Self::with_timeouts(
            timeout,
            Duration::from_secs(crate::config::DEFAULT_STREAM_IDLE_TIMEOUT_SECS),
        )
    }

    /// Creates a client with separate request-phase and stream-idle timeouts.
    pub fn with_timeouts(timeout: Duration, stream_idle_timeout: Duration) -> Self {
        // `http_status_as_error(false)` makes ureq return non-2xx responses as
        // `Ok` instead of an error, so the caller sees the status and body and
        // maps them onto leg's error variants.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_resolve(Some(timeout))
            .timeout_connect(Some(timeout))
            .timeout_send_request(Some(timeout))
            .timeout_send_body(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .build()
            .into();
        // Keep the header and send phases bounded while using ureq's per-read
        // body timeout for streaming idle detection. `timeout_recv_response`
        // stays unset because ureq counts it from the headers and would cap the
        // total duration of an otherwise active stream.
        let stream_agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_resolve(Some(timeout))
            .timeout_connect(Some(timeout))
            .timeout_send_request(Some(timeout))
            .timeout_send_body(Some(timeout))
            .timeout_recv_body(Some(stream_idle_timeout))
            .build()
            .into();
        Self {
            agent,
            stream_agent,
            timeout,
            stream_idle_timeout,
        }
    }
}

impl HttpClient for UreqHttpClient {
    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse> {
        let agent = self.agent.clone();
        let url = url.to_string();
        let headers: Vec<_> = headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        let body = body.to_string();
        let timeout = self.timeout;

        interrupt::run_cancellable(move || {
            let mut request = agent.post(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }

            let request = request.config().timeout_recv_body(Some(timeout)).build();
            let mut response = request.send(&body).map_err(|err| {
                let message = err.to_string();
                if is_retryable_connection_error(&err) {
                    LegError::Transport(message)
                } else {
                    LegError::NonRetryableTransport(message)
                }
            })?;

            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let body = response.body_mut().read_to_string().map_err(|err| {
                LegError::ResponseRead(format!("failed to read response body: {err}"))
            })?;

            Ok(HttpResponse {
                status,
                body,
                retry_after,
            })
        })
    }

    fn post_json_streaming(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        on_chunk: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> HttpCall {
        let agent = self.stream_agent.clone();
        let url = url.to_string();
        let headers: Vec<_> = headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        let body = body.to_string();
        let timeout = self.timeout;
        let stream_idle_timeout = self.stream_idle_timeout;

        let result = interrupt::run_cancellable_with_progress(
            move |emit| {
                let mut request = agent.post(&url);
                for (name, value) in &headers {
                    request = request.header(name, value);
                }

                let response = request.send(&body).map_err(|error| match error {
                    ureq::Error::Timeout(_) => LegError::NonRetryableTransport(format!(
                            "request timed out after {} seconds waiting for response headers (LEG_TIMEOUT_SECS)",
                            timeout.as_secs()
                        )),
                    other => request_error(other),
                })?;

                let status = response.status().as_u16();
                let retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                if (200..300).contains(&status) {
                    let mut reader = response.into_body().into_reader();
                    let mut buffer = [0_u8; 8192];
                    loop {
                        let length = match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(length) => length,
                            Err(error) => {
                                return Err(response_body_read_error(error, stream_idle_timeout));
                            }
                        };
                        emit(buffer[..length].to_vec())?;
                    }
                    Ok(HttpResponse {
                        status,
                        body: String::new(),
                        retry_after,
                    })
                } else {
                    let mut reader = response.into_body().into_reader();
                    let mut bytes = Vec::new();
                    let mut buffer = [0_u8; 8192];
                    let started = Instant::now();
                    loop {
                        if started.elapsed() >= timeout {
                            return Err(LegError::ResponseRead(format!(
                                "response body timeout after {} seconds (LEG_TIMEOUT_SECS)",
                                timeout.as_secs()
                            )));
                        }
                        let length = match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(length) => length,
                            Err(error) => {
                                return Err(response_body_read_error(error, stream_idle_timeout));
                            }
                        };
                        if bytes.len().saturating_add(length) > MAX_BUFFERED_RESPONSE_BODY {
                            return Err(LegError::ResponseRead(
                                "response body exceeds 10 MB limit".to_string(),
                            ));
                        }
                        bytes.extend_from_slice(&buffer[..length]);
                    }
                    let body = String::from_utf8_lossy(&bytes).into_owned();
                    Ok(HttpResponse {
                        status,
                        body,
                        retry_after,
                    })
                }
            },
            |chunk: Vec<u8>| on_chunk(&chunk),
        );
        HttpCall::new(result, 1)
    }
}

const MAX_BUFFERED_RESPONSE_BODY: usize = 10 * 1024 * 1024;

fn response_body_read_error(error: std::io::Error, stream_idle_timeout: Duration) -> LegError {
    let is_idle_timeout = matches!(
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<ureq::Error>()),
        Some(ureq::Error::Timeout(_))
    );
    if is_idle_timeout {
        LegError::ResponseRead(format!(
            "response body idle timeout after {} seconds (LEG_STREAM_IDLE_TIMEOUT_SECS)",
            stream_idle_timeout.as_secs()
        ))
    } else {
        LegError::ResponseRead(format!("failed to read response body: {error}"))
    }
}

fn is_retryable_connection_error(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::ConnectProxyFailed(_)
    )
}

fn request_error(error: ureq::Error) -> LegError {
    let message = error.to_string();
    if is_retryable_connection_error(&error) {
        LegError::Transport(message)
    } else {
        LegError::NonRetryableTransport(message)
    }
}

#[cfg(test)]
mod tests {
    use super::{HttpClient, UreqHttpClient, is_retryable_connection_error};
    use crate::error::LegError;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn classifies_only_connection_level_ureq_errors_as_retryable() {
        for error in [
            ureq::Error::Io(std::io::Error::other("connection reset")),
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
            ureq::Error::ConnectProxyFailed("proxy refused connection".to_string()),
        ] {
            assert!(is_retryable_connection_error(&error), "{error}");
        }

        assert!(!is_retryable_connection_error(&ureq::Error::BadUri(
            "missing scheme".to_string()
        )));
    }

    #[test]
    fn header_timeout_closes_every_stalled_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake peer");
        let address = listener.local_addr().expect("fake peer address");
        let peer = thread::spawn(move || {
            for _ in 0..10 {
                let (mut stream, _) = listener.accept().expect("accept request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set peer read timeout");
                read_request(&mut stream);
                assert_peer_closed(&mut stream);
            }
        });

        let client = UreqHttpClient::with_timeouts(Duration::from_secs(1), Duration::from_secs(1));
        let url = format!("http://{address}/");
        for _ in 0..10 {
            let call = client.post_json_streaming(&url, &[], "{}", &mut |_| Ok(()));
            assert_eq!(call.attempts, 1);
            match call.result.expect_err("header stall should time out") {
                LegError::NonRetryableTransport(message) => {
                    assert!(message.contains("LEG_TIMEOUT_SECS"), "{message}");
                }
                other => panic!("unexpected header-stall error: {other}"),
            }
        }
        peer.join()
            .expect("fake peer should see every closed socket");
    }

    #[test]
    fn stream_idle_timeout_closes_every_stalled_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake peer");
        let address = listener.local_addr().expect("fake peer address");
        let peer = thread::spawn(move || {
            for _ in 0..10 {
                let (mut stream, _) = listener.accept().expect("accept request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set peer read timeout");
                read_request(&mut stream);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: keep-alive\r\n\r\nx",
                    )
                    .expect("send partial response");
                assert_peer_closed(&mut stream);
            }
        });

        let client = UreqHttpClient::with_timeouts(Duration::from_secs(1), Duration::from_secs(1));
        let url = format!("http://{address}/");
        for _ in 0..10 {
            let call = client.post_json_streaming(&url, &[], "{}", &mut |_| Ok(()));
            assert_eq!(call.attempts, 1);
            match call.result.expect_err("body stall should time out") {
                LegError::ResponseRead(message) => {
                    assert!(
                        message.contains("LEG_STREAM_IDLE_TIMEOUT_SECS"),
                        "{message}"
                    );
                }
                other => panic!("unexpected body-stall error: {other}"),
            }
        }
        peer.join()
            .expect("fake peer should see every closed socket");
    }

    fn read_request(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let expected_length = loop {
            let length = stream.read(&mut buffer).expect("read request headers");
            assert_ne!(length, 0, "client closed before sending its request");
            request.extend_from_slice(&buffer[..length]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("valid content length"))
                })
                .unwrap_or(0);
            break header_end + content_length;
        };
        while request.len() < expected_length {
            let length = stream.read(&mut buffer).expect("read request body");
            assert_ne!(length, 0, "client closed before sending its request body");
            request.extend_from_slice(&buffer[..length]);
        }
    }

    fn assert_peer_closed(stream: &mut TcpStream) {
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Ok(length) => panic!("expected socket close, read {length} unexpected byte(s)"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            Err(error) => panic!("peer did not see socket close: {error}"),
        }
    }
}
