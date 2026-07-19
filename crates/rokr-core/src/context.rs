//! Context assembly (Phase 3 Enabler 2).
//!
//! Owns the policy for how a request's context is ordered into named
//! segments — tool specs, the static system segment (agent prompt plus
//! project context), the repo map (its own segment, a placeholder until the
//! repo-map ticket lands), and the transcript — rather than each call site
//! hand-concatenating a `Vec<Message>` itself. Originally (ticket 15) this
//! was purely a segment-ordering refactor, behavior-preserving modulo cache
//! hints. `cache-breakpoint-activation` (this ticket) is what actually
//! populates the breakpoints: the static segments (tools, system prompt)
//! get a long-TTL `Extended` breakpoint, and the rolling transcript tail
//! gets a short-TTL `Ephemeral` one that moves every turn. So `assemble()`'s
//! wire output now differs from the old hand-rolled construction exactly by
//! those `cache_control` hints — everything else (segment order, message
//! content, ordering) stays identical.

use crate::{CacheControl, CacheControlKind, ContentBlock, Message, ToolSpec};

/// Inputs to [`assemble`]: the static system-prompt text (agent prompt plus
/// project context, already concatenated by the caller), the tool specs to
/// advertise, an optional repo-map segment (`None` until the repo-map
/// ticket lands), and the running conversation transcript.
pub struct ContextInputs {
    pub system_prompt: String,
    pub tools: Vec<ToolSpec>,
    pub repo_map: Option<String>,
    pub transcript: Vec<Message>,
}

/// The assembled request, ready to hand to [`crate::Provider::send`]: the
/// ordered messages (static system segment, repo map if present, then the
/// transcript) and the tool specs, unchanged.
pub struct AssembledContext {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

/// Sets `cache_control` on a content block regardless of which variant it
/// is. All three `ContentBlock` variants already carry the field (ticket
/// 15), so this is a straightforward match-and-rebuild rather than a new
/// abstraction.
fn with_cache_control(block: ContentBlock, cache_control: CacheControl) -> ContentBlock {
    match block {
        ContentBlock::Text { text, .. } => ContentBlock::Text {
            text,
            cache_control: Some(cache_control),
        },
        ContentBlock::ToolUse {
            id, name, input, ..
        } => ContentBlock::ToolUse {
            id,
            name,
            input,
            cache_control: Some(cache_control),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control: Some(cache_control),
        },
    }
}

/// Assembles a request's context in the fixed segment order: the static
/// system segment, then the repo-map segment (only present once the
/// repo-map ticket populates `repo_map`), then the transcript. Tool specs
/// pass through unchanged, returned alongside rather than folded into
/// `messages`, since `Provider::send` already takes them as a separate
/// argument. Populates cache breakpoints (`cache-breakpoint-activation`):
/// static segments (tools, system prompt) get a long-TTL `Extended`
/// breakpoint; the rolling transcript tail gets a short-TTL `Ephemeral` one.
pub fn assemble(inputs: ContextInputs) -> AssembledContext {
    // Static segments (tools, system prompt) get a long-TTL `Extended`
    // breakpoint; the rolling transcript tail gets a short-TTL `Ephemeral`
    // one, since it moves every turn (cache-breakpoint-activation).
    let mut tools = inputs.tools;
    if let Some(last_tool) = tools.last_mut() {
        last_tool.cache_control = Some(CacheControl {
            kind: CacheControlKind::Extended,
        });
    }

    let mut messages = Vec::with_capacity(inputs.transcript.len() + 2);

    let mut system_message = Message::system_text(inputs.system_prompt);
    if let Some(last_block) = system_message.content.pop() {
        system_message.content.push(with_cache_control(
            last_block,
            CacheControl {
                kind: CacheControlKind::Extended,
            },
        ));
    }
    messages.push(system_message);

    if let Some(repo_map) = inputs.repo_map {
        let mut repo_map_message = Message::system_text(repo_map);
        if let Some(last_block) = repo_map_message.content.pop() {
            repo_map_message.content.push(with_cache_control(
                last_block,
                CacheControl {
                    kind: CacheControlKind::Extended,
                },
            ));
        }
        messages.push(repo_map_message);
    }

    let transcript_is_empty = inputs.transcript.is_empty();
    messages.extend(inputs.transcript);

    if !transcript_is_empty {
        if let Some(last_message) = messages.last_mut() {
            if let Some(last_block) = last_message.content.pop() {
                last_message.content.push(with_cache_control(
                    last_block,
                    CacheControl {
                        kind: CacheControlKind::Ephemeral,
                    },
                ));
            }
        }
    }

    AssembledContext { messages, tools }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CacheControl, CacheControlKind, ContentBlock};

