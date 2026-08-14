//! Helpers to handle connection delays when receiving errors

use super::Error;
use std::time::Duration;

pub trait RetryPolicy {
    /// Submit a new retry delay based on the [`enum@Error`], last retry number and duration, if
    /// available. A policy may also return `None` if it does not want to retry
    fn retry(&self, error: &Error, last_retry: Option<(usize, Duration)>) -> Option<Duration>;

    /// Set a new reconnection time if received from an event
    fn set_reconnection_time(&mut self, duration: Duration);
}

/// A [`RetryPolicy`] which backs off exponentially
#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    /// The start of the backoff
    pub start: Duration,
    /// The factor of which to backoff by
    pub factor: f64,
    /// The maximum duration to delay
    pub max_duration: Option<Duration>,
    /// The maximum number of retries before giving up
    pub max_retries: Option<usize>,
}

impl ExponentialBackoff {
    /// Create a new exponential backoff retry policy
    pub const fn new(
        start: Duration,
        factor: f64,
        max_duration: Option<Duration>,
        max_retries: Option<usize>,
    ) -> Self {
        Self {
            start,
            factor,
            max_duration,
            max_retries,
        }
    }
}

impl RetryPolicy for ExponentialBackoff {
    fn retry(&self, _error: &Error, last_retry: Option<(usize, Duration)>) -> Option<Duration> {
        if let Some((retry_num, last_duration)) = last_retry {
            if self
                .max_retries
                .is_none_or(|max_retries| retry_num < max_retries)
            {
                let duration = last_duration.mul_f64(self.factor);
                if let Some(max_duration) = self.max_duration {
                    Some(duration.min(max_duration))
                } else {
                    Some(duration)
                }
            } else {
                None
            }
        } else {
            Some(self.start)
        }
    }
    fn set_reconnection_time(&mut self, duration: Duration) {
        self.start = duration;
        if let Some(max_duration) = self.max_duration {
            self.max_duration = Some(max_duration.max(duration))
        }
    }
}

/// The default [`RetryPolicy`] when initializing an event source
pub const DEFAULT_RETRY: ExponentialBackoff = ExponentialBackoff::new(
    Duration::from_millis(300),
    2.,
    Some(Duration::from_secs(5)),
    None,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_all_fields() {
        let policy = ExponentialBackoff::new(
            Duration::from_millis(10),
            3.,
            Some(Duration::from_secs(1)),
            Some(2),
        );
        assert_eq!(policy.start, Duration::from_millis(10));
        assert_eq!(policy.factor, 3.);
        assert_eq!(policy.max_duration, Some(Duration::from_secs(1)));
        assert_eq!(policy.max_retries, Some(2));
    }

    #[test]
    fn first_retry_returns_start_duration() {
        let policy = ExponentialBackoff::new(Duration::from_millis(100), 2., None, None);
        assert_eq!(
            policy.retry(&Error::StreamEnded, None),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn subsequent_retries_scale_by_factor() {
        let policy = ExponentialBackoff::new(Duration::from_millis(100), 2., None, None);
        assert_eq!(
            policy.retry(&Error::StreamEnded, Some((1, Duration::from_millis(100)))),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            policy.retry(&Error::StreamEnded, Some((2, Duration::from_millis(200)))),
            Some(Duration::from_millis(400))
        );
    }

    #[test]
    fn retries_clamp_to_max_duration() {
        let policy = ExponentialBackoff::new(
            Duration::from_millis(100),
            2.,
            Some(Duration::from_millis(300)),
            None,
        );
        assert_eq!(
            policy.retry(&Error::StreamEnded, Some((1, Duration::from_millis(200)))),
            Some(Duration::from_millis(300))
        );
    }

    #[test]
    fn retries_stop_once_max_retries_reached() {
        let policy = ExponentialBackoff::new(Duration::from_millis(100), 2., None, Some(2));
        // retry_num 1 < max_retries 2: keep going
        assert_eq!(
            policy.retry(&Error::StreamEnded, Some((1, Duration::from_millis(100)))),
            Some(Duration::from_millis(200))
        );
        // retry_num 2 == max_retries 2: give up
        assert_eq!(
            policy.retry(&Error::StreamEnded, Some((2, Duration::from_millis(200)))),
            None
        );
    }

    #[test]
    fn default_retry_never_exhausts_and_starts_at_300ms() {
        assert_eq!(
            DEFAULT_RETRY.retry(&Error::StreamEnded, None),
            Some(Duration::from_millis(300))
        );
        // max_retries is None, so the backoff continues indefinitely (clamped by max_duration).
        assert_eq!(
            DEFAULT_RETRY.retry(&Error::StreamEnded, Some((100, Duration::from_secs(5)))),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn set_reconnection_time_updates_start() {
        let mut policy = ExponentialBackoff::new(Duration::from_millis(100), 2., None, None);
        policy.set_reconnection_time(Duration::from_secs(2));
        assert_eq!(policy.start, Duration::from_secs(2));
        // Without a max_duration, none is introduced.
        assert_eq!(policy.max_duration, None);
        // The next retry cycle uses the new reconnection time.
        assert_eq!(
            policy.retry(&Error::StreamEnded, None),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn set_reconnection_time_raises_max_duration_when_needed() {
        let mut policy = ExponentialBackoff::new(
            Duration::from_millis(100),
            2.,
            Some(Duration::from_secs(1)),
            None,
        );
        // New reconnection time above the cap: cap is raised so it is honored.
        policy.set_reconnection_time(Duration::from_secs(2));
        assert_eq!(policy.max_duration, Some(Duration::from_secs(2)));
        assert_eq!(policy.start, Duration::from_secs(2));

        // New reconnection time below the cap: cap is unchanged.
        policy.set_reconnection_time(Duration::from_millis(500));
        assert_eq!(policy.max_duration, Some(Duration::from_secs(2)));
        assert_eq!(policy.start, Duration::from_millis(500));
    }
}
