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

        // Fraction of total prompt tokens (input + cache-read + cache-write)
        // served from cache on this call. Observability only (section 1) —
        // no analytics pipeline consumes this yet (that's Phase 7).
        let cache_hit_ratio = {
            let total = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
            if total > 0 {
                usage.cache_read_tokens as f64 / total as f64
            } else {
                0.0
            }
        };
        tracing::info!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read_tokens = usage.cache_read_tokens,
            cache_write_tokens = usage.cache_write_tokens,
            cache_hit_ratio,
            "anthropic provider send completed"
        );

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Field values pulled off a single captured `tracing` event.
    #[derive(Debug, Default, Clone)]
    struct CapturedEvent {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        cache_hit_ratio: Option<f64>,
    }

    #[derive(Default)]
    struct EventVisitor(CapturedEvent);

    impl tracing::field::Visit for EventVisitor {
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            match field.name() {
                "input_tokens" => self.0.input_tokens = Some(value),
                "output_tokens" => self.0.output_tokens = Some(value),
                "cache_read_tokens" => self.0.cache_read_tokens = Some(value),
                "cache_write_tokens" => self.0.cache_write_tokens = Some(value),
                _ => {}
            }
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            // `u64` fields recorded via `tracing::info!` may surface through
            // `record_i64` depending on the value's field-type encoding;
            // mirror the same field routing here.
            match field.name() {
                "input_tokens" => self.0.input_tokens = Some(value as u64),
                "output_tokens" => self.0.output_tokens = Some(value as u64),
                "cache_read_tokens" => self.0.cache_read_tokens = Some(value as u64),
                "cache_write_tokens" => self.0.cache_write_tokens = Some(value as u64),
                _ => {}
            }
        }

        fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
            if field.name() == "cache_hit_ratio" {
                self.0.cache_hit_ratio = Some(value);
            }
        }

        fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
    }

    /// A minimal `tracing_subscriber::Layer` that captures every event's
    /// field values into a shared `Vec` for assertion, avoiding
    /// string-matching against formatted log lines.
    #[derive(Clone, Default)]
    struct CapturingLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for CapturingLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = EventVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }
    }

    #[tokio::test]
    async fn send_emits_tracing_event_with_usage_and_cache_hit_ratio() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": "hi there" }],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 30,
                    "cache_creation_input_tokens": 20
                }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider =
            AnthropicProvider::new(mock_server.uri(), "claude-3-5-sonnet-20241022", "test-api-key");

        let events: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = CapturingLayer {
            events: events.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let messages = vec![Message::user_text("hello")];

        // `set_default` (rather than `with_default`) so the guard can be
        // held across the `.await` below; safe here because `#[tokio::test]`
        // defaults to a single-threaded (current-thread) runtime, so the
        // future is never polled from a different OS thread than the one
        // that installed the thread-local subscriber.
        let guard = tracing::subscriber::set_default(subscriber);
        let result = provider.send(&messages, &[]).await;
        drop(guard);

        result.expect("mocked send() should succeed");

        let captured = events.lock().unwrap();
        let event = captured
            .iter()
            .find(|event| event.input_tokens.is_some())
            .expect("send() should emit a tracing event carrying usage fields");

        let expected_input = 100u64;
        let expected_output = 50u64;
        let expected_cache_read = 30u64;
        let expected_cache_write = 20u64;
        let expected_total = expected_input + expected_cache_read + expected_cache_write;
        let expected_ratio = expected_cache_read as f64 / expected_total as f64;

        assert_eq!(event.input_tokens, Some(expected_input));
        assert_eq!(event.output_tokens, Some(expected_output));
        assert_eq!(event.cache_read_tokens, Some(expected_cache_read));
        assert_eq!(event.cache_write_tokens, Some(expected_cache_write));
        assert_eq!(event.cache_hit_ratio, Some(expected_ratio));
    }
}
