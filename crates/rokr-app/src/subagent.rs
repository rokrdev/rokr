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
use std::sync::Arc;

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

/// R-002 (post-round-1 re-critique, major): a boxed, side-effect-only
/// callback `run_subagent` invokes when `PermissionPolicy::resolve` has
/// ALREADY resolved `Resolution::Deny` for a subagent's gated tool call --
/// ADR 0016 Decision 1 requires that ONLY `Resolution::Prompt` ever reaches
/// the human-facing `PermissionCallback` above; `Deny` must never invoke it.
/// This exists purely so the caller (headless's `HeadlessPermissionRequester`,
/// via `SessionRunner::run_submission`'s typed `H: PermissionRequester`) can
/// still record that a denial happened (its `denied` bookkeeping, used for
/// `subtype: error_permission`) without `run_subagent` needing direct access
/// to that concrete, type-erased `H` -- which is erased at `SubagentTool`'s
/// boundary in the first place (see this module's own doc comment on why
/// ADR 0009 requires that). The interactive TUI passes a no-op here: its
/// `permission_mode` is always `None`, so `Resolution::Deny` never occurs on
/// that path.
pub type NoteDeniedCallback = Box<dyn Fn() + Send + Sync>;

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
///
/// Ticket 74 (`subagent-permission-queue-serialization`): `session_grants`
/// is the SAME `Arc<Mutex<SessionGrants>>` the parent's own
/// `request_permission` closure in `runner.rs` consults (see that closure's
/// doc comment) -- passed here so a subagent's gated tool call is resolved
/// against the identical set of prior "remember for this session" grants,
/// not a separate/absent one. Consulting happens in `run_subagent`, on the
/// call's ORIGINAL (untagged) tool name, before `tag_permission_request`
/// ever runs -- session grants are recorded tool-name-keyed (see
/// `permission_policy::SessionGrants`), and tagging with the subagent's
/// name would otherwise make a grant recorded via the parent's own flow
/// (untagged) never match a subagent's tagged request for the same tool.
pub struct SubagentTool {
    provider: rokr_provider::ResilientProvider<rokr_provider::AnyProvider>,
    config_dir: PathBuf,
    request_permission: PermissionCallback,
    /// R-002: see [`NoteDeniedCallback`]'s doc comment.
    note_denied_without_prompt: NoteDeniedCallback,
    session_grants: Arc<std::sync::Mutex<crate::permission_policy::SessionGrants>>,
    /// F-005 (pre-ship review, major): threaded straight through to
    /// `run_subagent`'s own `PermissionPolicy::resolve` call, replacing a
    /// hardcoded `None` -- see `SessionRunner::permission_mode`'s doc
    /// comment for why this must be the SAME value the parent's own
    /// permission closure resolves against, not a second, independently
    /// re-derived one.
    permission_mode: Option<crate::cli::PermissionMode>,
}

