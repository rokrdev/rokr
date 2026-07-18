//! OpenAI-compatible `Provider` implementation (ADR 0003).
//!
//! Owns its own wire DTOs and converts `rokr_core::Message` <-> DTO at the
//! edge; `rokr-core` never sees an OpenAI-shaped struct (ADR 0006).

use serde::{Deserialize, Serialize};

use rokr_core::{ContentBlock, Message, Role};

use crate::{Provider, ProviderError};

/// OpenAI chat-completions wire format. Owned entirely by this module — see
/// ADR 0006: `rokr-core` types never carry provider-specific serde shapes.
#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<WireMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<WireChoice>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
}

fn role_to_wire(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn wire_to_role(wire: &str) -> Role {
    match wire {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        // OpenAI's `tool` role and anything unrecognized has no rokr-core
        // equivalent yet (ADR 0006); treat it as a user-authored turn until
        // Phase 2 adds tool-result content blocks.
        _ => Role::User,
    }
}

fn message_to_wire(message: &Message) -> WireMessage {
    WireMessage {
        role: role_to_wire(message.role).to_string(),
        content: message.text(),
    }
}

fn wire_to_message(wire: WireMessage) -> Message {
    Message {
        role: wire_to_role(&wire.role),
        content: vec![ContentBlock::Text {
            text: wire.content,
            cache_control: None,
        }],
    }
}

/// Environment variables read by [`OpenAiProvider::from_env`].
pub const ENV_BASE_URL: &str = "ROKR_OPENAI_BASE_URL";
pub const ENV_MODEL: &str = "ROKR_OPENAI_MODEL";
pub const ENV_API_KEY: &str = "ROKR_OPENAI_API_KEY";

/// An OpenAI-compatible chat completions provider. Configured with a base
/// URL, model name, and API key so any OpenAI-compatible endpoint (OpenAI
/// itself, or a compatible proxy) can be targeted.
pub struct OpenAiProvider {
    base_url: String,
    model: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Construct a provider with explicit config. Primarily for tests that
    /// point `base_url` at a mock server.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Construct a provider from `ROKR_OPENAI_BASE_URL`, `ROKR_OPENAI_MODEL`,
    /// and `ROKR_OPENAI_API_KEY`. Returns [`ProviderError::MissingEnvVar`] if
    /// any is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        let base_url = std::env::var(ENV_BASE_URL)
            .map_err(|_| ProviderError::MissingEnvVar(ENV_BASE_URL))?;
        let model =
            std::env::var(ENV_MODEL).map_err(|_| ProviderError::MissingEnvVar(ENV_MODEL))?;
        let api_key = std::env::var(ENV_API_KEY)
            .map_err(|_| ProviderError::MissingEnvVar(ENV_API_KEY))?;

        Ok(Self::new(base_url, model, api_key))
    }
}

impl Provider for OpenAiProvider {
    type Error = ProviderError;

    async fn send(&self, messages: &[Message]) -> Result<Message, ProviderError> {
        let request_body = ChatCompletionRequest {
            model: self.model.clone(),
            messages: messages.iter().map(message_to_wire).collect(),
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            return Err(ProviderError::UnexpectedStatus {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: ChatCompletionResponse = serde_json::from_str(&body)?;
        let choice = parsed.choices.into_iter().next().ok_or(ProviderError::EmptyResponse)?;

        Ok(wire_to_message(choice.message))
    }
}
