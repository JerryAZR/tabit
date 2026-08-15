//! Status-aware request-layer retry.
//!
//! Ports the retry semantics shared by the OpenAI and Anthropic SDKs (and pi's
//! `provider-retry` helper built on top of them):
//!
//! - **Retryable responses**: HTTP 408, 409, 429, and any 5xx, plus *any*
//!   status when the response carries `x-should-retry: true`. An explicit
//!   `x-should-retry: false` blocks retrying even a 5xx. All other 4xx
//!   responses fail fast.
//! - **Retryable transports**: connect/timeout/request failures that occur
//!   before any response bytes exist (see
//!   [`Error::transport_error_kind`](super::Error::transport_error_kind)).
//! - **Server-requested delays**: honored from `retry-after-ms` (milliseconds)
//!   and `retry-after` (integer seconds or an HTTP-date, parsed with the
//!   `httpdate` crate). A server-requested delay above [`DEFAULT_MAX_SERVER_DELAY`]
//!   fails the request immediately with
//!   [`Error::RetryDelayTooLong`](super::Error::RetryDelayTooLong) instead of
//!   sleeping — matching the SDKs, which refuse absurd delays.
//! - **Own backoff**: with no usable delay header, retry `i` waits
//!   `min(0.5s * 2^i, 8s)` scaled by a jitter factor drawn from `[0.75, 1.0)`.
//!
//! ## Retry-safety boundary
//!
//! Retrying is only safe while **zero bytes of a success response body have
//! been consumed**. The driver therefore only ever retries:
//!
//! 1. transport failures that happened before a response arrived, and
//! 2. non-2xx responses, whose status/body were consumed by the error path and
//!    never yielded as content.
//!
//! Once a 2xx response has been returned to the caller — streaming or not —
//! errors that surface later are terminal. Callers keep this invariant by
//! driving all attempts through this module *before* handing out the response.

use super::Error;
use http::{HeaderMap, StatusCode};
use std::{future::Future, time::Duration, time::SystemTime};

/// Default number of retries after the initial attempt (3 attempts total).
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default cap on server-requested retry delays (`retry-after*` headers).
///
/// A server asking for more than this fails the request instead of sleeping.
pub const DEFAULT_MAX_SERVER_DELAY: Duration = Duration::from_secs(60);

/// First own-backoff delay, in seconds; doubles per retry.
const BACKOFF_START: f64 = 0.5;
/// Upper bound on the own backoff delay.
const BACKOFF_CAP: Duration = Duration::from_secs(8);
/// Own-backoff delays are scaled by a jitter factor in `[JITTER_MIN, 1.0)`.
const JITTER_MIN: f64 = 0.75;
const JITTER_RANGE: f64 = 0.25;

/// Configuration for status-aware request retries.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retries after the initial attempt. `0` disables
    /// retries entirely. Defaults to [`DEFAULT_MAX_RETRIES`].
    pub max_retries: u32,
    /// Upper bound on server-requested retry delays. A server asking for a
    /// longer delay than this fails the request loudly instead of being
    /// obeyed. Defaults to [`DEFAULT_MAX_SERVER_DELAY`].
    pub max_server_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            max_server_delay: DEFAULT_MAX_SERVER_DELAY,
        }
    }
}

/// A server-requested retry delay exceeded the configured cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DelayTooLong {
    /// Delay the server asked the client to wait.
    pub requested: Duration,
    /// Configured maximum server-requested delay.
    pub cap: Duration,
}

/// Whether an HTTP status (with its response headers) should be retried.
pub(crate) fn is_retryable_status(status: StatusCode, headers: &HeaderMap) -> bool {
    match headers
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        // The provider explicitly opted in (or out) of retries.
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }

    let code = status.as_u16();
    code == 408 || code == 409 || code == 429 || (500..1000).contains(&code)
}

/// Whether a failed [`Error`] should be retried, per the policy above.
pub(crate) fn is_retryable_error(error: &Error) -> bool {
    if let Some(status) = error.non_success_status() {
        let no_headers = HeaderMap::new();
        let headers = error.non_success_headers().unwrap_or(&no_headers);
        return is_retryable_status(status, headers);
    }

    matches!(
        error.transport_error_kind(),
        Some(
            super::TransportErrorKind::Timeout
                | super::TransportErrorKind::Connect
                | super::TransportErrorKind::RequestFailed
        )
    )
}

