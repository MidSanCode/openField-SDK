//! Retry policy shared by every binding.
//!
//! `bots.md` §15.6 asks for exponential backoff on `5xx`, hour-window waits on
//! `429`, and explicitly *no* blind retries on `401`/`403`. [`RetryPolicy::run`]
//! implements exactly that and is reused by the sidecar and both language bindings
//! so the behaviour cannot drift between them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{SdkError, SdkError as Error};

/// Exponential backoff with jitter, gated on [`SdkError::is_retryable`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryPolicy {
    /// Total attempts, including the first one. `1` disables retrying.
    pub max_attempts: u32,
    /// Delay before the second attempt, in milliseconds.
    pub base_delay_ms: u64,
    /// Ceiling for the computed delay, in milliseconds.
    pub max_delay_ms: u64,
    /// Multiplier applied per attempt.
    pub multiplier: f64,
    /// Fraction of the delay randomised away (0.0 disables jitter).
    pub jitter: f64,
    /// When the server sends `Retry-After`, wait that long instead (capped by `max_delay_ms`).
    pub respect_retry_after: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            multiplier: 2.0,
            jitter: 0.25,
            respect_retry_after: true,
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// Builder: set the attempt ceiling.
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    /// Builder: set the first delay.
    pub fn with_base_delay(mut self, delay: Duration) -> Self {
        self.base_delay_ms = delay.as_millis() as u64;
        self
    }

    /// Verify the policy is usable. Called from language bindings before a run.
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.max_attempts == 0 {
            return Err(SdkError::InvalidArgument {
                message: "retry.max_attempts must be >= 1".into(),
            });
        }
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(SdkError::InvalidArgument {
                message: "retry.multiplier must be finite and >= 1.0".into(),
            });
        }
        if !(0.0..=1.0).contains(&self.jitter) || !self.jitter.is_finite() {
            return Err(SdkError::InvalidArgument {
                message: "retry.jitter must be within 0.0..=1.0".into(),
            });
        }
        if self.base_delay_ms > self.max_delay_ms {
            return Err(SdkError::InvalidArgument {
                message: "retry.base_delay_ms must be <= retry.max_delay_ms".into(),
            });
        }
        Ok(())
    }

    /// Delay to wait before making attempt number `attempt` (1-based, so attempt 1
    /// is the first call and never waits).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let exponent = (attempt - 2).min(32) as i32;
        let scaled = (self.base_delay_ms as f64) * self.multiplier.powi(exponent);
        let capped = scaled.min(self.max_delay_ms as f64).max(0.0);
        let millis = if self.jitter > 0.0 {
            let spread = capped * self.jitter;
            let unit = pseudo_random_unit();
            (capped - spread + 2.0 * spread * unit).clamp(0.0, self.max_delay_ms as f64)
        } else {
            capped
        };
        Duration::from_millis(millis.round() as u64)
    }

    /// Run `op`, retrying per this policy. `sleep` is injected so tests stay fast.
    pub fn run_with<T, F, S>(&self, mut op: F, mut sleep: S) -> crate::error::Result<T>
    where
        F: FnMut(u32) -> crate::error::Result<T>,
        S: FnMut(Duration),
    {
        let mut attempt = 1u32;
        loop {
            match op(attempt) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    if attempt >= self.max_attempts || !err.is_retryable() {
                        return Err(err);
                    }
                    let mut delay = self.delay_for_attempt(attempt + 1);
                    if self.respect_retry_after {
                        if let Some(secs) = err.retry_after_secs() {
                            let hinted = Duration::from_secs(secs);
                            let cap = Duration::from_millis(self.max_delay_ms);
                            delay = hinted.min(cap);
                        }
                    }
                    sleep(delay);
                    attempt += 1;
                }
            }
        }
    }

    /// Run `op`, sleeping with [`std::thread::sleep`].
    pub fn run<T, F>(&self, op: F) -> crate::error::Result<T>
    where
        F: FnMut(u32) -> crate::error::Result<T>,
    {
        self.run_with(op, |delay| {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        })
    }
}

/// Jitter source that avoids pulling in `rand`. Cheap, non-cryptographic, and
/// only used to de-synchronise retries between processes.
fn pseudo_random_unit() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let mut x = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ nanos;
    // splitmix64 finaliser
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// Backoff schedule for WebSocket reconnection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectPolicy {
    /// Delay before the first reconnection attempt, in milliseconds.
    pub initial_delay_ms: u64,
    /// Ceiling for the delay, in milliseconds.
    pub max_delay_ms: u64,
    /// Multiplier applied per consecutive failure.
    pub multiplier: f64,
    /// `None` retries forever; `Some(n)` stops the stream after `n` consecutive failures.
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay_ms: 1_000,
            max_delay_ms: 60_000,
            multiplier: 2.0,
            max_attempts: None,
        }
    }
}

