//! Context assembly (Phase 3 Enabler 2).
//!
//! Owns the policy for how a request's context is ordered into named
//! segments — tool specs, the static system segment (agent prompt plus
//! project context), the repo map (its own segment, a placeholder until the
//! repo-map ticket lands), and the transcript — rather than each call site
//! hand-concatenating a `Vec<Message>` itself. This refactor is explicitly
//! behavior-preserving: `assemble()` must produce identical wire output to
//! today's ad hoc construction (a single system message followed by the
//! running transcript) for an equivalent input. Cache breakpoints are not
//! populated by this module yet — that's `cache-breakpoint-activation`, a
//! follow-on ticket; this module only fixes the segment order they'll
//! attach to.

use crate::{Message, ToolSpec};

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

/// Assembles a request's context in the fixed segment order: the static
/// system segment, then the repo-map segment (only present once the
/// repo-map ticket populates `repo_map`), then the transcript. Tool specs
/// pass through unchanged, returned alongside rather than folded into
/// `messages`, since `Provider::send` already takes them as a separate
/// argument.
pub fn assemble(inputs: ContextInputs) -> AssembledContext {
    let mut messages = Vec::with_capacity(inputs.transcript.len() + 2);
    messages.push(Message::system_text(inputs.system_prompt));
    if let Some(repo_map) = inputs.repo_map {
        messages.push(Message::system_text(repo_map));
    }
    messages.extend(inputs.transcript);

    AssembledContext {
        messages,
        tools: inputs.tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core behavior-preservation guarantee this refactor exists for:
    /// `assemble()` must produce byte-identical wire output to the prior
    /// hand-rolled construction (a single system message built from the
    /// prompt, followed by the running transcript verbatim — see
    /// `crates/rokr/src/main.rs`'s pre-refactor transcript seeding) for an
    /// equivalent transcript, with no repo map present.
    #[test]
    fn assemble_produces_identical_output_to_prior_hand_rolled_construction() {
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

        // Prior hand-rolled construction: exactly what `main.rs` builds by
        // hand today — one system message, then the transcript, verbatim.
        let mut hand_rolled_messages = vec![Message::system_text(system_prompt.clone())];
        hand_rolled_messages.extend(transcript.clone());

        let assembled = assemble(ContextInputs {
            system_prompt,
            tools: tools.clone(),
            repo_map: None,
            transcript,
        });

        let hand_rolled_json =
            serde_json::to_vec(&hand_rolled_messages).expect("serialize hand-rolled messages");
        let assembled_json =
            serde_json::to_vec(&assembled.messages).expect("serialize assembled messages");

        assert_eq!(
            assembled_json, hand_rolled_json,
            "assemble() must produce byte-identical wire output to the prior hand-rolled construction"
        );
        assert_eq!(assembled.tools, tools);
    }
}
