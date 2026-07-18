//! The `Provider` trait and provider implementations (OpenAI-compatible first, Anthropic later).

pub mod openai;

/// The `Provider` trait now lives in `rokr-core` (see its doc comment there
/// for why: `single_turn` needs to be generic over it without rokr-core
/// depending on rokr-provider, which would cycle). Re-exported here so
/// existing call sites (`rokr_provider::Provider`) keep working unchanged.
/// Concrete implementations still live in this crate, one module per
/// provider (ADR 0003 as refined by 0009).
pub use rokr_core::Provider;

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

    #[error("failed to deserialize provider response: {0}")]
    Deserialize(#[from] serde_json::Error),

    #[error("provider response contained no choices")]
    EmptyResponse,
}

pub use openai::OpenAiProvider;
