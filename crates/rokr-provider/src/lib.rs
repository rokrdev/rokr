//! The `Provider` trait and provider implementations (OpenAI-compatible first, Anthropic later).

use std::time::Duration;

pub mod anthropic;
pub mod openai;
pub mod resilience;

/// The `Provider` trait now lives in `rokr-core` (see its doc comment there
/// for why: `single_turn` needs to be generic over it without rokr-core
/// depending on rokr-provider, which would cycle). Re-exported here so
/// existing call sites (`rokr_provider::Provider`) keep working unchanged.
/// Concrete implementations still live in this crate, one module per
/// provider (ADR 0003 as refined by 0009).
pub use rokr_core::{Message, Provider, ToolSpec, Usage};

/// Typed provider failures. Never panics: HTTP and deserialization failures
/// surface here instead.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("missing required environment variable: {0}")]
    MissingEnvVar(&'static str),

    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("unexpected response status {status}: {body}")]
    UnexpectedStatus { status: u16, body: String },

    #[error("rate limited{}", retry_after.map(|d| format!(" (retry after {:?})", d)).unwrap_or_default())]
    RateLimited { retry_after: Option<Duration> },

    #[error("failed to deserialize provider response: {0}")]
    Deserialize(#[from] serde_json::Error),

    #[error("provider response contained no choices")]
    EmptyResponse,
}

impl ProviderError {
    /// Classifies this error for a retry policy. See [`RetryHint`].
    ///
    /// - Transport-level failures (`Http`) are treated as retryable —
    ///   connection resets and timeouts surface as `reqwest::Error` and rokr
    ///   has no way to distinguish "transient network blip" from other
    ///   `reqwest::Error` causes at this layer, so all of them are retried.
    /// - `UnexpectedStatus` is retryable for 5xx (server-side, likely
    ///   transient) and non-retryable for any other 4xx (client error: bad
    ///   auth, bad request body, etc. — retrying won't help).
    /// - `RateLimited` honors the server's `retry-after` when present,
    ///   otherwise falls back to the caller's own backoff policy.
    /// - `MissingEnvVar`, `Deserialize`, and `EmptyResponse` are all
    ///   non-retryable: they indicate misconfiguration or a malformed
    ///   response, not a transient condition a retry would fix.
    pub fn retry_hint(&self) -> RetryHint {
        match self {
            ProviderError::Http(_) => RetryHint::Retryable,
            ProviderError::UnexpectedStatus { status, .. } => {
                if (500..600).contains(status) {
                    RetryHint::Retryable
                } else {
                    RetryHint::NonRetryable
                }
            }
            ProviderError::RateLimited { retry_after } => match retry_after {
                Some(duration) => RetryHint::RetryAfter(*duration),
                None => RetryHint::Retryable,
            },
            ProviderError::MissingEnvVar(_)
            | ProviderError::Deserialize(_)
            | ProviderError::EmptyResponse => RetryHint::NonRetryable,
        }
    }
}

/// How a [`ProviderError`] should be treated by a retry policy (see
/// `ResilientProvider` in this crate — ticket 25).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RetryHint {
    /// Never retry (e.g. auth or validation failures).
    NonRetryable,
    /// Safe to retry with the caller's own backoff policy.
    Retryable,
    /// Safe to retry, but wait at least this long first (e.g. a 429's
    /// server-provided `retry-after`).
    RetryAfter(Duration),
}

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;
pub use resilience::ResilientProvider;

pub const ENV_PROVIDER: &str = "ROKR_PROVIDER";

pub enum AnyProvider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
}

impl AnyProvider {
    /// Reads `ROKR_PROVIDER`: `"anthropic"` selects the Anthropic adapter;
    /// anything else (including unset) defaults to OpenAI.
    pub fn from_env() -> Result<Self, ProviderError> {
        match std::env::var(ENV_PROVIDER).as_deref() {
            Ok("anthropic") => Ok(AnyProvider::Anthropic(AnthropicProvider::from_env()?)),
            _ => Ok(AnyProvider::OpenAi(OpenAiProvider::from_env()?)),
        }
    }
}

impl Provider for AnyProvider {
    type Error = ProviderError;

    async fn send(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<(Message, Usage), ProviderError> {
        match self {
            AnyProvider::OpenAi(provider) => provider.send(messages, tools).await,
            AnyProvider::Anthropic(provider) => provider.send(messages, tools).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn any_provider_from_env_dispatches_to_anthropic_when_configured() {
        let _lock = ENV_GUARD.lock().unwrap();

        std::env::set_var(ENV_PROVIDER, "anthropic");
        std::env::set_var(anthropic::ENV_BASE_URL, "http://localhost:9");
        std::env::set_var(anthropic::ENV_MODEL, "claude-3-5-sonnet-20241022");
        std::env::set_var(anthropic::ENV_API_KEY, "test-key");

        let provider = AnyProvider::from_env()
            .expect("from_env should succeed with all required env vars set");

        assert!(
            matches!(provider, AnyProvider::Anthropic(_)),
            "ROKR_PROVIDER=anthropic should select the Anthropic adapter, not OpenAI"
        );

        std::env::remove_var(ENV_PROVIDER);
        std::env::remove_var(anthropic::ENV_BASE_URL);
        std::env::remove_var(anthropic::ENV_MODEL);
        std::env::remove_var(anthropic::ENV_API_KEY);
    }

    #[test]
    fn provider_error_classifies_retryable_vs_non_retryable_variants() {
        let server_error = ProviderError::UnexpectedStatus {
            status: 503,
            body: String::new(),
        };
        assert_eq!(server_error.retry_hint(), RetryHint::Retryable);

        let timeout_error = ProviderError::Http(
            reqwest::Client::new()
                .get("not a valid url")
                .build()
                .expect_err("a malformed URL should fail to build a request"),
        );
        assert_eq!(timeout_error.retry_hint(), RetryHint::Retryable);

        let auth_error = ProviderError::UnexpectedStatus {
            status: 401,
            body: String::new(),
        };
        assert_eq!(auth_error.retry_hint(), RetryHint::NonRetryable);

        let validation_error = ProviderError::UnexpectedStatus {
            status: 422,
            body: String::new(),
        };
        assert_eq!(validation_error.retry_hint(), RetryHint::NonRetryable);

        let rate_limited_with_hint = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(
            rate_limited_with_hint.retry_hint(),
            RetryHint::RetryAfter(Duration::from_secs(7))
        );

        let rate_limited_without_hint = ProviderError::RateLimited { retry_after: None };
        assert_eq!(rate_limited_without_hint.retry_hint(), RetryHint::Retryable);
    }
}
