//! Integration tests for the OpenAI-compatible provider (ADR 0003 / 0006).
//!
//! These drive `OpenAiProvider` against a `wiremock` HTTP server standing in
//! for an OpenAI-compatible endpoint, verifying request shape and asserting
//! the parsed assistant `Message` (or typed error) that comes back.

use rokr_core::{Message, Role};
use rokr_provider::{OpenAiProvider, Provider};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_provider_returns_assistant_message_from_mock_server() {
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
                        "content": "Hello from the mock OpenAI-compatible server!"
                    },
                    "finish_reason": "stop"
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let provider = OpenAiProvider::new(mock_server.uri(), "gpt-4o-mini", "test-api-key");

    let result = provider
        .send(&[Message::user_text("Say hello")])
        .await
        .expect("provider call should succeed against a healthy mock server");

    assert_eq!(result.role, Role::Assistant);
    assert_eq!(result.text(), "Hello from the mock OpenAI-compatible server!");
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

    let result = provider.send(&[Message::user_text("Say hello")]).await;

    assert!(
        result.is_err(),
        "a 500 from the provider must surface as Err, not panic or Ok"
    );
}