/// Parse the server-requested retry delay out of a response's headers.
///
/// Returns `Ok(None)` when no parseable delay header is present (the caller
/// falls back to its own backoff), and `Err` when a delay was requested but
/// exceeds `cap`.
pub(crate) fn server_retry_delay(
    headers: &HeaderMap,
    cap: Duration,
) -> Result<Option<Duration>, DelayTooLong> {
    if let Some(value) = header_str(headers, "retry-after-ms")
        && let Ok(millis) = value.parse::<u64>()
    {
        return validated(Duration::from_millis(millis), cap);
    }

    if let Some(value) = header_str(headers, "retry-after") {
        // Integer seconds, or an HTTP-date.
        if let Ok(seconds) = value.parse::<u64>() {
            return validated(Duration::from_secs(seconds), cap);
        }
        if let Ok(http_date) = httpdate::parse_http_date(value) {
            // Dates in the past mean "retry now".
            let delay = http_date
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            return validated(delay, cap);
        }
    }

    Ok(None)
}

/// Own exponential backoff for retry `retry_index` (0-based), scaled by
/// `jitter_factor` (callers draw it from `[0.75, 1.0)`).
pub(crate) fn backoff_delay(retry_index: u32, jitter_factor: f64) -> Duration {
    let exponential = BACKOFF_START * 2f64.powi(i32::try_from(retry_index).unwrap_or(i32::MAX));
    let seconds = exponential.min(BACKOFF_CAP.as_secs_f64());
    let millis = (seconds * jitter_factor * 1000.0).round() as u64;
    Duration::from_millis(millis)
}

