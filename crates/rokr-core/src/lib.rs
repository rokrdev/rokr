//! The agent loop, message and content-block model, context compaction.

use std::future::Future;
use std::pin::Pin;

pub mod message;

pub use message::{CacheControl, CacheControlKind, ContentBlock, Message, Role};

/// A tool a provider may call, described in rokr-core-native terms. The
/// minimal shape a `Provider` needs to advertise tools on the wire: a name,
/// a human-readable description, and a JSON Schema for its input. Built from
/// a `rokr_tools::Tool` by [`ExecutableTool::to_tool_spec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Object-safe veneer over `rokr_tools::Tool`, needed because `Tool::execute`
/// is a native `async fn` and therefore not itself dyn-compatible. The tool
/// loop needs to hold a heterogeneous, runtime-selectable set of tools (the
/// model picks a tool by name at each step), so this trait boxes the
/// execution future by hand. Implemented via [`impl_executable_tool`] for
/// each concrete `Tool` the loop needs to run — deliberately *not* a
/// blanket `impl<T: Tool> ExecutableTool for T`, because `Tool::execute`'s
/// future has no `Send` bound in its trait definition, and the compiler can
/// only prove a `-> impl Future + Send` return is actually `Send` when it
/// sees the concrete, monomorphized implementation, not a generic one.
pub trait ExecutableTool: Send + Sync {
    /// The tool's stable, model-facing name (delegates to `Tool::name`).
    fn name(&self) -> &'static str;

    /// This tool's wire-facing description, for advertising it to the
    /// provider (delegates to `Tool::description`/`Tool::input_schema`).
    fn to_tool_spec(&self) -> ToolSpec;

    /// Runs the tool, boxing the resulting future so it can live behind
    /// `dyn ExecutableTool`. `Send` so the loop's own future stays `Send`,
    /// which `rokr-tui::run` requires of the whole submit future.
    fn execute_boxed<'a>(
        &'a self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, rokr_tools::ToolError>> + Send + 'a>>;
}

/// Implements [`ExecutableTool`] for a concrete `rokr_tools::Tool` type by
/// delegating to its inherent `Tool` methods. See [`ExecutableTool`]'s docs
/// for why this is a macro over concrete types rather than a blanket impl.
macro_rules! impl_executable_tool {
    ($ty:ty) => {
        impl ExecutableTool for $ty {
            fn name(&self) -> &'static str {
                rokr_tools::Tool::name(self)
            }

            fn to_tool_spec(&self) -> ToolSpec {
                ToolSpec {
                    name: rokr_tools::Tool::name(self).to_string(),
                    description: rokr_tools::Tool::description(self).to_string(),
                    input_schema: rokr_tools::Tool::input_schema(self),
                }
            }

            fn execute_boxed<'a>(
                &'a self,
                input: serde_json::Value,
            ) -> Pin<Box<dyn Future<Output = Result<String, rokr_tools::ToolError>> + Send + 'a>>
            {
                Box::pin(async move { <$ty as rokr_tools::Tool>::execute(self, input).await })
            }
        }
    };
}

impl_executable_tool!(rokr_tools::read::ReadTool);
impl_executable_tool!(rokr_tools::glob::GlobTool);
impl_executable_tool!(rokr_tools::grep::GrepTool);
impl_executable_tool!(rokr_tools::ls::LsTool);

/// Runs the agent tool loop (ADR 0004): sends `input` as the first user turn
/// to `provider`, and for as long as the reply contains `ToolUse` blocks,
/// executes each named tool from `tools` directly (no preview/permission
/// gate — ADR 0005 restricts that to `PreviewableTool`s, which read-only
/// tools never are) and feeds the results back as a new user turn before
/// asking the provider again. Returns the first reply with no tool calls.
///
/// A `ToolUse` naming a tool not present in `tools` produces an error
/// `ToolResult` rather than failing the whole loop, so the provider can see
/// the failure and recover (e.g. by trying a different tool or apologizing).
pub async fn run_tool_loop<P: Provider>(
    provider: &P,
    input: impl Into<String>,
    tools: &[&dyn ExecutableTool],
) -> Result<Message, P::Error> {
    let tool_specs: Vec<ToolSpec> = tools.iter().map(|tool| tool.to_tool_spec()).collect();
    let mut transcript = vec![Message::user_text(input)];

    loop {
        let reply = provider.send(&transcript, &tool_specs).await?;

        let tool_uses: Vec<(String, String, serde_json::Value)> = reply
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect();

        if tool_uses.is_empty() {
            return Ok(reply);
        }

        transcript.push(reply);

        let mut result_blocks = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            let (content, is_error) = match tools.iter().find(|tool| tool.name() == name.as_str()) {
                Some(tool) => match tool.execute_boxed(input).await {
                    Ok(output) => (output, false),
                    Err(err) => (err.to_string(), true),
                },
                None => (format!("unknown tool: {name}"), true),
            };
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
            });
        }

        transcript.push(Message {
            role: Role::User,
            content: result_blocks,
        });
    }
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

    /// A fake read-only tool standing in for `rokr_tools::read::ReadTool`,
    /// scripted to echo the requested path back in its output so the test
    /// can assert the loop actually executed it (rather than the loop just
    /// passing arguments through untouched).
    struct FakeReadTool;

    impl_executable_tool!(FakeReadTool);

    impl rokr_tools::Tool for FakeReadTool {
        fn name(&self) -> &'static str {
            "read"
        }

        fn description(&self) -> &'static str {
            "fake read tool for tests"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, input: serde_json::Value) -> Result<String, rokr_tools::ToolError> {
            Ok(format!("contents of {}", input["path"]))
        }
    }

    /// Scripts a fixed sequence of replies (popped in order) and records the
    /// `messages` argument of every call, so a test can assert on the
    /// transcript shape the loop builds across iterations.
    struct ScriptedProvider {
        replies: std::sync::Mutex<std::collections::VecDeque<Message>>,
        calls: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    impl Provider for ScriptedProvider {
        type Error = StubError;

        async fn send(
            &self,
            messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<Message, StubError> {
            self.calls.lock().unwrap().push(messages.to_vec());
            self.replies.lock().unwrap().pop_front().ok_or(StubError)
        }
    }

    #[tokio::test]
    async fn loop_executes_tool_call_and_returns_final_reply() {
        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path": "/tmp/whatever.txt"}),
            }],
        };
        let final_reply = Message::assistant_text("final answer");

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_reply.clone(),
                final_reply.clone(),
            ])),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let read_tool = FakeReadTool;
        let tools: [&dyn ExecutableTool; 1] = [&read_tool];

        let result = run_tool_loop(&provider, "read the file", &tools)
            .await
            .expect("loop should succeed");

        assert_eq!(result.role, Role::Assistant);
        assert_eq!(result.text(), "final answer");

        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "provider should be called once per turn");

        // First call: just the initial user turn.
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0].role, Role::User);

        // Second call: initial user turn, the assistant's tool-call turn,
        // and a new turn carrying the tool's result back to the provider.
        assert_eq!(calls[1].len(), 3);
        assert_eq!(calls[1][0].role, Role::User);
        assert_eq!(calls[1][1].role, Role::Assistant);
        assert_eq!(calls[1][2].role, Role::User);

        match &calls[1][2].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }] => {
                assert_eq!(tool_use_id, "call_1");
                assert!(content.contains("/tmp/whatever.txt"));
                assert!(!is_error);
            }
            other => panic!("expected a single ToolResult block, got {other:?}"),
        }
    }
}
