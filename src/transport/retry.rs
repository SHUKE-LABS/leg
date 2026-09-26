//! Bounded retry policy for provider HTTP calls.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{LegError, Result};
use crate::interrupt;

use super::http::{HttpCall, HttpClient, HttpResponse};

/// Maximum delay used for both exponential backoff and `Retry-After`.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Retry settings for a [`RetryingHttpClient`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_retries: usize,
    base_delay: Duration,
}

impl RetryPolicy {
    /// Creates a retry policy. `max_retries` excludes the initial attempt.
    pub const fn new(max_retries: usize, base_delay: Duration) -> Self {
        Self {
            max_retries,
            base_delay,
        }
    }

    fn delay(&self, retry_number: usize, retry_after: Option<&str>) -> Duration {
        if let Some(value) = retry_after
            && let Some(delay) = parse_retry_after(value, SystemTime::now())
        {
            return delay.min(MAX_RETRY_DELAY);
        }

        full_jitter(self.backoff_ceiling(retry_number))
    }

    fn backoff_ceiling(&self, retry_number: usize) -> Duration {
        let mut delay = self.base_delay.min(MAX_RETRY_DELAY);
        for _ in 1..retry_number {
            if delay.is_zero() || delay >= MAX_RETRY_DELAY {
                break;
            }
            delay = delay.saturating_mul(2).min(MAX_RETRY_DELAY);
        }
        delay
    }
}

/// Retries eligible HTTP failures before a provider client receives a response.
pub struct RetryingHttpClient<H> {
    inner: H,
    policy: RetryPolicy,
}

