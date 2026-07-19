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
    /// Optional failover target, tried once (no retry loop of its own)
    /// after the primary's retries are exhausted. `None` by default, which
    /// keeps failover off unless a caller explicitly opts in via
    /// [`Self::with_secondary`] — see the PRD's "Failover and cache
    /// stability" section: cross-provider failover is cache-destroying, so
    /// it's a config-driven last resort, not the default.
    secondary: Option<P>,
}

impl<P> ResilientProvider<P> {
    /// Wraps `inner` with the default [`RetryPolicy`]. No secondary
    /// provider configured — failover is off.
    pub fn new(inner: P) -> Self {
        Self::with_policy(inner, RetryPolicy::default())
    }

    /// Wraps `inner` with an explicit [`RetryPolicy`] — primarily for tests
    /// that need fast, deterministic retry timing. No secondary provider
    /// configured — failover is off.
    pub fn with_policy(inner: P, policy: RetryPolicy) -> Self {
        Self {
            inner,
            policy,
            secondary: None,
        }
    }

    /// Configures a secondary provider to fail over to, once, after the
    /// primary's retries are exhausted. Cross-provider failover is
    /// cache-destroying (different tokenizer/cache namespace), so this is
    /// opt-in — without calling this, `ResilientProvider` behaves exactly
    /// as it did before ticket 26 (returns the primary's error unchanged).
    pub fn with_secondary(mut self, secondary: P) -> Self {
        self.secondary = Some(secondary);
        self
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
                return self.failover_or_err(messages, tools, error).await;
            }
            if start.elapsed().saturating_add(delay) > self.policy.max_elapsed {
                return self.failover_or_err(messages, tools, error).await;
            }

            tokio::time::sleep(delay).await;
        }
    }
}

impl<P> ResilientProvider<P>
where
    P: Provider<Error = ProviderError>,
{
    /// Called once the primary's retries are exhausted. Without a
    /// secondary configured, returns `primary_error` unchanged — this is
    /// what keeps failover off by default (ticket-25 behavior, byte for
    /// byte). With a secondary configured, emits a telemetry event marking
    /// the cache invalidation (cross-provider failover always misses the
    /// primary's prompt cache — see the PRD's "Failover and cache
    /// stability" section) and makes a single attempt against the
    /// secondary — no retry loop of its own — returning its result
    /// (success or error) directly.
    async fn failover_or_err(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        primary_error: ProviderError,
    ) -> Result<(Message, Usage), ProviderError> {
        let Some(secondary) = &self.secondary else {
            return Err(primary_error);
        };

        tracing::info!(
            primary_error = %primary_error,
            cache_invalidated = true,
            "provider retries exhausted, failing over to secondary provider"
        );

        secondary.send(messages, tools).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A minimal in-module fake `Provider`, scripted with a fixed sequence
    /// of results, one per call. Deliberately not reusing the acceptance
    /// test crate's `ScriptedProvider` (that's a separate crate for
    /// integration-test purposes) — this is a small white-box fake for a
    /// unit test co-located in this source file.
    #[derive(Clone)]
    struct FakeProvider {
        inner: Arc<FakeProviderState>,
    }

    struct FakeProviderState {
        responses: Mutex<VecDeque<Result<(Message, Usage), ProviderError>>>,
        call_count: AtomicUsize,
    }

    impl FakeProvider {
        fn new(responses: Vec<Result<(Message, Usage), ProviderError>>) -> Self {
            Self {
                inner: Arc::new(FakeProviderState {
                    responses: Mutex::new(responses.into_iter().collect()),
                    call_count: AtomicUsize::new(0),
                }),
            }
        }

        fn calls(&self) -> usize {
            self.inner.call_count.load(Ordering::SeqCst)
        }
    }

    impl Provider for FakeProvider {
        type Error = ProviderError;

        async fn send(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<(Message, Usage), ProviderError> {
            self.inner.call_count.fetch_add(1, Ordering::SeqCst);
            self.inner
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeProvider called more times than responses were scripted")
        }
    }

    #[tokio::test]
    async fn failover_disabled_by_default_when_no_secondary_configured() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            max_elapsed: Duration::from_secs(5),
        };

        let fake = FakeProvider::new(
            (0..policy.max_attempts)
                .map(|_| {
                    Err(ProviderError::UnexpectedStatus {
                        status: 503,
                        body: "primary down".to_string(),
                    })
                })
                .collect(),
        );

        let resilient = ResilientProvider::with_policy(fake.clone(), policy);
        assert!(
            resilient.secondary.is_none(),
            "failover must be off by default when no secondary is configured"
        );

        let messages = vec![Message::user_text("hello")];
        let result = resilient.send(&messages, &[]).await;

        match result {
            Err(ProviderError::UnexpectedStatus { status, .. }) => assert_eq!(status, 503),
            other => panic!("expected the primary's exhausted-retries error unchanged, got {other:?}"),
        }
        assert_eq!(
            fake.calls(),
            policy.max_attempts as usize,
            "no secondary configured means no failover attempt — the fake should be called exactly max_attempts times"
        );
    }
}
