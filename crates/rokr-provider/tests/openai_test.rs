//! Integration tests for the OpenAI-compatible provider (ADR 0003 / 0006).
//!
//! These drive `OpenAiProvider` against a `wiremock` HTTP server standing in
//! for an OpenAI-compatible endpoint, verifying request shape and asserting
//! the parsed assistant `Message` (or typed error) that comes back.

use rokr_core::{ContentBlock, Message, Role, ToolSpec};
use rokr_provider::{OpenAiProvider, Provider};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_provider_returns_assistant_message_from_mock_server() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-api-key"))
        .and(body_partial_json(json!({
            "model": "gpt-4o-mini",
            "messages": [
                {
                    "role": "user",
                    "content": "Say hello"
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello from the mock OpenAI-compatible server!"
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let result = provider
        .send(&[Message::user_text("Say hello")], &[])
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.role, Role::Assistant);
    assert_eq!(
        result.text(),
        "Hello from the mock OpenAI-compatible server!"
    );
}

#[tokio::test]
async fn openai_provider_surfaces_http_error_as_provider_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let result = provider.send(&[Message::user_text("Say hello")], &[]).await;

    assert!(
        result.is_err(),
        "a 500 from the provider must surface as Err, not panic or Ok"
    );
}

#[tokio::test]
async fn openai_provider_surfaces_invalid_json_as_deserialize_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let result = provider.send(&[Message::user_text("Say hello")], &[]).await;

    assert!(
        matches!(result, Err(rokr_provider::ProviderError::Deserialize(_))),
        "a 200 with a non-JSON body must surface as ProviderError::Deserialize"
    );
}

#[tokio::test]
async fn openai_provider_surfaces_empty_choices_as_empty_response_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "choices": [] })))
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let result = provider.send(&[Message::user_text("Say hello")], &[]).await;

    assert!(
        matches!(result, Err(rokr_provider::ProviderError::EmptyResponse)),
        "a 200 with an empty choices array must surface as ProviderError::EmptyResponse"
    );
}

#[tokio::test]
async fn openai_provider_request_includes_tools_array_when_specs_provided() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({
            "model": "gpt-4o-mini",
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get the current weather for a location",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "location": { "type": "string" }
                            },
                            "required": ["location"]
                        }
                    }
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "It's sunny."
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let tool_specs = [ToolSpec {
        name: "get_weather".to_string(),
        description: "Get the current weather for a location".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string" }
            },
            "required": ["location"]
        }),
    }];

    let result = provider
        .send(
            &[Message::user_text("What's the weather in Sydney?")],
            &tool_specs,
        )
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.role, Role::Assistant);
    assert_eq!(result.text(), "It's sunny.");
}

#[tokio::test]
async fn openai_provider_parses_tool_calls_response_into_tool_use_blocks() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_abc123",
                                "type": "function",
                                "function": {
                                    "name": "get_weather",
                                    "arguments": "{\"location\":\"Sydney\"}"
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let tool_specs = [ToolSpec {
        name: "get_weather".to_string(),
        description: "Get the current weather for a location".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string" }
            },
            "required": ["location"]
        }),
    }];

    let result = provider
        .send(
            &[Message::user_text("What's the weather in Sydney?")],
            &tool_specs,
        )
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.role, Role::Assistant);
    assert_eq!(result.content.len(), 1);
    match &result.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "get_weather");
            assert_eq!(input, &json!({ "location": "Sydney" }));
        }
        other => panic!("expected ToolUse block, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_provider_sends_tool_result_as_role_tool_wire_message() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({
            "messages": [
                {
                    "role": "user",
                    "content": "What's the weather in Sydney?"
                },
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"Sydney\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc123",
                    "content": "It's sunny in Sydney"
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Glad it's sunny!"
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let messages = vec![
        Message::user_text("What's the weather in Sydney?"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_abc123".to_string(),
                name: "get_weather".to_string(),
                input: json!({ "location": "Sydney" }),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_abc123".to_string(),
                content: "It's sunny in Sydney".to_string(),
                is_error: false,
            }],
        },
    ];

    let result = provider
        .send(&messages, &[])
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.role, Role::Assistant);
    assert_eq!(result.text(), "Glad it's sunny!");
}

#[tokio::test]
async fn openai_provider_preserves_malformed_tool_call_arguments_as_string() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_bad_json",
                                "type": "function",
                                "function": {
                                    "name": "get_weather",
                                    "arguments": "{not json"
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let tool_specs = [ToolSpec {
        name: "get_weather".to_string(),
        description: "Get the current weather for a location".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string" }
            },
            "required": ["location"]
        }),
    }];

    let result = provider
        .send(
            &[Message::user_text("What's the weather in Sydney?")],
            &tool_specs,
        )
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.content.len(), 1);
    match &result.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_bad_json");
            assert_eq!(name, "get_weather");
            assert_eq!(
                input,
                &json!("{not json"),
                "malformed arguments must be preserved as a Value::String, not silently nulled"
            );
        }
        other => panic!("expected ToolUse block, got {other:?}"),
    }
}

#[tokio::test]
async fn message_to_wire_orders_tool_results_before_text() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({
            "messages": [
                {
                    "role": "tool",
                    "tool_call_id": "call_xyz",
                    "content": "tool output"
                },
                {
                    "role": "user",
                    "content": "here is the result"
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "acknowledged"
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    // A single source `Message` mixing a `Text` block with a `ToolResult`
    // block. Nothing in rokr constructs this today (the tool loop keeps
    // `ToolResult`s in their own dedicated message), but `message_to_wire`
    // must still order the expanded `role: "tool"` message ahead of the
    // combined text message so a request built from such a message would
    // never be rejected by OpenAI's API.
    let mixed_message = Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: "here is the result".to_string(),
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_xyz".to_string(),
                content: "tool output".to_string(),
                is_error: false,
            },
        ],
    };

    let result = provider
        .send(&[mixed_message], &[])
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.text(), "acknowledged");
}

/// Guards `from_env` tests that mutate process-wide environment variables.
/// Nothing else in this binary currently touches `ROKR_OPENAI_*`, but this
/// mutex keeps that an enforced invariant rather than an assumed one, so a
/// future test added to this file can't flakily race this one.
static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn openai_provider_from_env_surfaces_missing_env_var() {
    let _lock = ENV_GUARD.lock().unwrap();

    std::env::remove_var(rokr_provider::openai::ENV_BASE_URL);
    std::env::remove_var(rokr_provider::openai::ENV_MODEL);
    std::env::remove_var(rokr_provider::openai::ENV_API_KEY);

    let result = rokr_provider::OpenAiProvider::from_env();

    assert!(
        matches!(result, Err(rokr_provider::ProviderError::MissingEnvVar(_))),
        "from_env() with unset env vars must surface as ProviderError::MissingEnvVar"
    );
}
