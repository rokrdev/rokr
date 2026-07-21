//! Agent-as-tool (ticket 30, PRD Phase 4 "Subagents"): a tool whose
//! execution seeds a fresh transcript with a subagent's task, runs
//! `rokr_core::run_tool_loop` to completion against that subagent's own
//! prompt (loaded from `{config_dir}/agents/{name}.md` via the existing
//! `rokr_config::read_agent_prompt`) and a read-only tool subset, and
//! returns only the subagent's final assistant text as the tool result --
//! its internal tool-use/tool-result turns never reach the parent
//! transcript, since `ExecutableTool::execute_boxed` only ever hands back a
//! single `String`.
//!
//! `rokr-tools` cannot depend on `rokr-provider` (it would cycle with
//! `rokr-core -> rokr-tools`), so this can't be an ordinary
//! `rokr_tools::Tool` wrapped by `rokr_core`'s `impl_executable_tool!`
//! macro (that macro is private to `rokr-core` besides). `SubagentTool`
//! implements `rokr_core::ExecutableTool` directly, by hand, here in the
//! `rokr-app` library crate, which already depends on all of `rokr-core`,
//! `rokr-provider`, `rokr-tools`, and `rokr-config`.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

/// A permission-request callback, matching the shape
/// `rokr_core::run_tool_loop` itself expects
/// (`Fn(PermissionRequest) -> Future<Output = bool>`), boxed so it can be
/// stored as a struct field instead of threaded as a generic type
/// parameter. Deliberately not `rokr_tui::PermissionHandle` directly: this
/// keeps `SubagentTool` decoupled from rokr-tui's concrete type, the same
/// way `run_tool_loop`'s own signature stays decoupled from it (see
/// rokr-tui's `run` doc comment on this seam). The `SessionRunner` bridges
/// the two, passing a clone of the SAME `PermissionHandle` the parent's own
/// top-level `request_permission` callback uses -- see `run_subagent`'s
/// doc comment on where the tagging with the subagent's name happens.
pub type PermissionCallback = Box<
    dyn Fn(rokr_core::PermissionRequest) -> Pin<Box<dyn Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// The `subagent` tool: invoking it by name runs a fresh, synchronous
/// `rokr_core::run_tool_loop` to completion against the named subagent's
/// own prompt and a read-only tool subset, returning only its final
/// assistant text.
///
/// Owns a *concrete* `rokr_provider::ResilientProvider<rokr_provider::AnyProvider>`,
/// never a `P: Provider` type parameter on this struct itself. ADR 0009
/// (`docs/adr/0009-provider-trait-location.md`) warns that a
/// generic-over-`Provider` future is not automatically provably `Send` when
/// it crosses an abstract boundary -- and `ExecutableTool::execute_boxed`
/// is exactly that boundary, since it must hand back a
/// `Pin<Box<dyn Future<Output = _> + Send>>`. Keeping this field concrete
/// means the `run_subagent` call inside `execute_boxed` monomorphizes on
/// `ResilientProvider<AnyProvider>` at a single, known call site -- the
/// same reasoning ADR 0009 already applies to `rokr_core::single_turn`.
///
/// F-004: wrapped in `ResilientProvider` (not a bare `AnyProvider`) so a
/// subagent's own provider calls get the same retry/backoff the parent
/// session's send path gets -- before this fix, a single transient failure
/// inside a subagent call had no retry at all.
pub struct SubagentTool {
    provider: rokr_provider::ResilientProvider<rokr_provider::AnyProvider>,
    config_dir: PathBuf,
    request_permission: PermissionCallback,
}

impl SubagentTool {
    pub fn new(
        provider: rokr_provider::ResilientProvider<rokr_provider::AnyProvider>,
        config_dir: PathBuf,
        request_permission: PermissionCallback,
    ) -> Self {
        Self {
            provider,
            config_dir,
            request_permission,
        }
    }
}

