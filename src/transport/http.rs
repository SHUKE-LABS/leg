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

use std::time::Duration;

use crate::error::{LegError, Result};
use crate::interrupt;

/// A completed HTTP response: the status code and the raw body text.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body, read as a UTF-8 string.
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
}

impl<T: HttpClient + ?Sized> HttpClient for &T {
    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse> {
        (**self).post_json(url, headers, body)
    }

    fn post_json_with_attempts(&self, url: &str, headers: &[(&str, &str)], body: &str) -> HttpCall {
        (**self).post_json_with_attempts(url, headers, body)
    }
}

/// A [`HttpClient`] backed by [`ureq`], with a per-request global timeout.
///
/// Blocking by design: it matches the synchronous [`Transport`] trait, so there
/// is no async runtime to manage for the single-turn first-reply path.
///
/// [`Transport`]: crate::transport::Transport
pub struct UreqHttpClient {
    agent: ureq::Agent,
}

impl UreqHttpClient {
    /// Creates a client whose requests time out after `timeout`.
    pub fn new(timeout: Duration) -> Self {
        // `http_status_as_error(false)` makes ureq return non-2xx responses as
        // `Ok` instead of an error, so the caller sees the status and body and
        // maps them onto leg's error variants.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .into();
        Self { agent }
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

        interrupt::run_cancellable(move || {
            let mut request = agent.post(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }

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

#[cfg(test)]
mod tests {
    use super::is_retryable_connection_error;

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
}
