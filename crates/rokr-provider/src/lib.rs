//! The `Provider` trait and provider implementations (OpenAI-compatible first, Anthropic later).

pub mod openai;

use rokr_core::Message;

/// A backend capable of turning a conversation (ordered `Message`s) into the
/// next assistant `Message`. One module per concrete implementation (ADR
/// 0003); each implementation owns its own wire format and converts to/from
/// `rokr_core::Message` at the edge (ADR 0006).
pub trait Provider {
    async fn send(&self, messages: &[Message]) -> Result<Message, ProviderError>;
}

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
