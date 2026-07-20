//! OpenAI-compatible `Provider` implementation (ADR 0003).
//!
//! Owns its own wire DTOs and converts `rokr_core::Message` <-> DTO at the
//! edge; `rokr-core` never sees an OpenAI-shaped struct (ADR 0006).

use serde::{Deserialize, Serialize};

use rokr_core::{ContentBlock, Message, Role, ToolSpec, Usage};

use crate::{Provider, ProviderError};

/// OpenAI chat-completions wire format. Owned entirely by this module — see
/// ADR 0006: `rokr-core` types never carry provider-specific serde shapes.
#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMessage {
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// A single tool invocation as OpenAI represents it, both in a response
/// message's `tool_calls` array and (were rokr to replay assistant turns) in
/// an outgoing history message.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolCall {
    id: String,
    r#type: String,
    function: WireFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

/// A tool advertised to the model, OpenAI's `{type: "function", function: {..}}`
/// shape (translated from a rokr-core [`ToolSpec`]).
#[derive(Debug, Serialize)]
struct WireTool {
    r#type: String,
    function: WireFunctionDef,
}

#[derive(Debug, Serialize)]
struct WireFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
}

/// OpenAI's `usage` object. `prompt_tokens_details.cached_tokens` is how
/// OpenAI reports implicit prefix-cache hits; there's no wire concept of a
/// cache *write* count, so [`Usage::cache_write_tokens`] is always `0` for
/// this provider.
#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct WirePromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
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
        // OpenAI's `tool` role has no rokr-core `Role` equivalent (tool
        // results are a `ContentBlock`, not a role, per ADR 0006); this
        // branch only matters if a `tool`-authored wire message were ever
        // parsed back through `wire_to_message`, which the current request
        // flow doesn't do.
        _ => Role::User,
    }
}

fn tool_spec_to_wire(spec: &ToolSpec) -> WireTool {
    WireTool {
        r#type: "function".to_string(),
        function: WireFunctionDef {
            name: spec.name.clone(),
            description: spec.description.clone(),
            parameters: spec.input_schema.clone(),
        },
    }
}

/// Expands one rokr `Message` into the OpenAI wire messages it represents.
/// Usually one-to-one, but a [`ContentBlock::ToolResult`] always becomes its
/// own `role: "tool"` message (OpenAI has no concept of a mixed-role
/// message), so this returns a `Vec`.
///
/// Invariant: if a single source `Message` ever mixed `Text`/`ToolUse`
/// blocks with `ToolResult` blocks, any resulting `role: "tool"` wire
/// messages are always ordered ahead of the combined text/tool_calls wire
/// message. OpenAI rejects a `role: "tool"` message that doesn't immediately
/// follow the assistant message carrying the `tool_calls` it answers, so
/// tool-result messages must never be pushed after a same-source
/// text/tool_calls message. Nothing in rokr currently constructs such a
/// mixed message (the tool loop always keeps `ToolResult`s in their own
/// dedicated message), but this ordering keeps the function correct if that
/// ever changes.
fn message_to_wire(message: &Message) -> Vec<WireMessage> {
    let mut wire_messages = Vec::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text {
                text: block_text, ..
            } => text.push_str(block_text),
            // `cache_control` is ignored here by design: OpenAI-compatible
            // endpoints already do implicit prefix caching, so this adapter
            // deliberately never emits an explicit cache directive (see
            // docs/phase-3-context-caching.md's "Cache-optimization
            // activation" decision) — explicit emission is reserved for the
            // Anthropic provider.
            ContentBlock::ToolUse {
                id,
                name,
                input,
                cache_control: _,
            } => {
                tool_calls.push(WireToolCall {
                    id: id.clone(),
                    r#type: "function".to_string(),
                    function: WireFunctionCall {
                        name: name.clone(),
                        arguments: input.to_string(),
                    },
                });
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: _,
                cache_control: _,
            } => {
                wire_messages.push(WireMessage {
                    role: "tool".to_string(),
                    content: Some(content.clone()),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                });
            }
        }
    }

    if !text.is_empty() || !tool_calls.is_empty() {
        // Pushed after any `role: "tool"` messages collected above (rather
        // than inserted at index 0) so tool-result messages always precede
        // the combined text/tool_calls message — see the invariant
        // documented on this function.
        wire_messages.push(WireMessage {
            role: role_to_wire(message.role).to_string(),
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        });
    }

    wire_messages
}

fn wire_to_message(wire: WireMessage) -> Message {
    let mut content = Vec::new();

    if let Some(text) = wire.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text,
                cache_control: None,
            });
        }
    }

    if let Some(tool_calls) = wire.tool_calls {
        for tool_call in tool_calls {
            // If the model returns malformed JSON in `arguments`, don't
            // silently collapse it to `Value::Null` — that hides the actual
            // problem behind a generic "invalid input" failure downstream.
            // Instead, preserve the raw string as a `Value::String` so a
            // downstream `serde_json::from_value::<T>` failure names the
            // malformed content (e.g. "invalid type: string ... expected
            // struct WriteInput") rather than an opaque null.
            let input = serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| {
                serde_json::Value::String(tool_call.function.arguments.clone())
            });
            content.push(ContentBlock::ToolUse {
                id: tool_call.id,
                name: tool_call.function.name,
                input,
                cache_control: None,
            });
        }
    }

    Message {
        role: wire_to_role(&wire.role),
        content,
    }
}

/// Environment variables read by [`OpenAiProvider::from_env`].
pub const ENV_BASE_URL: &str = "ROKR_OPENAI_BASE_URL";
pub const ENV_MODEL: &str = "ROKR_OPENAI_MODEL";
pub const ENV_API_KEY: &str = "ROKR_OPENAI_API_KEY";

/// An OpenAI-compatible chat completions provider. Configured with a base
/// URL, model name, and API key so any OpenAI-compatible endpoint (OpenAI
/// itself, or a compatible proxy) can be targeted.
#[derive(Clone)]
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
        let base_url =
            std::env::var(ENV_BASE_URL).map_err(|_| ProviderError::MissingEnvVar(ENV_BASE_URL))?;
        let model =
            std::env::var(ENV_MODEL).map_err(|_| ProviderError::MissingEnvVar(ENV_MODEL))?;
        let api_key =
            std::env::var(ENV_API_KEY).map_err(|_| ProviderError::MissingEnvVar(ENV_API_KEY))?;

        Ok(Self::new(base_url, model, api_key))
    }
}

impl Provider for OpenAiProvider {
    type Error = ProviderError;

    async fn send(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<(Message, Usage), ProviderError> {
        let request_body = ChatCompletionRequest {
            model: self.model.clone(),
            messages: messages.iter().flat_map(message_to_wire).collect(),
            tools: tools.iter().map(tool_spec_to_wire).collect(),
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
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let body = response.text().await?;

        if !status.is_success() {
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimited { retry_after });
            }
            return Err(ProviderError::UnexpectedStatus {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: ChatCompletionResponse = serde_json::from_str(&body)?;
        let usage = parsed
            .usage
            .map(|wire_usage| Usage {
                input_tokens: wire_usage.prompt_tokens,
                output_tokens: wire_usage.completion_tokens,
                cache_read_tokens: wire_usage
                    .prompt_tokens_details
                    .map(|details| details.cached_tokens)
                    .unwrap_or(0),
                // OpenAI's wire shape has no cache-write concept at all.
                cache_write_tokens: 0,
            })
            .unwrap_or_default();
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or(ProviderError::EmptyResponse)?;

        Ok((wire_to_message(choice.message), usage))
    }
}
