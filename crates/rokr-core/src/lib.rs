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

/// Primitive description of what a gated tool's execution would do, shown to
/// the user before granting permission (`docs/adr/0005-permission-model.md`).
/// `Command` covers `bash`, the only gated tool this ticket wires up.
/// `Diff` is shaped now for the `write`/`edit` tools landing in later
/// tickets, per ADR 0005's "permission-aware from the first tool onward" —
/// nothing produces it yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPayload {
    Command(String),
    Diff { old: String, new: String },
}

/// A gated tool call awaiting user permission: the tool's name plus a
/// primitive description of its effect. Deliberately made of primitives
/// only (no `serde_json::Value`, no tool objects), so a UI layer like
/// `rokr-tui` can render it without depending on `rokr-core`/`rokr-tools`
/// types (see that crate's `run` doc comment on staying decoupled).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub payload: PermissionPayload,
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

    /// Side-effect-free preview for gated tools (ADR 0005). `None` (the
    /// default) for tools that don't implement `rokr_tools::PreviewableTool`
    /// — read/glob/grep/ls stay auto-approved via this default, never
    /// calling into the permission machinery. `Some(Ok(payload))` for a
    /// gated tool, wrapping its preview into the matching
    /// [`PermissionPayload`] variant; `Some(Err(_))` if the preview itself
    /// failed (e.g. malformed input).
    fn preview(
        &self,
        _input: serde_json::Value,
    ) -> Option<Result<PermissionPayload, rokr_tools::ToolError>> {
        None
    }
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

/// Like [`impl_executable_tool`], but for a `rokr_tools::PreviewableTool`:
/// also implements [`ExecutableTool::preview`] by delegating to
/// `PreviewableTool::preview` and mapping its `rokr_tools::Preview` result to
/// the matching [`PermissionPayload`] variant, one-for-one — both enums have
/// the identical shape (`Command(String)` and `Diff { old, new }`), so this
/// mapping is generic across every gated tool, not per-type.
macro_rules! impl_executable_tool_gated {
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

            fn preview(
                &self,
                input: serde_json::Value,
            ) -> Option<Result<PermissionPayload, rokr_tools::ToolError>> {
                Some(rokr_tools::PreviewableTool::preview(self, input).map(
                    |preview| match preview {
                        rokr_tools::Preview::Command(command) => {
                            PermissionPayload::Command(command)
                        }
                        rokr_tools::Preview::Diff { old, new } => {
                            PermissionPayload::Diff { old, new }
                        }
                    },
                ))
            }
        }
    };
}

impl_executable_tool_gated!(rokr_tools::bash::BashTool);
impl_executable_tool_gated!(rokr_tools::write::WriteTool);
impl_executable_tool_gated!(rokr_tools::edit::EditTool);