/// Draw a jitter factor in `[0.75, 1.0)`.
fn jitter_factor(rng: &mut fastrand::Rng) -> f64 {
    JITTER_MIN + rng.f64() * JITTER_RANGE
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn validated(delay: Duration, cap: Duration) -> Result<Option<Duration>, DelayTooLong> {
    if delay > cap {
        Err(DelayTooLong {
            requested: delay,
            cap,
        })
    } else {
        Ok(Some(delay))
    }
}

impl RetryConfig {
    /// Run `make_attempt` until it succeeds, the policy gives up, or a
    /// server-requested delay exceeds [`Self::max_server_delay`].
    ///
    /// Every attempt builds a fresh request (the closure owns whatever it
    /// needs), so retries never replay a partially-consumed request.
    pub(crate) async fn execute<F, Fut, T>(&self, mut make_attempt: F) -> super::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = super::Result<T>>,
    {
        let mut rng = fastrand::Rng::new();
        let mut retry_index: u32 = 0;

        loop {
            let error = match make_attempt().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };

            if retry_index >= self.max_retries || !is_retryable_error(&error) {
                return Err(error);
            }

            let delay = match self.delay_for(&error, retry_index, &mut rng) {
                Ok(delay) => delay,
                Err(too_long) => {
                    return Err(Error::RetryDelayTooLong {
                        source: Box::new(error),
                        requested: too_long.requested,
                        cap: too_long.cap,
                    });
                }
            };

            retry_index += 1;
            tracing::debug!(
                retry = retry_index,
                delay_ms = delay.as_millis(),
                "retrying provider request"
            );
            futures_timer::Delay::new(delay).await;
        }
    }

    fn delay_for(
        &self,
        error: &Error,
        retry_index: u32,
        rng: &mut fastrand::Rng,
    ) -> Result<Duration, DelayTooLong> {
        if let Some(headers) = error.non_success_headers()
            && let Some(delay) = server_retry_delay(headers, self.max_server_delay)?
        {
            return Ok(delay);
        }

        Ok(backoff_delay(retry_index, jitter_factor(rng)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{Error, TransportErrorKind};
    use std::time::Instant;

    fn status_error(status: StatusCode, headers: &'static [(&'static str, &'static str)]) -> Error {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::from_static(name),
                http::HeaderValue::from_str(value).expect("static test header value"),
            );
        }
        Error::NonSuccessResponse {
            status,
            message: "test body".to_string(),
            headers: map,
        }
    }

    #[test]
    fn defaults_match_the_sdk_policy() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.max_server_delay, Duration::from_secs(60));
    }

    #[test]
    fn retryable_statuses_are_408_409_429_and_5xx() {
        let headers = HeaderMap::new();
        for status in [408, 409, 429, 500, 502, 503, 529] {
            let status = StatusCode::from_u16(status).expect("valid status");
            assert!(
                is_retryable_status(status, &headers),
                "{status} should be retryable"
            );
        }
    }

    #[test]
    fn other_client_errors_are_not_retryable() {
        let headers = HeaderMap::new();
        for status in [400, 401, 403, 404, 409 + 1, 422, 499] {
            let status = StatusCode::from_u16(status).expect("valid status");
            assert!(
                !is_retryable_status(status, &headers),
                "{status} should not be retryable"
            );
        }
    }

    #[test]
    fn x_should_retry_overrides_status_classification() {
        let status = StatusCode::BAD_REQUEST;
        let opt_in = status_error(status, &[("x-should-retry", "true")]);
        assert!(is_retryable_error(&opt_in));

        let overloaded = status_error(StatusCode::BAD_GATEWAY, &[("x-should-retry", "false")]);
        assert!(!is_retryable_error(&overloaded));
    }

    #[test]
    fn legacy_status_errors_without_headers_use_status_only() {
        assert!(is_retryable_error(&Error::InvalidStatusCode(
            StatusCode::TOO_MANY_REQUESTS
        )));
        assert!(!is_retryable_error(&Error::InvalidStatusCode(
            StatusCode::UNAUTHORIZED
        )));
    }

    #[test]
    fn body_transfer_errors_are_not_retryable() {
        let body_error = Error::NonSuccessResponse {
            status: StatusCode::OK,
            message: String::new(),
            headers: HeaderMap::new(),
        };
        // A 2xx preserved in a status-shaped error is not a retry candidate.
        assert!(!is_retryable_error(&body_error));
        assert_eq!(
            body_error.transport_error_kind(),
            None,
            "status errors carry no transport kind"
        );
        assert_eq!(
            Error::StreamEnded.transport_error_kind(),
            None,
            "non-instance errors carry no transport kind"
        );
        let _ = TransportErrorKind::Timeout;
    }

    #[test]
    fn retry_after_ms_is_honored() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after-ms", "250".parse().expect("static value"));
        let delay = server_retry_delay(&headers, Duration::from_secs(60))
            .expect("delay within cap")
            .expect("header present");
        assert_eq!(delay, Duration::from_millis(250));
    }

    #[test]
    fn retry_after_seconds_are_honored() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "3".parse().expect("static value"));
        let delay = server_retry_delay(&headers, Duration::from_secs(60))
            .expect("delay within cap")
            .expect("header present");
        assert_eq!(delay, Duration::from_secs(3));
    }

    #[test]
    fn retry_after_http_date_is_honored() {
        let target = SystemTime::now() + Duration::from_secs(30);
        let http_date = httpdate::fmt_http_date(target);
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            http_date.parse().expect("formatted date is a valid header"),
        );
        let delay = server_retry_delay(&headers, Duration::from_secs(60))
            .expect("delay within cap")
            .expect("header present");
        let lower = Duration::from_secs(28);
        let upper = Duration::from_secs(30);
        assert!(
            delay >= lower && delay <= upper,
            "http-date delay should be ~30s, got {delay:?}"
        );
    }

    #[test]
    fn past_http_date_means_retry_immediately() {
        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(60));
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            past.parse().expect("formatted date is a valid header"),
        );
        let delay = server_retry_delay(&headers, Duration::from_secs(60))
            .expect("delay within cap")
            .expect("header present");
        assert_eq!(delay, Duration::ZERO);
    }

    #[test]
    fn unparseable_delay_headers_fall_back_to_none() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "soon-ish".parse().expect("static value"));
        headers.insert(
            "retry-after-ms",
            "not-a-number".parse().expect("static value"),
        );
        assert_eq!(
            server_retry_delay(&headers, Duration::from_secs(60)).expect("no cap breach"),
            None
        );
    }

    #[test]
    fn delay_above_the_cap_fails_with_the_requested_value() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "61".parse().expect("static value"));
        let too_long = server_retry_delay(&headers, Duration::from_secs(60))
            .expect_err("61s exceeds the 60s cap");
        assert_eq!(too_long.requested, Duration::from_secs(61));
        assert_eq!(too_long.cap, Duration::from_secs(60));

        let mut ms_headers = HeaderMap::new();
        ms_headers.insert("retry-after-ms", "60001".parse().expect("static value"));
        assert!(server_retry_delay(&ms_headers, Duration::from_secs(60)).is_err());
    }

    #[test]
    fn backoff_delay_stays_within_jitter_bounds() {
        for retry_index in [0u32, 1, 2, 3, 4, 10, 40] {
            let base = (BACKOFF_START * 2f64.powi(retry_index.min(31) as i32))
                .min(BACKOFF_CAP.as_secs_f64());
            let lower = Duration::from_secs_f64(base * JITTER_MIN);
            let upper = Duration::from_secs_f64(base);

            let jittered = backoff_delay(retry_index, JITTER_MIN);
            assert!(
                jittered >= lower && jittered <= upper,
                "retry {retry_index}: {jittered:?} not within [{lower:?}, {upper:?}]"
            );
            let full = backoff_delay(retry_index, 1.0);
            assert_eq!(full, upper, "jitter factor 1.0 yields the raw backoff");
        }
    }

    #[test]
    fn backoff_delay_growth_matches_the_sdk_constants() {
        assert_eq!(backoff_delay(0, 1.0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1, 1.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, 1.0), Duration::from_secs(2));
        assert_eq!(backoff_delay(4, 1.0), Duration::from_secs(8));
        assert_eq!(
            backoff_delay(9, 1.0),
            Duration::from_secs(8),
            "capped at 8s"
        );
    }

    #[tokio::test]
    async fn execute_retries_retryable_statuses_until_success() {
        let config = RetryConfig {
            max_retries: 3,
            max_server_delay: Duration::from_secs(60),
        };
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();

        let outcome: std::result::Result<(), Error> = config
            .execute(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Err(status_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    &[("retry-after-ms", "1")],
                )))
            })
            .await;

        let error = outcome.expect_err("all attempts fail");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "3 retries after the initial attempt"
        );
        assert!(
            matches!(error, Error::NonSuccessResponse { status, .. } if status == StatusCode::TOO_MANY_REQUESTS),
            "the final attempt's error is surfaced verbatim"
        );
    }

    #[tokio::test]
    async fn execute_fails_fast_on_non_retryable_errors() {
        let config = RetryConfig::default();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();

        let outcome: std::result::Result<(), Error> = config
            .execute(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Err(status_error(StatusCode::UNAUTHORIZED, &[])))
            })
            .await;

        assert!(outcome.is_err());
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "non-retryable 4xx must not be re-attempted"
        );
    }

    #[tokio::test]
    async fn execute_returns_success_immediately() {
        let config = RetryConfig::default();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();

        let value = config
            .execute(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok::<_, Error>(7))
            })
            .await
            .expect("success on first attempt");

        assert_eq!(value, 7);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn execute_fails_loudly_when_server_delay_exceeds_the_cap() {
        let config = RetryConfig::default();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();

        let started = Instant::now();
        let outcome: std::result::Result<(), Error> = config
            .execute(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Err(status_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    &[("retry-after", "120")],
                )))
            })
            .await;

        let error = outcome.expect_err("oversized server delay must fail");
        assert!(
            matches!(
                &error,
                Error::RetryDelayTooLong { requested, cap, .. }
                    if *requested == Duration::from_secs(120)
                        && *cap == Duration::from_secs(60)
            ),
            "error should name the requested delay, got: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must fail instead of honoring the 120s delay"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no second attempt after the loud failure"
        );
    }

    #[tokio::test]
    async fn execute_with_zero_retries_disables_retrying() {
        let config = RetryConfig {
            max_retries: 0,
            max_server_delay: Duration::from_secs(60),
        };
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();

        let outcome: std::result::Result<(), Error> = config
            .execute(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Err(status_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &[("retry-after-ms", "1")],
                )))
            })
            .await;

        assert!(outcome.is_err());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