    /// The core behavior-preservation guarantee this refactor exists for:
    /// `assemble()` must produce byte-identical wire output to the prior
    /// hand-rolled construction (a single system message built from the
    /// prompt, followed by the running transcript verbatim — see
    /// `crates/rokr/src/main.rs`'s pre-refactor transcript seeding) for an
    /// equivalent transcript, with no repo map present.
    #[test]
    fn assemble_produces_identical_output_to_prior_hand_rolled_construction_modulo_cache_control() {
        let system_prompt = "You are a helpful build agent.".to_string();
        let tools = vec![ToolSpec {
            name: "read".to_string(),
            description: "reads a file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        }];
        let transcript = vec![
            Message::user_text("hello"),
            Message::assistant_text("hi there"),
        ];

        // Prior hand-rolled construction: exactly what `main.rs` built by
        // hand pre-refactor — one system message, then the transcript,
        // verbatim, with no cache hints at all.
        let mut hand_rolled_messages = vec![Message::system_text(system_prompt.clone())];
        hand_rolled_messages.extend(transcript.clone());

        let assembled = assemble(ContextInputs {
            system_prompt,
            tools: tools.clone(),
            repo_map: None,
            transcript,
        });

        // `assemble()` now populates cache breakpoints
        // (cache-breakpoint-activation), which the hand-rolled construction
        // never had. Compare modulo `cache_control` to keep asserting what
        // this test still owns: segment order and message content are
        // otherwise unchanged. The breakpoint placement itself is asserted
        // by `assemble_places_breakpoints_after_tools_and_static_system_segment`.
        let hand_rolled_json = serde_json::to_vec(&strip_cache_control(hand_rolled_messages))
            .expect("serialize hand-rolled messages");
        let assembled_json = serde_json::to_vec(&strip_cache_control(assembled.messages))
            .expect("serialize assembled messages");

        assert_eq!(
            assembled_json, hand_rolled_json,
            "assemble() must produce identical wire output to the prior hand-rolled construction, \
             modulo cache_control breakpoints"
        );
        assert_eq!(assembled.tools.len(), tools.len());
        assert_eq!(assembled.tools[0].name, tools[0].name);
        assert_eq!(assembled.tools[0].description, tools[0].description);
        assert_eq!(assembled.tools[0].input_schema, tools[0].input_schema);
    }

    /// Clears `cache_control` on every content block of every message, so a
    /// test can compare wire output structurally (order, roles, text/tool
    /// content) while ignoring cache-breakpoint hints.
    fn strip_cache_control(messages: Vec<Message>) -> Vec<Message> {
        messages
            .into_iter()
            .map(|message| Message {
                role: message.role,
                content: message
                    .content
                    .into_iter()
                    .map(|block| match block {
                        ContentBlock::Text { text, .. } => ContentBlock::Text {
                            text,
                            cache_control: None,
                        },
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            cache_control: None,
                        },
                        ContentBlock::ToolResult {
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
                    })
                    .collect(),
            })
            .collect()
    }

    /// Cache-breakpoint placement policy (`cache-breakpoint-activation`):
    /// static segments (tools, system prompt) get a long-TTL `Extended`
    /// breakpoint; the rolling transcript tail gets a short-TTL `Ephemeral`
    /// one that moves every turn. Uses >= 2 tools and >= 2 transcript
    /// messages so "last-only" placement is actually exercised rather than
    /// accidentally true for a length-1 list.
    #[test]
    fn assemble_places_breakpoints_after_tools_and_static_system_segment() {
        let system_prompt = "You are a helpful build agent.".to_string();
        let tools = vec![
            ToolSpec {
                name: "read".to_string(),
                description: "reads a file".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: None,
            },
            ToolSpec {
                name: "write".to_string(),
                description: "writes a file".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: None,
            },
        ];
        let transcript = vec![
            Message::user_text("hello"),
            Message::assistant_text("hi there"),
        ];

        let assembled = assemble(ContextInputs {
            system_prompt,
            tools,
            repo_map: None,
            transcript,
        });

        // Every tool but the last stays untouched; the last gets Extended.
        assert_eq!(
            assembled.tools[0].cache_control, None,
            "only the last tool spec should get a breakpoint"
        );
        assert_eq!(
            assembled.tools[1].cache_control,
            Some(CacheControl {
                kind: CacheControlKind::Extended
            }),
            "the last tool spec should get an Extended breakpoint"
        );

        // The system message (messages[0]) is a single Text block that
        // should get an Extended breakpoint.
        match &assembled.messages[0].content[..] {
            [ContentBlock::Text { cache_control, .. }] => {
                assert_eq!(
                    cache_control,
                    &Some(CacheControl {
                        kind: CacheControlKind::Extended
                    }),
                    "the system segment should get an Extended breakpoint"
                );
            }
            other => panic!("expected a single Text block for the system message, got {other:?}"),
        }

        // The first transcript message (messages[1], "hello") must be left
        // untouched — only the tail of the whole assembled list gets marked.
        match &assembled.messages[1].content[..] {
            [ContentBlock::Text { cache_control, .. }] => {
                assert_eq!(
                    cache_control, &None,
                    "non-tail transcript messages must not get a breakpoint"
                );
            }
            other => panic!("expected a single Text block, got {other:?}"),
        }

        // The last message in the whole assembled list (the actual
        // conversation tail) gets an Ephemeral breakpoint on its last block.
        let last_message = assembled
            .messages
            .last()
            .expect("assembled messages non-empty");
        match last_message.content.last() {
            Some(ContentBlock::Text { cache_control, .. }) => {
                assert_eq!(
                    cache_control,
                    &Some(CacheControl {
                        kind: CacheControlKind::Ephemeral
                    }),
                    "the transcript tail should get an Ephemeral breakpoint"
                );
            }
            other => panic!("expected a Text block, got {other:?}"),
        }
    }
}