/// Runs the agent tool loop (ADR 0004) against the running conversation
/// `transcript`, and for as long as the reply contains `ToolUse` blocks,
/// executes each named tool from `tools` directly (no preview/permission
/// gate — ADR 0005 restricts that to `PreviewableTool`s, which read-only
/// tools never are) and feeds the results back as a new user turn before
/// asking the provider again. The caller owns `transcript` and is
/// responsible for having already pushed the new user turn onto it before
/// calling; this function appends the assistant/tool-call/tool-result
/// messages it produces onto the same transcript in place, so the caller can
/// reuse it (with the new turn already recorded) as the seed for a
/// subsequent call. Returns the first reply with no tool calls, which is
/// also pushed onto `transcript` before returning.
///
/// A `ToolUse` naming a tool not present in `tools` produces an error
/// `ToolResult` rather than failing the whole loop, so the provider can see
/// the failure and recover (e.g. by trying a different tool or apologizing).
///
/// For a gated tool (one whose [`ExecutableTool::preview`] returns
/// `Some(_)`), the loop previews it, calls `request_permission` with the
/// resulting [`PermissionRequest`], and only executes on `true`; a `false`
/// (rejected) decision skips execution entirely and instead produces an
/// error `ToolResult` reflecting the rejection, same as an unknown-tool
/// error — the loop continues rather than aborting. Non-gated tools
/// (`preview` returns `None`) skip the permission check and execute
/// directly, exactly as before.
pub async fn run_tool_loop<P, F, Fut>(
    provider: &P,
    transcript: &mut Vec<Message>,
    tools: &[&dyn ExecutableTool],
    request_permission: F,
) -> Result<Message, P::Error>
where
    P: Provider,
    F: Fn(PermissionRequest) -> Fut,
    Fut: Future<Output = bool>,
{
    let tool_specs: Vec<ToolSpec> = tools.iter().map(|tool| tool.to_tool_spec()).collect();

    loop {
        let reply = provider.send(&transcript[..], &tool_specs).await?;

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
            transcript.push(reply.clone());
            return Ok(reply);
        }

        transcript.push(reply);

        let mut result_blocks = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            let (content, is_error) = match tools.iter().find(|tool| tool.name() == name.as_str()) {
                Some(tool) => match tool.preview(input.clone()) {
                    None => match tool.execute_boxed(input).await {
                        Ok(output) => (output, false),
                        Err(err) => (err.to_string(), true),
                    },
                    Some(Err(preview_err)) => (preview_err.to_string(), true),
                    Some(Ok(payload)) => {
                        let request = PermissionRequest {
                            tool_name: name.clone(),
                            payload,
                        };
                        if request_permission(request).await {
                            match tool.execute_boxed(input).await {
                                Ok(output) => (output, false),
                                Err(err) => (err.to_string(), true),
                            }
                        } else {
                            ("permission denied by user".to_string(), true)
                        }
                    }
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

        let mut transcript = vec![Message::user_text("read the file")];

        let result = run_tool_loop(&provider, &mut transcript, &tools, |_request| async {
            true
        })
        .await
        .expect("loop should succeed");

        assert_eq!(result.role, Role::Assistant);
        assert_eq!(result.text(), "final answer");

        assert_eq!(
            transcript.last(),
            Some(&result),
            "the final reply should also be pushed onto the running transcript"
        );

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

    /// A fake gated tool (analogous to `bash`, which implements
    /// `PreviewableTool`) that records whether it was actually executed via
    /// a shared flag, so a test can assert the loop skipped execution when
    /// permission was denied.
    struct FakeGatedTool {
        executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl rokr_tools::Tool for FakeGatedTool {
        fn name(&self) -> &'static str {
            "fake_gated"
        }

        fn description(&self) -> &'static str {
            "fake gated tool for tests"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
        ) -> Result<String, rokr_tools::ToolError> {
            self.executed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("executed".to_string())
        }
    }

    impl rokr_tools::PreviewableTool for FakeGatedTool {
        fn preview(
            &self,
            _input: serde_json::Value,
        ) -> Result<rokr_tools::Preview, rokr_tools::ToolError> {
            Ok(rokr_tools::Preview::Command("fake command".to_string()))
        }
    }

    impl_executable_tool_gated!(FakeGatedTool);

    #[tokio::test]
    async fn loop_skips_execution_when_permission_rejected() {
        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "fake_gated".to_string(),
                input: serde_json::json!({}),
            }],
        };
        let final_reply = Message::assistant_text("final answer after rejection");

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_reply.clone(),
                final_reply.clone(),
            ])),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gated_tool = FakeGatedTool {
            executed: executed.clone(),
        };
        let tools: [&dyn ExecutableTool; 1] = [&gated_tool];

        let mut transcript = vec![Message::user_text("run the command")];

        let result = run_tool_loop(&provider, &mut transcript, &tools, |_request| async {
            false
        })
        .await
        .expect("loop should succeed even when permission is rejected");

        assert_eq!(result.text(), "final answer after rejection");
        assert!(
            !executed.load(std::sync::atomic::Ordering::SeqCst),
            "tool must not execute when permission is rejected"
        );

        let calls = provider.calls.lock().unwrap();
        match &calls[1][2].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }] => {
                assert_eq!(tool_use_id, "call_1");
                assert!(
                    *is_error,
                    "a rejected tool call should be reflected as an error result"
                );
                assert!(
                    content.to_lowercase().contains("denied")
                        || content.to_lowercase().contains("reject"),
                    "result content should reflect the rejection, got: {content}"
                );
            }
            other => panic!("expected a single ToolResult block, got {other:?}"),
        }
    }

    /// Creates a fresh, uniquely-named directory under the system temp dir,
    /// mirroring `crates/rokr/tests/tui_test.rs`'s `unique_temp_dir` helper
    /// (duplicated here rather than shared, since rokr-core has no
    /// `tempfile` dev-dependency to reach for instead).
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rokr-core-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_tool_preview_computed_before_permission_granted() {
        let temp_dir = unique_temp_dir("write-preview");
        let target_file = temp_dir.join("target.txt");
        let old_content = "pre-existing content";
        std::fs::write(&target_file, old_content).unwrap();
        let target_path = target_file.to_string_lossy().into_owned();

        let write_tool = rokr_tools::write::WriteTool;
        let preview = write_tool.preview(serde_json::json!({
            "path": target_path,
            "content": "new content"
        }));

        match preview {
            Some(Ok(PermissionPayload::Diff { old, new })) => {
                assert_eq!(old, old_content);
                assert_eq!(new, "new content");
            }
            other => panic!("expected Some(Ok(PermissionPayload::Diff {{ .. }})), got {other:?}"),
        }

        assert_eq!(
            std::fs::read_to_string(&target_file).unwrap(),
            old_content,
            "preview must not have written to the file"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// F-007: a rejected `write` call must never reach `execute` and must
    /// leave the target file untouched, exercised against the real
    /// `rokr_tools::write::WriteTool` (not `FakeGatedTool`) so this catches a
    /// regression where the permission gate is bypassed for the actual tool.
    #[tokio::test]
    async fn write_tool_reject_leaves_file_untouched() {
        let temp_dir = unique_temp_dir("write-reject");
        let target_file = temp_dir.join("target.txt");
        let original_content = "original content";
        std::fs::write(&target_file, original_content).unwrap();
        let target_path = target_file.to_string_lossy().into_owned();

        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "write".to_string(),
                input: serde_json::json!({
                    "path": target_path,
                    "content": "clobbered content"
                }),
            }],
        };
        let final_reply = Message::assistant_text("final answer after rejection");

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_reply.clone(),
                final_reply.clone(),
            ])),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let write_tool = rokr_tools::write::WriteTool;
        let tools: [&dyn ExecutableTool; 1] = [&write_tool];

        let mut transcript = vec![Message::user_text("overwrite the file")];

        let result = run_tool_loop(&provider, &mut transcript, &tools, |_request| async {
            false
        })
        .await
        .expect("loop should succeed even when permission is rejected");

        assert_eq!(result.text(), "final answer after rejection");
        assert_eq!(
            std::fs::read_to_string(&target_file).unwrap(),
            original_content,
            "a rejected write must never reach execute and must leave the file untouched"
        );

        let calls = provider.calls.lock().unwrap();
        match &calls[1][2].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }] => {
                assert_eq!(tool_use_id, "call_1");
                assert!(
                    *is_error,
                    "a rejected tool call should be reflected as an error result"
                );
                assert!(
                    content.to_lowercase().contains("denied")
                        || content.to_lowercase().contains("reject"),
                    "result content should reflect the rejection, got: {content}"
                );
            }
            other => panic!("expected a single ToolResult block, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
