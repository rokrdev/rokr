//! Acceptance tests for `ResilientProvider` (ticket 25): a decorator that
//! retries retryable provider failures and never retries auth/validation
//! failures. Exercises `ResilientProvider` through the public `Provider`
//! trait against a scripted fake — no network involved.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rokr_provider::{Message, Provider, ProviderError, ResilientProvider, ToolSpec, Usage};

/// A retry policy with near-zero delays, so acceptance tests exercise the
/// real retry/backoff/cap logic without incurring real sleep time that
/// would slow the suite down.
fn fast_test_policy() -> rokr_provider::resilience::RetryPolicy {
    rokr_provider::resilience::RetryPolicy {
        max_attempts: 5,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(5),
        max_elapsed: std::time::Duration::from_secs(5),
    }
}

/// A `Provider` whose `send` returns a pre-programmed sequence of results,
/// one per call, and counts how many times it was called. Panics if called
/// more times than results were scripted — that's a signal the decorator
/// retried when it shouldn't have (or retried more times than expected).
///
/// `ResilientProvider::new` takes its inner provider by value, so this is
/// `Clone` (cheaply, via an inner `Arc`) so the test can hand one clone to
/// the decorator and keep another to inspect `calls()` afterward — both
/// clones share the same underlying state.
#[derive(Clone)]
struct ScriptedProvider {
    inner: Arc<ScriptedProviderState>,
}

struct ScriptedProviderState {
    responses: Mutex<VecDeque<Result<(Message, Usage), ProviderError>>>,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    fn new(responses: Vec<Result<(Message, Usage), ProviderError>>) -> Self {
        Self {
            inner: Arc::new(ScriptedProviderState {
                responses: Mutex::new(responses.into_iter().collect()),
                call_count: AtomicUsize::new(0),
            }),
        }
    }

    fn calls(&self) -> usize {
        self.inner.call_count.load(Ordering::SeqCst)
    }
}

impl Provider for ScriptedProvider {
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
            .expect("ScriptedProvider called more times than responses were scripted")
    }
}

#[tokio::test]
async fn resilient_provider_retries_retryable_error_then_succeeds() {
    let fake = ScriptedProvider::new(vec![
        Err(ProviderError::UnexpectedStatus {
            status: 503,
            body: "temporarily unavailable".to_string(),
        }),
        Ok((Message::assistant_text("recovered"), Usage::default())),
    ]);

    let resilient = ResilientProvider::with_policy(fake.clone(), fast_test_policy());

    let messages = vec![Message::user_text("hello")];
    let result = resilient.send(&messages, &[]).await;

    let (message, _usage) = result.expect(
        "ResilientProvider should retry a retryable error and return the second attempt's success",
    );
    assert_eq!(message.text(), "recovered");
    assert_eq!(
        fake.calls(),
        2,
        "the fake provider should have been called exactly twice: once for the failure, once for the retry"
    );
}

#[tokio::test]
async fn resilient_provider_never_retries_auth_validation_error() {
    let fake = ScriptedProvider::new(vec![Err(ProviderError::UnexpectedStatus {
        status: 401,
        body: "invalid api key".to_string(),
    })]);

    let resilient = ResilientProvider::with_policy(fake.clone(), fast_test_policy());

    let messages = vec![Message::user_text("hello")];
    let result = resilient.send(&messages, &[]).await;

    assert!(
        result.is_err(),
        "a 4xx auth/validation error should be returned as an error, not swallowed"
    );
    assert_eq!(
        fake.calls(),
        1,
        "a 4xx auth/validation error must never be retried — the fake should be called exactly once"
    );
}

#[tokio::test]
async fn resilient_provider_waits_at_least_the_retry_after_duration_before_retrying() {
    let retry_after = std::time::Duration::from_millis(40);

    let fake = ScriptedProvider::new(vec![
        Err(ProviderError::RateLimited {
            retry_after: Some(retry_after),
        }),
        Ok((Message::assistant_text("recovered after rate limit"), Usage::default())),
    ]);

    let resilient = ResilientProvider::with_policy(fake.clone(), fast_test_policy());

    let messages = vec![Message::user_text("hello")];
    let started = std::time::Instant::now();
    let result = resilient.send(&messages, &[]).await;
    let elapsed = started.elapsed();

    let (message, _usage) = result.expect(
        "ResilientProvider should retry after honoring the retry-after duration and succeed",
    );
    assert_eq!(message.text(), "recovered after rate limit");
    assert_eq!(fake.calls(), 2);
    assert!(
        elapsed >= retry_after,
        "expected to wait at least the retry-after duration ({retry_after:?}) before retrying, only waited {elapsed:?}"
    );
}