impl rokr_core::ExecutableTool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn to_tool_spec(&self) -> rokr_core::ToolSpec {
        rokr_core::ToolSpec {
            name: "subagent".to_string(),
            description: "Delegates a task to a named subagent, running it to completion \
                against its own prompt and a read-only tool subset, and returns only its \
                final answer -- never its internal tool calls."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the subagent to invoke, matching \
                            {config_dir}/agents/{name}.md"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task to give the subagent"
                    }
                },
                "required": ["name", "task"]
            }),
            cache_control: None,
        }
    }

    fn execute_boxed<'a>(
        &'a self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, rokr_tools::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let name = input
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    rokr_tools::ToolError::InvalidInput("missing 'name' field".to_string())
                })?
                .to_string();
            let task = input
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    rokr_tools::ToolError::InvalidInput("missing 'task' field".to_string())
                })?
                .to_string();

            let subagent_prompt = rokr_config::read_agent_prompt(&self.config_dir, &name)
                .map_err(|err| {
                    rokr_tools::ToolError::ExecutionFailed(format!(
                        "failed to load subagent '{name}' prompt: {err}"
                    ))
                })?;

            // Depth cap (PRD Phase 4 "Subagents"): this is exactly the Plan
            // tier's read-only tool set (see the `SessionRunner`'s
            // `AgentTier::Plan` arm), deliberately excluding the `subagent`
            // tool itself -- that omission is what keeps delegation depth
            // capped at one; a subagent built from this set has no way to
            // spawn a further subagent.
            let read = rokr_tools::read::ReadTool;
            let glob = rokr_tools::glob::GlobTool;
            let grep = rokr_tools::grep::GrepTool;
            let ls = rokr_tools::ls::LsTool;
            let tools: [&dyn rokr_core::ExecutableTool; 4] = [&read, &glob, &grep, &ls];

            // Monomorphized on the concrete `ResilientProvider<AnyProvider>`
            // field here -- never a generic `P: Provider` bound at this
            // type's level. See this type's own doc comment and ADR 0009's
            // AFIT `Send` warning.
            run_subagent(
                &self.provider,
                &subagent_prompt,
                task,
                &tools,
                &name,
                &self.request_permission,
            )
            .await
        })
    }
}

/// Tags a gated tool call's `tool_name` with the subagent's name so a
/// permission prompt shown to the user makes clear which subagent is
/// asking (PRD Phase 4 "Subagents": "Permission inheritance"). A
/// standalone pure function so the exact tagging format is easy to reason
/// about and change in one place.
fn tag_permission_request(
    request: rokr_core::PermissionRequest,
    subagent_name: &str,
) -> rokr_core::PermissionRequest {
    rokr_core::PermissionRequest {
        tool_name: format!("{} (subagent: {subagent_name})", request.tool_name),
        payload: request.payload,
    }
}