impl ReconnectPolicy {
    /// Delay before reconnection number `attempt` (1-based).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(32) as i32;
        let scaled = (self.initial_delay_ms as f64) * self.multiplier.powi(exponent);
        Duration::from_millis(scaled.clamp(0.0, self.max_delay_ms as f64) as u64)
    }

    /// Whether another reconnection attempt is allowed.
    pub fn allows(&self, consecutive_failures: u32) -> bool {
        match self.max_attempts {
            Some(max) => consecutive_failures < max,
            None => true,
        }
    }

    /// Verify the policy is usable.
    pub fn validate(&self) -> crate::error::Result<()> {
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(Error::InvalidArgument {
                message: "reconnect.multiplier must be finite and >= 1.0".into(),
            });
        }
        if self.initial_delay_ms > self.max_delay_ms {
            return Err(Error::InvalidArgument {
                message: "reconnect.initial_delay_ms must be <= reconnect.max_delay_ms".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiErrorBody;

    fn http(status: u16, retry_after: Option<u64>) -> SdkError {
        SdkError::Http {
            status,
            body: ApiErrorBody::default(),
            retry_after_secs: retry_after,
        }
    }

    #[test]
    fn default_policy_is_valid_and_attempt_1_never_waits() {
        let policy = RetryPolicy::default();
        policy.validate().unwrap();
        assert_eq!(policy.delay_for_attempt(1), Duration::ZERO);
        assert!(policy.delay_for_attempt(2) >= Duration::from_millis(500 - 125));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let policy = RetryPolicy {
            jitter: 0.0,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(500));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(1000));
        assert_eq!(policy.delay_for_attempt(4), Duration::from_millis(2000));
        let capped = RetryPolicy {
            max_delay_ms: 1500,
            jitter: 0.0,
            ..RetryPolicy::default()
        };
        assert_eq!(capped.delay_for_attempt(5), Duration::from_millis(1500));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let policy = RetryPolicy::default();
        for _ in 0..200 {
            let d = policy.delay_for_attempt(2);
            assert!(d >= Duration::from_millis(375) && d <= Duration::from_millis(625));
        }
    }

    #[test]
    fn retries_only_retryable_errors() {
        let mut calls = Vec::new();
        let policy = RetryPolicy::default();
        let result: crate::error::Result<()> = policy.run_with(
            |attempt| {
                calls.push(attempt);
                Err(http(500, None))
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, vec![1, 2, 3], "5xx retried up to max_attempts");

        let mut forbidden_calls = Vec::new();
        let result: crate::error::Result<()> = policy.run_with(
            |attempt| {
                forbidden_calls.push(attempt);
                Err(http(403, None))
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(forbidden_calls, vec![1], "403 must not be retried");
    }

    #[test]
    fn succeeds_after_transient_failures() {
        let policy = RetryPolicy::default();
        let mut slept = Vec::new();
        let value = policy
            .run_with(
                |attempt| {
                    if attempt < 3 {
                        Err(http(503, None))
                    } else {
                        Ok(attempt)
                    }
                },
                |d| slept.push(d),
            )
            .unwrap();
        assert_eq!(value, 3);
        assert_eq!(slept.len(), 2);
    }

    #[test]
    fn retry_after_overrides_backoff() {
        let policy = RetryPolicy {
            max_delay_ms: 120_000,
            ..RetryPolicy::default()
        };
        let mut slept = Vec::new();
        let _: crate::error::Result<()> =
            policy.run_with(|_| Err(http(429, Some(42))), |d| slept.push(d));
        // Three attempts means two waits, and both follow Retry-After rather than the
        // 500ms/1000ms exponential schedule.
        assert_eq!(
            slept,
            vec![Duration::from_secs(42), Duration::from_secs(42)]
        );
    }

    #[test]
    fn retry_after_is_capped() {
        let policy = RetryPolicy {
            max_delay_ms: 1_000,
            ..RetryPolicy::default()
        };
        let mut slept = Vec::new();
        let _: crate::error::Result<()> =
            policy.run_with(|_| Err(http(429, Some(3600))), |d| slept.push(d));
        assert_eq!(
            slept,
            vec![Duration::from_millis(1_000), Duration::from_millis(1_000)],
            "a one-hour Retry-After must be clamped to max_delay_ms"
        );
    }

    #[test]
    fn none_policy_never_retries() {
        let mut calls = 0;
        let _: crate::error::Result<()> = RetryPolicy::none().run(|_| {
            calls += 1;
            Err(http(500, None))
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn rejects_impossible_policies() {
        assert!(RetryPolicy {
            max_attempts: 0,
            ..RetryPolicy::default()
        }
        .validate()
        .is_err());
        assert!(RetryPolicy {
            multiplier: 0.5,
            ..RetryPolicy::default()
        }
        .validate()
        .is_err());
        assert!(RetryPolicy {
            jitter: 1.5,
            ..RetryPolicy::default()
        }
        .validate()
        .is_err());
        assert!(RetryPolicy {
            base_delay_ms: 5_000,
            max_delay_ms: 1_000,
            ..RetryPolicy::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn reconnect_schedule() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(1_000));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(2_000));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(4_000));
        assert_eq!(policy.delay_for_attempt(99), Duration::from_millis(60_000));
        assert!(policy.allows(9_999));
        let bounded = ReconnectPolicy {
            max_attempts: Some(3),
            ..ReconnectPolicy::default()
        };
        assert!(bounded.allows(2));
        assert!(!bounded.allows(3));
    }

    #[test]
    fn policy_round_trips_through_json_for_the_sidecar() {
        let policy = RetryPolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let back: RetryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, policy);
        // Partial JSON must fill in defaults rather than fail.
        let partial: RetryPolicy = serde_json::from_str(r#"{"max_attempts":5}"#).unwrap();
        assert_eq!(partial.max_attempts, 5);
        assert_eq!(partial.base_delay_ms, 500);
    }
}
