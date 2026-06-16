//! Request executor — retry/backoff + secret redaction, ported from `packages/llm/src/route/
//! executor.ts`. Wraps a transport attempt so transient provider failures are retried with
//! exponential backoff + jitter (honoring `Retry-After`), and so secrets never reach logs/errors.
//!
//! The retry *policy* ([`RetryPolicy::next_delay`]) is pure and unit-tested per case; [`execute`] is
//! the async loop (tested with a 0-delay policy so it doesn't really sleep).

use std::future::Future;
use std::time::Duration;

use crate::LlmError;

/// Exponential-backoff retry policy.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts before giving up (e.g. 5).
    pub max_attempts: u32,
    /// Base backoff; attempt *n* waits `base * 2^(n-1)`, capped at `cap`.
    pub base: Duration,
    /// Maximum backoff.
    pub cap: Duration,
    /// Add up to +50% jitter to each delay (mitigates thundering herd).
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(60),
            jitter: true,
        }
    }
}

/// Whether an error is worth retrying: provider `Status` per its `retryable` flag, network errors
/// always, decode errors never.
fn is_retryable(error: &LlmError) -> bool {
    match error {
        LlmError::Status { retryable, .. } => *retryable,
        LlmError::Http(_) => true,
        LlmError::Decode(_) => false,
    }
}

/// The `Retry-After` delay carried by a `Status` error, if any.
fn retry_after(error: &LlmError) -> Option<Duration> {
    match error {
        LlmError::Status {
            retry_after: Some(seconds),
            ..
        } => Some(Duration::from_secs(*seconds)),
        _ => None,
    }
}

/// Add up to +50% jitter using a cheap time-seeded value (no `rand` dependency; backoff jitter does
/// not need cryptographic randomness).
fn apply_jitter(delay: Duration) -> Duration {
    let span = delay.as_millis() as u64 / 2;
    if span == 0 {
        return delay;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    delay + Duration::from_millis(seed % (span + 1))
}

impl RetryPolicy {
    /// The deterministic exponential backoff for `attempt` (1-based), capped at `cap`.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
        let millis = (self.base.as_millis() as u64).saturating_mul(factor);
        Duration::from_millis(millis).min(self.cap)
    }

    /// The delay to wait before retry after `attempt` attempts given `error`, or `None` to stop
    /// (non-retryable, or `max_attempts` reached). Honors `Retry-After` over the computed backoff.
    pub fn next_delay(&self, attempt: u32, error: &LlmError) -> Option<Duration> {
        if attempt >= self.max_attempts || !is_retryable(error) {
            return None;
        }
        let base = retry_after(error)
            .unwrap_or_else(|| self.backoff(attempt))
            .min(self.cap);
        Some(if self.jitter {
            apply_jitter(base)
        } else {
            base
        })
    }
}

/// Run `attempt` under `policy`, retrying retryable failures with backoff until it succeeds or the
/// policy stops. `attempt` is re-invoked for each try (it must be idempotent at the request level).
pub async fn execute<T, F, Fut>(policy: &RetryPolicy, mut attempt: F) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut tries = 0;
    loop {
        tries += 1;
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => match policy.next_delay(tries, &error) {
                Some(delay) => tokio::time::sleep(delay).await,
                None => return Err(error),
            },
        }
    }
}

/// Mask provider API-key tokens (Anthropic `sk-ant-…`) in `text` so secrets never reach logs or error
/// reports. (The `x-api-key` header value *is* this token, so this also covers an echoed header.)
pub fn redact_secrets(text: &str) -> String {
    const PREFIX: &str = "sk-ant-";
    let mut parts = text.split(PREFIX);
    let mut out = String::with_capacity(text.len());
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for segment in parts {
        out.push_str(PREFIX);
        out.push_str("***");
        // Drop the leading key-token characters; keep the rest of the segment.
        out.push_str(
            segment.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn status(code: u16, retryable: bool, retry_after: Option<u64>) -> LlmError {
        LlmError::Status {
            code,
            retryable,
            retry_after,
            message: "x".into(),
        }
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(60),
            jitter: false,
        };
        assert_eq!(policy.backoff(1), Duration::from_secs(2));
        assert_eq!(policy.backoff(2), Duration::from_secs(4));
        assert_eq!(policy.backoff(3), Duration::from_secs(8));
        // 2 * 2^9 = 1024s, capped at 60.
        assert_eq!(policy.backoff(10), Duration::from_secs(60));
    }

    #[test]
    fn next_delay_classifies_and_limits() {
        let policy = RetryPolicy {
            jitter: false,
            ..Default::default()
        };
        // Retryable below the cap → a backoff.
        assert_eq!(
            policy.next_delay(1, &status(429, true, None)),
            Some(Duration::from_secs(2))
        );
        // Non-retryable → stop.
        assert_eq!(policy.next_delay(1, &status(401, false, None)), None);
        // At max attempts → stop.
        assert_eq!(policy.next_delay(5, &status(503, true, None)), None);
        // Network errors are retryable.
        assert!(policy
            .next_delay(1, &LlmError::Http("reset".into()))
            .is_some());
        // Decode errors are not.
        assert_eq!(policy.next_delay(1, &LlmError::Decode("bad".into())), None);
    }

    #[test]
    fn retry_after_overrides_backoff() {
        let policy = RetryPolicy {
            jitter: false,
            ..Default::default()
        };
        // attempt 1 backoff would be 2s, but Retry-After=7 wins.
        assert_eq!(
            policy.next_delay(1, &status(429, true, Some(7))),
            Some(Duration::from_secs(7))
        );
    }

    #[tokio::test]
    async fn execute_retries_then_succeeds() {
        // 0-delay policy → no real sleeping.
        let policy = RetryPolicy {
            max_attempts: 5,
            base: Duration::ZERO,
            cap: Duration::ZERO,
            jitter: false,
        };
        let calls = AtomicU32::new(0);
        let result: Result<&str, LlmError> = execute(&policy, || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(status(429, true, None))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn execute_fails_fast_on_non_retryable() {
        let policy = RetryPolicy {
            base: Duration::ZERO,
            cap: Duration::ZERO,
            jitter: false,
            ..Default::default()
        };
        let calls = AtomicU32::new(0);
        let result: Result<&str, LlmError> = execute(&policy, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(status(401, false, None)) }
        })
        .await;
        assert!(matches!(result, Err(LlmError::Status { code: 401, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn execute_gives_up_after_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::ZERO,
            cap: Duration::ZERO,
            jitter: false,
        };
        let calls = AtomicU32::new(0);
        let result: Result<&str, LlmError> = execute(&policy, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(status(503, true, None)) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn redact_masks_api_keys() {
        assert_eq!(
            redact_secrets("auth sk-ant-api03-AbC_123-xyz done"),
            "auth sk-ant-*** done"
        );
        // Multiple keys, e.g. an echoed header + body.
        assert_eq!(
            redact_secrets("x-api-key: sk-ant-AAA and sk-ant-BBB"),
            "x-api-key: sk-ant-*** and sk-ant-***"
        );
        // No key → unchanged.
        assert_eq!(redact_secrets("nothing secret here"), "nothing secret here");
    }
}