impl SubagentTool {
    pub fn new(
        provider: rokr_provider::ResilientProvider<rokr_provider::AnyProvider>,
        config_dir: PathBuf,
        request_permission: PermissionCallback,
        note_denied_without_prompt: NoteDeniedCallback,
        session_grants: Arc<std::sync::Mutex<crate::permission_policy::SessionGrants>>,
        permission_mode: Option<crate::cli::PermissionMode>,
    ) -> Self {
        Self {
            provider,
            config_dir,
            request_permission,
            note_denied_without_prompt,
            session_grants,
            permission_mode,
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

    /// The one override of this default in the whole codebase (ticket 73,
    /// concurrent-subagent-fan-out; see ADR 0017): a subagent call is a
    /// long-running, self-contained provider round trip against a
    /// read-only tool subset (the depth-1 cap in `execute_boxed` below), so
    /// two subagent calls in the same `run_tool_loop` batch cannot
    /// interfere with each other's execution the way two `write`/`bash`
    /// calls could. This is what lets `run_tool_loop` run multiple
    /// `subagent` calls in one assistant reply concurrently instead of
    /// strictly sequentially, without `run_tool_loop` itself ever matching
    /// on the tool name `"subagent"`.
    fn concurrent_safe(&self) -> bool {
        true
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
                &self.note_denied_without_prompt,
                &self.session_grants,
                self.permission_mode,
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
///
/// `pub` (not the crate-private visibility every other function in this
/// module besides `SubagentTool`'s own methods has): ticket 74
/// (`subagent-permission-queue-serialization`)'s acceptance test lives in
/// the `rokr` crate's own integration suite (`crates/rokr/tests/
/// tui_test.rs`), which cannot reach a private free function in this crate.
/// It needs this function specifically (rather than going through
/// `SubagentTool::execute_boxed`) because `execute_boxed` hardcodes its
/// tool set to the read-only `[read, glob, grep, ls]` roster -- none of
/// which is gated -- so there is no way to exercise a subagent's gated-call
/// permission path through the real, unmodified `SubagentTool` at all. This
/// ticket deliberately does not widen that production roster (a bigger
/// behavior change than it asks for), so the acceptance test instead calls
/// `run_subagent` directly with an injected gated test tool, mirroring the
/// exact pattern this module's own `FakeGatedTool` unit tests already use.
///
/// Pre-ship review F-012 (nit): `#[doc(hidden)]` -- `pub` ONLY for the
/// cross-crate acceptance test described above, not a real public API this
/// crate intends external callers to use.
#[doc(hidden)]
pub async fn run_subagent<P: rokr_core::Provider>(
    provider: &P,
    subagent_prompt: &str,
    task: String,
    tools: &[&dyn rokr_core::ExecutableTool],
    subagent_name: &str,
    request_permission: &PermissionCallback,
    // R-002 (post-round-1 re-critique, major): see `NoteDeniedCallback`'s
    // doc comment.
    note_denied_without_prompt: &NoteDeniedCallback,
    session_grants: &Arc<std::sync::Mutex<crate::permission_policy::SessionGrants>>,
    // Pre-ship review F-005 (major): threaded from `SessionRunner`
    // (`None` for the interactive TUI path, `Some(mode)` for headless) into
    // `PermissionPolicy::resolve` below, replacing a hardcoded `None` --
    // this is exactly the dual-resolver seam the PRD forbade: headless
    // execution must resolve permission modes through the SAME policy
    // layer the TUI does, not a second, independently re-derived one.
    permission_mode: Option<crate::cli::PermissionMode>,
) -> Result<String, rokr_tools::ToolError> {
    let mut transcript = vec![rokr_core::Message::user_text(task)];

    // Ticket 74: consults the SAME `PermissionPolicy`/`SessionGrants` the
    // parent's own `request_permission` closure in `runner.rs` consults
    // (see that closure's doc comment), on the call's ORIGINAL (untagged)
    // `request.tool_name` -- BEFORE `tag_permission_request` below ever
    // runs.
    //
    // On `Resolution::Allow`, `request_permission` (the boxed callback that
    // ultimately round-trips through `rokr_tui::PermissionHandle`'s mpsc
    // channel to the render loop) is never even called -- this is what
    // keeps a session-wide "remember" grant from ever populating that
    // channel for a subagent's gated call, per ticket 74's acceptance
    // criterion ("the fast path that keeps a fan-out of subagents from
    // stalling").
    //
    // R-002 (post-round-1 re-critique, major): `Resolution::Deny` and
    // `Resolution::Prompt` must NOT share an arm -- ADR 0016 Decision 1
    // ("only Prompt reaches the callback"). Round-1's fix (F-005) merged
    // them into one arm that both invoked `request_permission`, which was
    // itself the bug this fixes. `Deny` now calls
    // `note_denied_without_prompt` instead -- for headless (`Some(mode)`),
    // that's what lets `HeadlessPermissionRequester` still record the
    // denial (for `subtype: error_permission`) without ever reaching the
    // human-facing callback; for the interactive TUI (`permission_mode:
    // None`), `Deny` stays structurally unreachable (`None` only ever
    // yields `Allow` or `Prompt`), so its no-op `note_denied_without_prompt`
    // is never even called.
    let tagged_request_permission = |request: rokr_core::PermissionRequest| async move {
        let resolution = {
            // Silent-failure audit, final pre-ship item: recovers from a
            // poisoned lock instead of re-panicking. `SessionGrants` has no
            // partial-mutation invariant that a torn write could leave
            // broken in a way worse than the panic-cascade this avoids (a
            // single prior panic while holding this lock would otherwise
            // permanently lock out the whole session-grants mechanism for
            // the rest of the session).
            let grants = session_grants
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            crate::permission_policy::PermissionPolicy::resolve(
                permission_mode,
                &request.tool_name,
                None,
                &grants,
            )
        };
        match resolution {
            crate::permission_policy::Resolution::Allow => true,
            crate::permission_policy::Resolution::Deny => {
                note_denied_without_prompt();
                false
            }
            crate::permission_policy::Resolution::Prompt => {
                (request_permission)(tag_permission_request(request, subagent_name)).await
            }
        }
    };

    // Ticket 49 (hooks-tracer-bullet), extended by ticket 50
    // (hooks-remaining-events-and-config): `PreToolUse`/`PostToolUse` hooks
    // fire for the main loop only -- a subagent's own tool calls don't
    // (yet) go through either hook check, hence the hardcoded `None`s
    // rather than threading hook callbacks down from `SubagentTool`.
    // Subagents keep the pre-existing unbounded `max_iterations` behavior
    // (`None`) -- headless/eval's cap is scoped to `SessionRunner`'s own
    // send path and doesn't touch subagent orchestration.
    let (reply, _usage) = rokr_core::run_tool_loop(
        provider,
        subagent_prompt,
        None,
        &mut transcript,
        tools,
        tagged_request_permission,
        None,
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
        let note_denied: NoteDeniedCallback = Box::new(|| {});
        let session_grants = Arc::new(std::sync::Mutex::new(
            crate::permission_policy::SessionGrants::new(),
        ));

        let result = run_subagent(
            &provider,
            "you are a test subagent",
            "read the notes file".to_string(),
            &tools,
            "researcher",
            &request_permission,
            &note_denied,
            &session_grants,
            None,
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
        let note_denied: NoteDeniedCallback = Box::new(|| {});

        let session_grants = Arc::new(std::sync::Mutex::new(
            crate::permission_policy::SessionGrants::new(),
        ));

        // F-004: `SubagentTool::new` must accept the resilience-wrapped
        // provider directly -- a bare `AnyProvider` field here would mean
        // this transient failure is never retried at all.
        let tool = SubagentTool::new(
            resilient_provider,
            temp_dir.clone(),
            request_permission,
            note_denied,
            session_grants,
            None,
        );

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

        let note_denied: NoteDeniedCallback = Box::new(|| {});
        let session_grants = Arc::new(std::sync::Mutex::new(
            crate::permission_policy::SessionGrants::new(),
        ));

        let result = run_subagent(
            &provider,
            "you are a test subagent",
            "run the fake gated tool".to_string(),
            &tools,
            "researcher",
            &request_permission,
            &note_denied,
            &session_grants,
            None,
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

    /// R-002 (post-round-1 re-critique, major): a `Resolution::Deny` for a
    /// subagent's gated tool call must NEVER reach the human-facing
    /// `request_permission` callback -- ADR 0016 Decision 1 ("only Prompt
    /// reaches the callback"). `request_permission` here panics if invoked
    /// at all, which is what makes this a real teeth-check: against the
    /// pre-fix code (which routed `Deny` through this SAME arm as
    /// `Prompt`), this test would panic instead of completing.
    /// `note_denied_without_prompt` firing instead is asserted directly via
    /// a shared flag.
    #[tokio::test]
    async fn subagent_deny_mode_never_reaches_request_permission_callback() {
        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "fake_gated".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            }],
        };
        let final_reply = Message::assistant_text("done after denial");

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_reply,
                final_reply,
            ])),
        };

        let gated_tool = FakeGatedTool;
        let tools: [&dyn rokr_core::ExecutableTool; 1] = [&gated_tool];

        let request_permission: PermissionCallback = Box::new(|_request| {
            Box::pin(async {
                panic!(
                    "request_permission must never be invoked for a Deny-mode denial (R-002)"
                );
            })
        });

        let denied_recorded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let denied_recorded_writer = denied_recorded.clone();
        let note_denied: NoteDeniedCallback = Box::new(move || {
            denied_recorded_writer.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let session_grants = Arc::new(std::sync::Mutex::new(
            crate::permission_policy::SessionGrants::new(),
        ));

        let result = run_subagent(
            &provider,
            "you are a test subagent",
            "run the fake gated tool".to_string(),
            &tools,
            "researcher",
            &request_permission,
            &note_denied,
            &session_grants,
            Some(crate::cli::PermissionMode::Deny),
        )
        .await
        .expect("subagent run should still complete even though the gated call was denied");

        assert_eq!(result, "done after denial");
        assert!(
            denied_recorded.load(std::sync::atomic::Ordering::SeqCst),
            "note_denied_without_prompt must be called for a Deny-mode denial"
        );
    }

    /// Ticket 74 (`subagent-permission-queue-serialization`), test 1: two
    /// subagents dispatched CONCURRENTLY (`tokio::join!`, mirroring ticket
    /// 73's own concurrent-dispatch mechanism), each making one gated call
    /// via `FakeGatedTool`, must each receive back EXACTLY the answer sent
    /// for its OWN request -- never the other's. Extends the single-caller
    /// `subagent_gated_tool_call_surfaces_permission_request_tagged_with_
    /// subagent_name` pattern immediately above (same hand-rolled
    /// mpsc/oneshot responder shape) to two concurrent callers whose
    /// answers deliberately differ: "alice" is approved, "bob" is denied.
    ///
    /// A `ConditionalProvider` (rather than the fixed-sequence
    /// `ScriptedProvider`) stands in as each subagent's underlying model:
    /// its second reply ECHOES the just-produced `ToolResult` content
    /// (`"executed"` on approval, `"permission denied by user"` on denial)
    /// back as the subagent's final answer, so a swapped or dropped
    /// correlation between a request and its own response becomes visible
    /// in the RETURNED TEXT, not just in which branch executed internally.
    /// It's pure/stateless (computed only from the incoming `messages`
    /// slice), so ONE shared instance safely serves both concurrent calls
    /// with no risk of the test harness itself racing.
    ///
    /// Red-proof note (per this ticket's TDD process step 3): as written,
    /// this test passed immediately -- there is no cross-talk bug in this
    /// layer today (each call already gets its own oneshot channel, and
    /// `run_subagent`'s local `transcript`/closures are call-local, not
    /// shared mutable state). To confirm the test has teeth rather than
    /// being a tautology, the responder above was temporarily changed to
    /// send a single fixed answer to BOTH received requests regardless of
    /// which subagent tagged them (collapsing the `alice`/`bob` branch to
    /// always `true`) -- reproducing a "cross-talk"-shaped bug (both
    /// callers get the same answer) -- and this test failed on bob's
    /// assertion as expected. Restored to the discriminating responder
    /// below afterward.
    #[tokio::test]
    async fn concurrent_permission_requests_from_two_subagents_each_receive_their_own_correct_response()
     {
        struct ConditionalProvider;

        impl rokr_core::Provider for ConditionalProvider {
            type Error = StubError;

            async fn send(
                &self,
                messages: &[Message],
                _tools: &[rokr_core::ToolSpec],
            ) -> Result<(Message, rokr_core::Usage), StubError> {
                let last = messages.last().ok_or(StubError)?;
                let tool_result_content = last.content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                    _ => None,
                });
                let reply = match tool_result_content {
                    None => Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "fake_gated".to_string(),
                            input: serde_json::json!({}),
                            cache_control: None,
                        }],
                    },
                    Some(content) => Message::assistant_text(format!("outcome: {content}")),
                };
                Ok((reply, rokr_core::Usage::default()))
            }
        }

        let provider = ConditionalProvider;
        let gated_tool = FakeGatedTool;
        let tools: [&dyn rokr_core::ExecutableTool; 1] = [&gated_tool];

        let (tx, mut rx) = tokio::sync::mpsc::channel::<(
            rokr_core::PermissionRequest,
            tokio::sync::oneshot::Sender<bool>,
        )>(4);

        // Drains BOTH requests off the ONE shared channel -- mirroring how
        // a real render loop drains `rokr_tui::PermissionHandle`'s single
        // mpsc receiver -- and answers each based on which subagent it was
        // tagged for, deciding PER REQUEST rather than by arrival order (so
        // this doesn't just get lucky if the two calls happen to arrive in
        // a fixed order).
        tokio::spawn(async move {
            for _ in 0..2 {
                if let Some((request, responder)) = rx.recv().await {
                    let approve = request.tool_name.contains("alice");
                    let _ = responder.send(approve);
                }
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

        let note_denied: NoteDeniedCallback = Box::new(|| {});
        let session_grants = Arc::new(std::sync::Mutex::new(
            crate::permission_policy::SessionGrants::new(),
        ));

        let (alice_result, bob_result) = tokio::join!(
            run_subagent(
                &provider,
                "you are a test subagent",
                "alice's task".to_string(),
                &tools,
                "alice",
                &request_permission,
                &note_denied,
                &session_grants,
                None,
            ),
            run_subagent(
                &provider,
                "you are a test subagent",
                "bob's task".to_string(),
                &tools,
                "bob",
                &request_permission,
                &note_denied,
                &session_grants,
                None,
            ),
        );

        let alice_result = alice_result.expect("alice's subagent run should succeed");
        let bob_result = bob_result.expect("bob's subagent run should succeed");

        assert_eq!(
            alice_result, "outcome: executed",
            "alice's gated call was approved -- her result must reflect the tool actually \
             running, got: {alice_result}"
        );
        assert_eq!(
            bob_result, "outcome: permission denied by user",
            "bob's gated call was denied -- his result must reflect the denial, NOT alice's \
             approval (cross-talk), got: {bob_result}"
        );
    }

    /// Ticket 73 (concurrent-subagent-fan-out), test 2 (acceptance): the
    /// REAL `SubagentTool` (not a fake), invoked TWICE via a single
    /// top-level assistant reply naming the `subagent` tool twice, must
    /// have both underlying provider HTTP calls genuinely overlap in wall
    /// clock time -- not merely both eventually complete -- when
    /// dispatched through a real top-level `rokr_core::run_tool_loop`.
    ///
    /// **Deviation from the originally sketched harness**, per this
    /// ticket's own escape hatch ("if this exact harness proves
    /// impractical... use your engineering judgment"): the original plan
    /// was a `wiremock::Respond` impl blocking on a shared `std::sync::
    /// Barrier` inside `respond()`, mirroring `rokr-core`'s own test 1.
    /// That does NOT work here -- investigation of `wiremock` 0.6's
    /// `BareMockServer` (`mock_server/hyper.rs`) shows every incoming
    /// request is handled under a single `tokio::sync::RwLock::write()`
    /// held for the ENTIRE `handle_request` call, which is exactly where
    /// `Respond::respond()` gets invoked. A single `MockServer` therefore
    /// only ever has ONE request "inside `respond()`" at a time, by
    /// wiremock's own design -- a barrier of 2 in `respond()` deadlocks
    /// unconditionally, even with perfectly concurrent client dispatch,
    /// because the second request can never even reach `respond()` until
    /// the first (stuck on the barrier) releases that lock. Confirmed
    /// empirically: the barrier-based version of this test hung
    /// indefinitely both before AND after implementing concurrent
    /// dispatch in `run_tool_loop`, proving the harness itself -- not
    /// `run_tool_loop` -- was the problem.
    ///
    /// This version proves the same thing (genuine wall-clock overlap)
    /// with timing instead of a rendezvous: every mocked response carries
    /// `ResponseTemplate::set_delay(PER_CALL_DELAY)`. Per wiremock's own
    /// `hyper.rs` (see its comment: "We do not wait for the delay within
    /// the handler otherwise we would be holding on to the write-side of
    /// the `RwLock`..."), that delay is awaited strictly AFTER the
    /// per-request write lock is released, so a second request's
    /// `respond()` computation can run (and start ITS OWN delay) while
    /// the first request is still delaying. Two calls awaited
    /// concurrently therefore take roughly `PER_CALL_DELAY` in total;
    /// two calls awaited sequentially take roughly `2 * PER_CALL_DELAY`.
    /// `TIMEOUT_BOUND` sits strictly between those two figures (with
    /// generous slack on both sides for CI jitter and localhost network
    /// overhead), so `tokio::time::timeout` reproduces the same
    /// `Err(Elapsed(()))` red the barrier-based design was meant to
    /// produce: it fires under today's still-sequential dispatch and
    /// passes once the calls genuinely overlap.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_concurrent_subagent_tool_calls_in_one_reply_are_dispatched_concurrently() {
        const PER_CALL_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
        // Comfortably above one round trip (~500ms + localhost overhead)
        // and comfortably below two sequential round trips (~1000ms +
        // overhead), so it cleanly separates concurrent from sequential
        // dispatch without being tight enough to flake on a loaded CI box.
        const TIMEOUT_BOUND: std::time::Duration = std::time::Duration::from_millis(850);

        let mock_server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).unwrap_or_default();
                let task_text = body["messages"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();

                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "text",
                            "text": format!("final answer for: {task_text}")
                        }],
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    }))
                    .set_delay(PER_CALL_DELAY)
            })
            .mount(&mock_server)
            .await;

        let any_provider = rokr_provider::AnyProvider::Anthropic(
            rokr_provider::AnthropicProvider::new(
                mock_server.uri(),
                "claude-3-5-sonnet-20241022",
                "test-api-key",
            ),
        );
        let resilient_provider = rokr_provider::ResilientProvider::new(any_provider);

        let temp_dir = unique_temp_dir("concurrent-subagent-dispatch");
        let agents_dir = temp_dir.join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("researcher.md"), "you are a test subagent").unwrap();

        let request_permission: PermissionCallback =
            Box::new(|_request| Box::pin(async { true }));
        let note_denied: NoteDeniedCallback = Box::new(|| {});

        let session_grants = Arc::new(std::sync::Mutex::new(
            crate::permission_policy::SessionGrants::new(),
        ));
        let subagent_tool = SubagentTool::new(
            resilient_provider,
            temp_dir.clone(),
            request_permission,
            note_denied,
            session_grants,
            None,
        );
        let tools: [&dyn ExecutableTool; 1] = [&subagent_tool];

        let top_level_tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call_a".to_string(),
                    name: "subagent".to_string(),
                    input: serde_json::json!({"name": "researcher", "task": "task-a"}),
                    cache_control: None,
                },
                ContentBlock::ToolUse {
                    id: "call_b".to_string(),
                    name: "subagent".to_string(),
                    input: serde_json::json!({"name": "researcher", "task": "task-b"}),
                    cache_control: None,
                },
            ],
        };
        let top_level_final_reply = Message::assistant_text("top-level done");

        let top_level_provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                top_level_tool_call_reply,
                top_level_final_reply,
            ])),
        };

        let mut transcript = vec![Message::user_text("run two subagents concurrently")];

        let timeout_result = tokio::time::timeout(
            TIMEOUT_BOUND,
            rokr_core::run_tool_loop(
                &top_level_provider,
                "you are a test top-level agent",
                None,
                &mut transcript,
                &tools,
                |_request| async { true },
                None,
                None,
                None,
            ),
        )
        .await;

        let (final_message, _usage) = timeout_result
            .expect(
                "run_tool_loop should complete within the timeout -- a timeout here means two \
                 concurrent `subagent` tool calls in the same reply are still taking roughly \
                 2x PER_CALL_DELAY, i.e. still being dispatched sequentially rather than \
                 overlapping",
            )
            .expect("top-level loop should succeed");

        assert_eq!(final_message.text(), "top-level done");

        match &transcript[transcript.len() - 2].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id: id_a,
                content: content_a,
                is_error: is_error_a,
                ..
            }, ContentBlock::ToolResult {
                tool_use_id: id_b,
                content: content_b,
                is_error: is_error_b,
                ..
            }] => {
                assert_eq!(id_a, "call_a");
                assert!(!is_error_a);
                assert_eq!(content_a, "final answer for: task-a");
                assert_eq!(id_b, "call_b");
                assert!(!is_error_b);
                assert_eq!(content_b, "final answer for: task-b");
            }
            other => panic!("expected two ToolResult blocks in original order, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
