//! Anthropic Messages API `Provider` implementation (Phase 4 tracer bullet).
//!
//! Owns its own wire DTOs and converts `rokr_core::Message` <-> DTO at the
//! edge; `rokr-core` never sees an Anthropic-shaped struct (ADR 0006).

use serde::{Deserialize, Serialize};

use rokr_core::{CacheControl, ContentBlock, Message, Role, ToolSpec, Usage};

use crate::{Provider, ProviderError};

#[derive(Debug, Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<WireSystemBlock>,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

#[derive(Debug, Serialize)]
struct WireSystemBlock {
    r#type: String, // always "text"
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<WireCacheControl>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireCacheControl {
    r#type: String, // always "ephemeral"
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMessage {
    role: String, // "user" | "assistant" only — system is hoisted out
    content: Vec<WireContentBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<WireCacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<WireCacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<WireCacheControl>,
    },
}

#[derive(Debug, Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<WireCacheControl>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<WireContentBlock>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn cache_control_to_wire(cc: &Option<CacheControl>) -> Option<WireCacheControl> {
    cc.as_ref().map(|cache_control| match cache_control.kind {
        rokr_core::CacheControlKind::Ephemeral => WireCacheControl {
            r#type: "ephemeral".to_string(),
            ttl: None,
        },
        rokr_core::CacheControlKind::Extended => WireCacheControl {
            r#type: "ephemeral".to_string(),
            ttl: Some("1h".to_string()),
        },
    })
}

fn content_block_to_wire(block: &ContentBlock) -> WireContentBlock {
    match block {
        ContentBlock::Text { text, cache_control } => WireContentBlock::Text {
            text: text.clone(),
            cache_control: cache_control_to_wire(cache_control),
        },
        ContentBlock::ToolUse {
            id,
            name,
            input,
            cache_control,
        } => WireContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: cache_control_to_wire(cache_control),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => WireContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
            cache_control: cache_control_to_wire(cache_control),
        },
    }
}

fn tool_spec_to_wire(spec: &ToolSpec) -> WireTool {
    WireTool {
        name: spec.name.clone(),
        description: spec.description.clone(),
        input_schema: spec.input_schema.clone(),
        cache_control: cache_control_to_wire(&spec.cache_control),
    }
}

fn message_to_wire(message: &Message) -> WireMessage {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        // A stray `Role::System` reaching this function is a can't-happen
        // after the leading-system-run hoisting in `send()` — map it to
        // "user" as a defensive fallback rather than panicking.
        Role::System => "user",
    };

    WireMessage {
        role: role.to_string(),
        content: message.content.iter().map(content_block_to_wire).collect(),
    }
}

/// Response parsing direction. Always sets `cache_control: None` — Anthropic
/// responses never carry cache_control on content blocks.
fn wire_content_block_to_content_block(block: WireContentBlock) -> ContentBlock {
    match block {
        WireContentBlock::Text { text, .. } => ContentBlock::Text {
            text,
            cache_control: None,
        },
        WireContentBlock::ToolUse { id, name, input, .. } => ContentBlock::ToolUse {
            id,
            name,
            input,
            cache_control: None,
        },
        WireContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control: None,
        },
    }
}

/// Environment variables read by [`AnthropicProvider::from_env`].
pub const ENV_BASE_URL: &str = "ROKR_ANTHROPIC_BASE_URL";
pub const ENV_MODEL: &str = "ROKR_ANTHROPIC_MODEL";
pub const ENV_API_KEY: &str = "ROKR_ANTHROPIC_API_KEY";

/// An Anthropic Messages API provider. Configured with a base URL, model
/// name, and API key.
pub struct AnthropicProvider {
    base_url: String,
    model: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
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

    /// Construct a provider from `ROKR_ANTHROPIC_BASE_URL`,
    /// `ROKR_ANTHROPIC_MODEL`, and `ROKR_ANTHROPIC_API_KEY`. Returns
    /// [`ProviderError::MissingEnvVar`] if any is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        let base_url =
            std::env::var(ENV_BASE_URL).map_err(|_| ProviderError::MissingEnvVar(ENV_BASE_URL))?;
        let model =
            std::env::var(ENV_MODEL).map_err(|_| ProviderError::MissingEnvVar(ENV_MODEL))?;
        let api_key =
            std::env::var(ENV_API_KEY).map_err(|_| ProviderError::MissingEnvVar(ENV_API_KEY))?;

        Ok(Self::new(base_url, model, api_key))
    }
}

impl Provider for AnthropicProvider {
    type Error = ProviderError;

    async fn send(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<(Message, Usage), ProviderError> {
        // System-message hoisting: Anthropic has no `system`-role message in
        // its `messages` array — leading system-role messages are lifted
        // into the top-level `system` array instead.
        let system_count = messages.iter().take_while(|m| m.role == Role::System).count();
        let (system_messages, rest) = messages.split_at(system_count);

        let system: Vec<WireSystemBlock> = system_messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text, cache_control } => Some(WireSystemBlock {
                    r#type: "text".to_string(),
                    text: text.clone(),
                    cache_control: cache_control_to_wire(cache_control),
                }),
                // System messages only ever carry Text blocks in practice.
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect();

        let request_body = MessagesRequest {
            model: self.model.clone(),
            // Fixed default (no ticket-specified value).
            max_tokens: 4096,
            system,
            messages: rest.iter().map(message_to_wire).collect(),
            tools: tools.iter().map(tool_spec_to_wire).collect(),
        };

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
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

        let parsed: MessagesResponse = serde_json::from_str(&body)?;

        if parsed.content.is_empty() {
            return Err(ProviderError::EmptyResponse);
        }

        let usage = parsed
            .usage
            .map(|wire_usage| Usage {
                input_tokens: wire_usage.input_tokens,
                output_tokens: wire_usage.output_tokens,
                cache_read_tokens: wire_usage.cache_read_input_tokens,
                cache_write_tokens: wire_usage.cache_creation_input_tokens,
            })
            .unwrap_or_default();

        Ok((
            Message {
                role: Role::Assistant,
                content: parsed
                    .content
                    .into_iter()
                    .map(wire_content_block_to_content_block)
                    .collect(),
            },
            usage,
        ))
    }
}