/// Runs `task` against `subagent_prompt` and `tools` to completion via
/// `rokr_core::run_tool_loop`, returning only the final assistant text.
/// Generic over `P: rokr_core::Provider` (unlike `SubagentTool` itself) so
/// this core logic stays unit-testable against a scripted/fake provider --
/// its only production call site (`SubagentTool::execute_boxed`)
/// monomorphizes it on the concrete `AnyProvider`, which is what keeps the
/// resulting future provably `Send` per ADR 0009 (see `SubagentTool`'s doc
/// comment).
async fn run_subagent<P: rokr_core::Provider>(
    provider: &P,
    subagent_prompt: &str,
    task: String,
    tools: &[&dyn rokr_core::ExecutableTool],
    subagent_name: &str,
    request_permission: &PermissionCallback,
) -> Result<String, rokr_tools::ToolError> {
    let mut transcript = vec![rokr_core::Message::user_text(task)];

    let tagged_request_permission = |request: rokr_core::PermissionRequest| {
        (request_permission)(tag_permission_request(request, subagent_name))
    };

    // Ticket 49 (hooks-tracer-bullet), extended by ticket 50
    // (hooks-remaining-events-and-config): `PreToolUse`/`PostToolUse` hooks
    // fire for the main loop only -- a subagent's own tool calls don't
    // (yet) go through either hook check, hence the hardcoded `None`s
    // rather than threading hook callbacks down from `SubagentTool`.
    let (reply, _usage) = rokr_core::run_tool_loop(
        provider,
        subagent_prompt,
        None,
        &mut transcript,
        tools,
        tagged_request_permission,
        None,
        None,
    )
    .await
    .map_err(|err| rokr_tools::ToolError::ExecutionFailed(err.to_string()))?;

    Ok(reply.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rokr_core::{ContentBlock, ExecutableTool, Message, Role};

    #[derive(Debug)]
    struct StubError;

    impl std::fmt::Display for StubError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stub error")
        }
    }

    /// Scripts a fixed sequence of replies (popped in order), mirroring
    /// `rokr-core`'s own `ScriptedProvider` test fake (duplicated here
    /// rather than shared -- it's a private test-only type in that crate).
    struct ScriptedProvider {
        replies: std::sync::Mutex<std::collections::VecDeque<Message>>,
    }

    impl rokr_core::Provider for ScriptedProvider {
        type Error = StubError;

        async fn send(
            &self,
            _messages: &[Message],
            _tools: &[rokr_core::ToolSpec],
        ) -> Result<(Message, rokr_core::Usage), StubError> {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(StubError)
                .map(|message| (message, rokr_core::Usage::default()))
        }
    }

    /// A fake gated tool (analogous to `bash`/`write`/`edit`), hand-written
    /// rather than via `rokr-core`'s private `impl_executable_tool_gated!`
    /// macro. Used to exercise the permission round-trip a subagent's own
    /// gated tool calls must go through.
    struct FakeGatedTool;

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

    impl rokr_core::ExecutableTool for FakeGatedTool {
        fn name(&self) -> &'static str {
            rokr_tools::Tool::name(self)
        }

        fn to_tool_spec(&self) -> rokr_core::ToolSpec {
            rokr_core::ToolSpec {
                name: rokr_tools::Tool::name(self).to_string(),
                description: rokr_tools::Tool::description(self).to_string(),
                input_schema: rokr_tools::Tool::input_schema(self),
                cache_control: None,
            }
        }

        fn execute_boxed<'a>(
            &'a self,
            input: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, rokr_tools::ToolError>> + Send + 'a>>
        {
            Box::pin(async move { <Self as rokr_tools::Tool>::execute(self, input).await })
        }

        fn preview(
            &self,
            input: serde_json::Value,
        ) -> Option<Result<rokr_core::PermissionPayload, rokr_tools::ToolError>> {
            Some(
                rokr_tools::PreviewableTool::preview(self, input).map(|preview| match preview {
                    rokr_tools::Preview::Command(command) => {
                        rokr_core::PermissionPayload::Command(command)
                    }
                    rokr_tools::Preview::Diff { path, old, new } => {
                        rokr_core::PermissionPayload::Diff { path, old, new }
                    }
                }),
            )
        }
    }

    /// Creates a fresh, uniquely-named directory under the system temp dir,
    /// mirroring `rokr-core`'s own `unique_temp_dir` test helper.
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rokr-subagent-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A scripted subagent run whose own transcript involves a real
    /// multi-turn tool round-trip (a `read` call, then a final answer) must
    /// return only the final assistant text -- never a rendering of its
    /// internal tool-use/tool-result turns. This is `run_subagent`'s core
    /// contract: `ExecutableTool::execute_boxed` can only ever hand a
    /// single `String` back to the parent's `run_tool_loop`, so if this
    /// holds, the parent transcript structurally cannot see the subagent's
    /// internal turns.
    #[tokio::test]
    async fn subagent_tool_returns_only_final_text_not_internal_turns() {
        let temp_dir = unique_temp_dir("returns-only-final-text");
        let target_file = temp_dir.join("notes.txt");
        std::fs::write(&target_file, "some file contents").unwrap();
        let target_path = target_file.to_string_lossy().into_owned();

        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path": target_path}),
                cache_control: None,
            }],
        };
        let final_reply = Message::assistant_text("subagent's final answer");

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_reply,
                final_reply,
            ])),
        };

        let read_tool = rokr_tools::read::ReadTool;
        let tools: [&dyn rokr_core::ExecutableTool; 1] = [&read_tool];

        let request_permission: PermissionCallback =
            Box::new(|_request| Box::pin(async { true }));

        let result = run_subagent(
            &provider,
            "you are a test subagent",
            "read the notes file".to_string(),
            &tools,
            "researcher",
            &request_permission,
        )
        .await
        .expect("subagent run should succeed");

        assert_eq!(
            result, "subagent's final answer",
            "the tool result must be exactly the subagent's final assistant text"
        );
        assert!(
            !result.contains("some file contents") && !result.contains("call_1"),
            "the subagent's internal tool-use/tool-result turns must not leak into the \
             returned text, got: {result}"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// F-004: `SubagentTool` must retry a transient (retryable) provider
    /// failure the same way the parent session's own send path does --
    /// before this fix, `SubagentTool.provider` was a bare, unwrapped
    /// `AnyProvider` with zero retry/backoff, so a single transient 503
    /// would fail the whole subagent call outright. Mirrors
    /// `rokr-provider`'s own `factory.rs` acceptance-test pattern: a mock
    /// server fails the first two attempts with a retryable 503, then
    /// succeeds; the provider handed to `SubagentTool` is wrapped in
    /// `ResilientProvider` with a fast policy, so if `SubagentTool` is
    /// genuinely resilience-wrapped, `execute_boxed` must still succeed and
    /// the mock must record all three attempts.
    #[tokio::test]
    async fn subagent_tool_retries_transient_failure_via_resilient_provider() {
        let mock_server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&mock_server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "subagent final answer after retry"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&mock_server)
            .await;

        let any_provider = rokr_provider::AnyProvider::Anthropic(rokr_provider::AnthropicProvider::new(
            mock_server.uri(),
            "claude-3-5-sonnet-20241022",
            "test-api-key",
        ));
        let fast_policy = rokr_provider::RetryPolicy {
            max_attempts: 5,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            max_elapsed: std::time::Duration::from_secs(5),
        };
        let resilient_provider =
            rokr_provider::ResilientProvider::with_policy(any_provider, fast_policy);

        let temp_dir = unique_temp_dir("retries-transient-failure");
        let agents_dir = temp_dir.join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("researcher.md"), "you are a test subagent").unwrap();

        let request_permission: PermissionCallback =
            Box::new(|_request| Box::pin(async { true }));

        // F-004: `SubagentTool::new` must accept the resilience-wrapped
        // provider directly -- a bare `AnyProvider` field here would mean
        // this transient failure is never retried at all.
        let tool = SubagentTool::new(resilient_provider, temp_dir.clone(), request_permission);

        let result = tool
            .execute_boxed(serde_json::json!({
                "name": "researcher",
                "task": "do the thing"
            }))
            .await
            .expect(
                "subagent call should succeed once the resilience-wrapped provider retries \
                 past the transient 503s",
            );

        assert_eq!(result, "subagent final answer after retry");

        assert_eq!(
            mock_server.received_requests().await.unwrap().len(),
            3,
            "expected exactly 3 attempts (2 retried 503s + 1 successful 200), proving \
             SubagentTool's provider is genuinely resilience-wrapped, not a bare AnyProvider \
             with no retry/backoff"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// A gated tool call made inside a subagent's own loop must round-trip
    /// through the SAME permission callback the parent session uses (here,
    /// simulated with a real channel + oneshot round-trip, the same
    /// mechanics `rokr_tui::PermissionHandle::request` itself uses), and
    /// the resulting `PermissionRequest.tool_name` must be tagged with the
    /// subagent's name so the user can tell which agent is asking.
    #[tokio::test]
    async fn subagent_gated_tool_call_surfaces_permission_request_tagged_with_subagent_name() {
        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "fake_gated".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            }],
        };
        let final_reply = Message::assistant_text("done after permission");

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_reply,
                final_reply,
            ])),
        };

        let gated_tool = FakeGatedTool;
        let tools: [&dyn rokr_core::ExecutableTool; 1] = [&gated_tool];

        let (tx, mut rx) = tokio::sync::mpsc::channel::<(
            rokr_core::PermissionRequest,
            tokio::sync::oneshot::Sender<bool>,
        )>(1);

        let received_request: std::sync::Arc<tokio::sync::Mutex<Option<rokr_core::PermissionRequest>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let received_request_writer = received_request.clone();
        tokio::spawn(async move {
            if let Some((request, responder)) = rx.recv().await {
                *received_request_writer.lock().await = Some(request);
                let _ = responder.send(true);
            }
        });

        let request_permission: PermissionCallback = Box::new(move |request| {
            let tx = tx.clone();
            Box::pin(async move {
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                if tx.send((request, resp_tx)).await.is_err() {
                    return false;
                }
                resp_rx.await.unwrap_or(false)
            })
        });

        let result = run_subagent(
            &provider,
            "you are a test subagent",
            "run the fake gated tool".to_string(),
            &tools,
            "researcher",
            &request_permission,
        )
        .await
        .expect("subagent run should succeed once permission is granted");

        assert_eq!(result, "done after permission");

        let request = received_request
            .lock()
            .await
            .clone()
            .expect("a permission request should have been received over the channel");

        assert!(
            request.tool_name.contains("fake_gated"),
            "tagged tool_name should still identify the original tool, got: {}",
            request.tool_name
        );
        assert!(
            request.tool_name.contains("researcher"),
            "tagged tool_name should identify the subagent that made the call, got: {}",
            request.tool_name
        );
    }
}
