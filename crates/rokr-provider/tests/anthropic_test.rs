//! Wire-shape tests for the Anthropic `Provider` implementation (Phase 4
//! tracer bullet). Mirrors `tests/openai_test.rs`'s wiremock pattern:
//! assert on the actual wire request/response shape rather than internal
//! call sequencing.

use rokr_core::{CacheControl, CacheControlKind, ContentBlock, Message, Role, ToolSpec};
use rokr_provider::{AnthropicProvider, Provider};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mirrors `context::assemble`'s output shape: leading system-role
/// messages (agent prompt, then repo map) each carrying an `Extended`
/// breakpoint, followed by ordinary transcript messages.
#[tokio::test]
async fn anthropic_provider_hoists_system_messages_into_top_level_system_array() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "system": [
                {
                    "type": "text",
                    "text": "You are a helpful build agent.",
                    "cache_control": { "type": "ephemeral", "ttl": "1h" }
                },
                {
                    "type": "text",
                    "text": "repo map contents"
                }
            ],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "hello" }
                    ]
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Hello from the mock Anthropic server!" }],
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = AnthropicProvider::new(
        mock_server.uri(),
        "claude-3-5-sonnet-20241022",
        "test-api-key",
    );

    let system_prompt_message = Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "You are a helpful build agent.".to_string(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Extended,
            }),
        }],
    };
    let repo_map_message = Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "repo map contents".to_string(),
            cache_control: None,
        }],
    };
    let user_message = Message::user_text("hello");

    let messages = vec![system_prompt_message, repo_map_message, user_message];

    let (result, _usage) = provider
        .send(&messages, &[])
        .await
        .expect(
            "provider call should succeed once leading system messages are hoisted into the \
             top-level `system` array",
        );

    assert_eq!(result.text(), "Hello from the mock Anthropic server!");
}

/// Exercises all three static/rolling breakpoint locations at once (last
/// tool spec, both system-segment entries, and the transcript tail), and
/// confirms the wire translation never emits more than the four-breakpoint
/// ceiling `context::assemble` already respects on the core side.
#[tokio::test]
async fn anthropic_provider_places_cache_control_on_last_tool_spec_system_array_and_transcript_tail(
) {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "system": [
                {
                    "type": "text",
                    "text": "system prompt text",
                    "cache_control": { "type": "ephemeral", "ttl": "1h" }
                },
                {
                    "type": "text",
                    "text": "repo map text",
                    "cache_control": { "type": "ephemeral", "ttl": "1h" }
                }
            ],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "earlier turn" }
                    ]
                },
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "text",
                            "text": "final answer",
                            "cache_control": { "type": "ephemeral" }
                        }
                    ]
                }
            ],
            "tools": [
                {
                    "name": "read",
                    "description": "reads a file",
                    "input_schema": { "type": "object" }
                },
                {
                    "name": "write",
                    "description": "writes a file",
                    "input_schema": { "type": "object" },
                    "cache_control": { "type": "ephemeral", "ttl": "1h" }
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "acknowledged" }],
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = AnthropicProvider::new(
        mock_server.uri(),
        "claude-3-5-sonnet-20241022",
        "test-api-key",
    );

    let system_prompt_message = Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "system prompt text".to_string(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Extended,
            }),
        }],
    };
    let repo_map_message = Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: "repo map text".to_string(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Extended,
            }),
        }],
    };
    let first_transcript_message = Message::user_text("earlier turn");
    let tail_message = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "final answer".to_string(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Ephemeral,
            }),
        }],
    };

    let messages = vec![
        system_prompt_message,
        repo_map_message,
        first_transcript_message,
        tail_message,
    ];

    let tools = vec![
        ToolSpec {
            name: "read".to_string(),
            description: "reads a file".to_string(),
            input_schema: json!({"type": "object"}),
            cache_control: None,
        },
        ToolSpec {
            name: "write".to_string(),
            description: "writes a file".to_string(),
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Extended,
            }),
        },
    ];

    let (result, _usage) = provider
        .send(&messages, &tools)
        .await
        .expect("provider call should succeed with all three breakpoint locations correctly placed");

    assert_eq!(result.text(), "acknowledged");

    let received = mock_server
        .received_requests()
        .await
        .expect("wiremock should have recorded the request");
    assert_eq!(received.len(), 1);
    let body_str =
        String::from_utf8(received[0].body.clone()).expect("request body should be valid utf-8");
    let cache_control_count = body_str.matches("\"cache_control\"").count();
    assert_eq!(
        cache_control_count, 4,
        "must emit exactly the four breakpoints (system prompt, repo map, last tool spec, \
         transcript tail) and never exceed the four-breakpoint ceiling; wire body was: {body_str}"
    );
}
