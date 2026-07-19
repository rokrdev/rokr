//! Provider-agnostic message model (ADR 0006).
//!
//! Every message — system, user, or assistant — is a role plus an ordered
//! list of content blocks. `ContentBlock` is the designated extension point
//! for new modalities and turn types (tool calls, images, thinking); it is
//! intentionally *not* `#[non_exhaustive]` so that adding a variant forces a
//! compile error in every provider adapter's `match`, guaranteeing no
//! provider silently drops a block type it doesn't handle.
//!
//! `rokr-core` types derive `serde::{Serialize, Deserialize}` only for
//! rokr's own persistence (sessions, checkpoints) in a rokr-native shape.
//! Providers own their own wire DTOs and convert `Message` <-> DTO at the
//! edge (ADR 0003) — no provider-specific serde attributes belong here.

use serde::{Deserialize, Serialize};

/// Who authored a message. Kept minimal and provider-neutral: providers map
/// these onto their own conventions at the edge (e.g. Anthropic lifts
/// `System` to the top-level `system` param; OpenAI tool results map onto
/// the `tool` role, not a rokr role).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Caching hint kind, distinguishing a breakpoint's TTL. `Ephemeral` is the
/// short-lived default; `Extended` is the long-lived ("hour-plus") variant
/// (Phase 3). Modeling TTL as a kind rather than a raw wire duration keeps
/// this provider-agnostic — each provider adapter maps a kind onto whatever
/// duration (or no-op) makes sense for its own wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheControlKind {
    Ephemeral,
    Extended,
}

/// Caching hint attached to a content block. Always `None` until Phase 3;
/// present now so caching is additive rather than a struct change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    pub kind: CacheControlKind,
}

/// A single unit of content within a message. This enum is the designated
/// extension point for the message model: new modalities and turn types
/// (tool calls, images, thinking) are added as variants, never by changing
/// `Message` or `Role`.
///
/// Deliberately NOT `#[non_exhaustive]`: adding a variant must break every
/// provider adapter's `match` at compile time.
// Phase 8:  Image { source, cache_control }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text {
        text: String,
        cache_control: Option<CacheControl>,
    },
    /// A model-issued request to invoke a tool. `id` correlates this block
    /// with the [`ContentBlock::ToolResult`] that answers it; `input` is the
    /// tool's arguments as the provider's raw JSON, un-typed here because
    /// `rokr-core` has no dependency on tool schemas (ADR 0006).
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        cache_control: Option<CacheControl>,
    },
    /// The outcome of executing a [`ContentBlock::ToolUse`], keyed back to it
    /// by `tool_use_id`. `content` is the tool's output rendered as text;
    /// `is_error` distinguishes a tool failure from a successful result so
    /// providers can convey that distinction on the wire.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        cache_control: Option<CacheControl>,
    },
}

/// A message is always role + an ordered list of blocks, uniformly. The
/// Anthropic-style "content may be a bare string" shortcut is a wire-format
/// concern owned by the provider adapter, not the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    fn text_message(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
        }
    }

    pub fn system_text(text: impl Into<String>) -> Self {
        Self::text_message(Role::System, text)
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Self::text_message(Role::User, text)
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::text_message(Role::Assistant, text)
    }

    /// Concatenates all `Text` blocks in order. Convenience accessor for the
    /// render layer; other block kinds (tool use/result, and future
    /// modalities) contribute nothing here.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrips_role_and_content() {
        let original = Message::user_text("hello world");

        let json = serde_json::to_string(&original).expect("serialize message");
        let restored: Message = serde_json::from_str(&json).expect("deserialize message");

        assert_eq!(restored.role, Role::User);
        assert_eq!(restored.content.len(), 1);
        match &restored.content[0] {
            ContentBlock::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "hello world");
                assert!(cache_control.is_none());
            }
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_and_tool_result_round_trip_serialization() {
        let original = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({ "path": "src/lib.rs" }),
                    cache_control: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "file contents".to_string(),
                    is_error: false,
                    cache_control: None,
                },
            ],
        };

        let json = serde_json::to_string(&original).expect("serialize message");
        let restored: Message = serde_json::from_str(&json).expect("deserialize message");

        assert_eq!(restored.role, Role::Assistant);
        assert_eq!(restored.content.len(), 2);

        match &restored.content[0] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(input, &serde_json::json!({ "path": "src/lib.rs" }));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }

        match &restored.content[1] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult block, got {other:?}"),
        }
    }

    /// Phase 3 enabler: `CacheControlKind` gains a TTL-distinguishing
    /// variant (short-lived `Ephemeral` vs. long-lived `Extended`), and
    /// `ToolUse`/`ToolResult` blocks gain an optional `cache_control` that
    /// must round-trip through serde like the existing `Text` block one
    /// already does.
    #[test]
    fn cache_control_ttl_kind_and_tool_block_cache_control_round_trip() {
        let short_lived = CacheControl {
            kind: CacheControlKind::Ephemeral,
        };
        let long_lived = CacheControl {
            kind: CacheControlKind::Extended,
        };
        assert_ne!(
            short_lived, long_lived,
            "the two TTL kinds must be distinguishable"
        );

        let original = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({ "path": "src/lib.rs" }),
                    cache_control: Some(long_lived.clone()),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "file contents".to_string(),
                    is_error: false,
                    cache_control: Some(short_lived.clone()),
                },
            ],
        };

        let json = serde_json::to_string(&original).expect("serialize message");
        let restored: Message = serde_json::from_str(&json).expect("deserialize message");

        match &restored.content[0] {
            ContentBlock::ToolUse { cache_control, .. } => {
                assert_eq!(cache_control, &Some(long_lived));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }

        match &restored.content[1] {
            ContentBlock::ToolResult { cache_control, .. } => {
                assert_eq!(cache_control, &Some(short_lived));
            }
            other => panic!("expected ToolResult block, got {other:?}"),
        }
    }
}
