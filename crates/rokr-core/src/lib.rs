//! The agent loop, message and content-block model, context compaction.

pub mod message;

pub use message::{CacheControl, CacheControlKind, ContentBlock, Message, Role};

/// A tool a provider may call, described in rokr-core-native terms. Not
/// `rokr-tools::Tool` — that dependency edge doesn't exist yet (that crate
/// depends on `rokr-core`, not the reverse), so `ToolSpec` stays the minimal
/// shape a `Provider` needs to advertise tools on the wire: a name, a
/// human-readable description, and a JSON Schema for its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A backend capable of turning a conversation (ordered `Message`s) into the
/// next assistant `Message`. Defined here rather than in `rokr-provider` so
/// that `rokr-core`'s own orchestration (e.g. [`single_turn`]) can be generic
/// over it without `rokr-core` depending on `rokr-provider` — which already
/// depends on `rokr-core` per ADR 0003 as refined by 0009, so the reverse
/// edge would be a cycle. `rokr-provider` re-exports this trait so existing
/// call sites are unaffected; concrete implementations still live there,
/// one module per provider (ADR 0003 as refined by 0009).
///
/// The associated `Error` type keeps this trait free of any
/// provider-specific error shape (e.g. reqwest/serde_json failure variants),
/// so `rokr-core`'s dependency graph stays minimal and provider-agnostic
/// (ADR 0006).
pub trait Provider {
    type Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static;

    async fn send(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Message, Self::Error>;
}

/// Sends a single user turn to `provider` and returns the assistant's reply.
/// Phase 1's minimal orchestration: wrap `input` as a user [`Message`], call
/// the provider with just that one message and no tools, and hand back
/// whatever assistant `Message` comes back.
pub async fn single_turn<P: Provider>(
    provider: &P,
    input: impl Into<String>,
) -> Result<Message, P::Error> {
    let user_message = Message::user_text(input);
    provider.send(&[user_message], &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StubError;

    impl std::fmt::Display for StubError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stub error")
        }
    }

    struct StubProvider;

    impl Provider for StubProvider {
        type Error = StubError;

        async fn send(
            &self,
            messages: &[Message],
            tools: &[ToolSpec],
        ) -> Result<Message, StubError> {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role, Role::User);
            assert_eq!(messages[0].text(), "hello");
            assert!(tools.is_empty());
            Ok(Message::assistant_text("hi there"))
        }
    }

    #[tokio::test]
    async fn single_turn_returns_assistant_message() {
        let provider = StubProvider;

        let response = single_turn(&provider, "hello")
            .await
            .expect("stub provider call should succeed");

        assert_eq!(response.role, Role::Assistant);
        assert_eq!(response.text(), "hi there");
    }
}