impl<H> RetryingHttpClient<H> {
    /// Wraps `inner` with a bounded retry policy.
    pub fn new(inner: H, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

impl<H: HttpClient> HttpClient for RetryingHttpClient<H> {
    fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse> {
        self.post_json_with_attempts(url, headers, body).result
    }

    fn post_json_with_attempts(&self, url: &str, headers: &[(&str, &str)], body: &str) -> HttpCall {
        let mut retries = 0;
        let mut attempts = 0_u64;

        loop {
            if let Err(error) = interrupt::check() {
                return HttpCall::new(Err(error), attempts);
            }

            let call = self.inner.post_json_with_attempts(url, headers, body);
            attempts = attempts.saturating_add(call.attempts);

            match call.result {
                Ok(response) => {
                    if !is_retryable_status(response.status) || retries >= self.policy.max_retries {
                        return HttpCall::new(Ok(response), attempts);
                    }

                    if let Err(error) = sleep_interruptibly(
                        self.policy
                            .delay(retries + 1, response.retry_after.as_deref()),
                    ) {
                        return HttpCall::new(Err(error), attempts);
                    }
                }
                Err(error) => {
                    if !matches!(error, LegError::Transport(_))
                        || retries >= self.policy.max_retries
                    {
                        return HttpCall::new(Err(error), attempts);
                    }

                    if let Err(error) = sleep_interruptibly(self.policy.delay(retries + 1, None)) {
                        return HttpCall::new(Err(error), attempts);
                    }
                }
            }
            retries += 1;
        }
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let deadline = httpdate::parse_http_date(value).ok()?;
    Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

fn full_jitter(ceiling: Duration) -> Duration {
    let range = ceiling.as_nanos().saturating_add(1);
    if range <= 1 {
        return Duration::ZERO;
    }

    Duration::from_nanos((random_sample() as u128 % range) as u64)
}

fn random_sample() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    thread::current().id().hash(&mut hasher);
    hasher.finish()
}

fn sleep_interruptibly(duration: Duration) -> Result<()> {
    let deadline = Instant::now() + duration;
    loop {
        interrupt::check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::http::{HttpCall, HttpResponse};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    struct ScriptedHttp {
        calls: Cell<usize>,
        results: RefCell<VecDeque<Result<HttpResponse>>>,
    }

    impl ScriptedHttp {
        fn new(results: Vec<Result<HttpResponse>>) -> Self {
            Self {
                calls: Cell::new(0),
                results: RefCell::new(results.into()),
            }
        }
    }

    impl HttpClient for ScriptedHttp {
        fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &str,
        ) -> Result<HttpResponse> {
            self.calls.set(self.calls.get() + 1);
            self.results
                .borrow_mut()
                .pop_front()
                .expect("scripted HTTP call")
        }
    }

    fn response(status: u16, retry_after: Option<&str>) -> Result<HttpResponse> {
        Ok(HttpResponse {
            status,
            body: String::new(),
            retry_after: retry_after.map(str::to_string),
        })
    }

    fn post(client: &impl HttpClient) -> HttpCall {
        client.post_json_with_attempts("https://provider.invalid", &[], "{}")
    }

    #[test]
    fn retries_connection_failures_and_counts_attempts() {
        let inner = ScriptedHttp::new(vec![
            Err(LegError::Transport("connection refused".to_string())),
            response(200, None),
        ]);
        let client = RetryingHttpClient::new(&inner, RetryPolicy::new(1, Duration::ZERO));

        let call = post(&client);

        assert_eq!(call.result.expect("retry should succeed").status, 200);
        assert_eq!(call.attempts, 2);
        assert_eq!(inner.calls.get(), 2);
    }

    #[test]
    fn retries_only_the_eligible_statuses() {
        for status in [408, 429, 500, 502, 503, 504, 529] {
            let inner = ScriptedHttp::new(vec![response(status, Some("0")), response(200, None)]);
            let client = RetryingHttpClient::new(&inner, RetryPolicy::new(1, Duration::ZERO));

            let call = post(&client);

            assert_eq!(
                call.result.expect("eligible status should retry").status,
                200,
                "status {status}"
            );
            assert_eq!(call.attempts, 2, "status {status}");
            assert_eq!(inner.calls.get(), 2, "status {status}");
        }
    }

    #[test]
    fn other_client_errors_and_response_read_failures_do_not_retry() {
        for error in [
            LegError::Api {
                status: 400,
                error_type: None,
                message: "invalid request".to_string(),
            },
            LegError::ResponseRead("truncated response".to_string()),
            LegError::NonRetryableTransport("invalid URL".to_string()),
        ] {
            let inner = ScriptedHttp::new(vec![Err(error), response(200, None)]);
            let client = RetryingHttpClient::new(&inner, RetryPolicy::new(2, Duration::ZERO));

            let call = post(&client);

            assert!(call.result.is_err());
            assert_eq!(call.attempts, 1);
            assert_eq!(inner.calls.get(), 1);
        }
    }

    #[test]
    fn zero_retry_budget_disables_retries() {
        let inner = ScriptedHttp::new(vec![
            Err(LegError::Transport("connection refused".to_string())),
            response(200, None),
        ]);
        let client = RetryingHttpClient::new(&inner, RetryPolicy::new(0, Duration::ZERO));

        let call = post(&client);

        assert!(call.result.is_err());
        assert_eq!(call.attempts, 1);
        assert_eq!(inner.calls.get(), 1);
    }

    #[test]
    fn retry_budget_caps_status_retries_and_keeps_attempts() {
        let inner = ScriptedHttp::new(vec![
            response(503, Some("0")),
            response(503, Some("0")),
            response(503, Some("0")),
        ]);
        let client = RetryingHttpClient::new(&inner, RetryPolicy::new(1, Duration::ZERO));

        let call = post(&client);

        assert_eq!(
            call.result.expect("last HTTP response is returned").status,
            503
        );
        assert_eq!(call.attempts, 2);
        assert_eq!(inner.calls.get(), 2);
    }

    #[test]
    fn retry_after_supports_seconds_and_http_dates_and_is_capped() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        let future = httpdate::fmt_http_date(now + Duration::from_secs(45));
        assert_eq!(
            parse_retry_after(&future, now)
                .unwrap()
                .min(MAX_RETRY_DELAY),
            MAX_RETRY_DELAY
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn retry_after_overrides_backoff_and_obeys_the_cap() {
        let policy = RetryPolicy::new(3, Duration::from_secs(2));
        assert_eq!(policy.delay(1, Some("0")), Duration::ZERO);
        assert_eq!(policy.delay(1, Some("120")), MAX_RETRY_DELAY);
    }

    #[test]
    fn full_jitter_stays_inside_its_delay_ceiling() {
        let ceiling = Duration::from_millis(40);
        assert_eq!(full_jitter(Duration::ZERO), Duration::ZERO);
        assert!(full_jitter(ceiling) <= ceiling);
    }

    #[test]
    fn backoff_ceiling_doubles_then_caps() {
        let policy = RetryPolicy::new(8, Duration::from_secs(10));
        assert_eq!(policy.backoff_ceiling(1), Duration::from_secs(10));
        assert_eq!(policy.backoff_ceiling(2), Duration::from_secs(20));
        assert_eq!(policy.backoff_ceiling(3), MAX_RETRY_DELAY);
        assert_eq!(policy.backoff_ceiling(8), MAX_RETRY_DELAY);
    }
}
