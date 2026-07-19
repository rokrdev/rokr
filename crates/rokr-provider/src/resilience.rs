//! A [`Provider`] decorator that retries retryable failures with exponential
//! backoff and jitter (ticket 25). Lives here, not in `rokr-core`, so the
//! `Provider` port stays abstract over errors (ADR 0009) while this
//! decorator gets to inspect concrete `ProviderError` variants via
//! [`ProviderError::retry_hint`].
//!
//! Same-provider retry only: this decorator always resends the identical
//! `messages`/`tools` payload to the same wrapped provider, which is what
//! keeps a retry cache-safe (see the PRD's "Failover and cache stability"
//! section) — it never reconstructs or mutates the payload between
//! attempts. Cross-provider failover is out of scope for this decorator
//! (ticket 26).

use std::time::{Duration, Instant};

use crate::{Message, Provider, ProviderError, RetryHint, ToolSpec, Usage};

/// Retry policy knobs, exposed so callers (and tests) can inject a
/// fast/deterministic policy instead of the production defaults — the
/// acceptance tests use a near-zero-delay policy so the suite stays fast
/// without needing to fake the clock.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first), after which the
    /// last error is returned even if it was classified retryable.
    pub max_attempts: u32,
    /// Base delay for the exponential backoff computation (attempt 1's
    /// delay, before jitter).
    pub base_delay: Duration,
    /// Upper bound on the computed backoff delay, before jitter, so
    /// exponential growth doesn't run away on a long retry sequence.
    pub max_delay: Duration,
    /// Total wall-clock budget across all attempts. If the elapsed time
    /// plus the next computed delay would exceed this, the last error is
    /// returned instead of waiting and retrying again.
    pub max_elapsed: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            max_elapsed: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff (`base_delay * 2^(attempt - 1)`, capped at
    /// `max_delay`) with full jitter: the actual delay is a random duration
    /// in `[0, capped_delay]`, per the standard "full jitter" strategy for
    /// avoiding synchronized retry storms across clients.
    fn backoff_delay(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(31); // avoid u32 shift overflow
        let exponential = self.base_delay.saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
        let capped = exponential.min(self.max_delay);

        let jitter_range_ms = capped.as_millis().min(u128::from(u64::MAX)) as u64;
        if jitter_range_ms == 0 {
            return Duration::ZERO;
        }
        let jittered_ms = rand::random::<u64>() % (jitter_range_ms + 1);
        Duration::from_millis(jittered_ms)
    }
}

/// Wraps a `Provider<Error = ProviderError>` and retries retryable failures
/// according to a [`RetryPolicy`]. Never retries an error [`ProviderError::retry_hint`]
/// classifies as [`RetryHint::NonRetryable`] (auth/validation 4xx errors).
/// Honors a rate-limited response's server-provided retry-after duration
/// ([`RetryHint::RetryAfter`]) instead of the computed exponential delay
/// when present.
pub struct ResilientProvider<P> {
    inner: P,
    policy: RetryPolicy,
}

impl<P> ResilientProvider<P> {
    /// Wraps `inner` with the default [`RetryPolicy`].
    pub fn new(inner: P) -> Self {
        Self::with_policy(inner, RetryPolicy::default())
    }

    /// Wraps `inner` with an explicit [`RetryPolicy`] — primarily for tests
    /// that need fast, deterministic retry timing.
    pub fn with_policy(inner: P, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

impl<P> Provider for ResilientProvider<P>
where
    P: Provider<Error = ProviderError>,
{
    type Error = ProviderError;

    async fn send(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<(Message, Usage), ProviderError> {
        let start = Instant::now();
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;

            let error = match self.inner.send(messages, tools).await {
                Ok(result) => return Ok(result),
                Err(error) => error,
            };

            let delay = match error.retry_hint() {
                RetryHint::NonRetryable => return Err(error),
                RetryHint::Retryable => self.policy.backoff_delay(attempt),
                RetryHint::RetryAfter(retry_after) => retry_after,
            };

            if attempt >= self.policy.max_attempts {
                return Err(error);
            }
            if start.elapsed().saturating_add(delay) > self.policy.max_elapsed {
                return Err(error);
            }

            tokio::time::sleep(delay).await;
        }
    }
}
