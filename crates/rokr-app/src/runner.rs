//! Ticket 52 (clap-and-sessionrunner-extraction): `SessionRunner` owns the
//! submit-and-run orchestration that used to live inline in `main.rs`'s
//! `submit` closure -- assembling the per-turn tool set, expanding
//! `@`-mentions, firing `UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`Stop`
//! hooks, calling `rokr_core::run_tool_loop`, persisting the `Turn` /
//! `Checkpoint` / `Compaction` records via `SessionHandle`, and running the
//! auto-compaction check. This is a pure move: `run_submission` reproduces
//! the moved closure's behavior byte-for-byte, so the TUI driver in
//! `main.rs` now just constructs a `SessionRunner` and forwards each
//! Enter-press into it.

use std::sync::Arc;

use rokr_core::Provider;

use crate::cli::AgentTier;
use crate::subagent;

/// F-003: the real send path (`run_tool_loop`, `compact_transcript`) AND
/// `subagent::SubagentTool` (ticket 30, F-004) share this SINGLE
/// resilience-wrapped provider -- previously this was two separate locks (a
/// resilience-wrapped one for the send path, a bare unwrapped one for
/// subagents), written and read non-atomically, so a `/model` switch racing
/// `submit`'s two reads could split the parent and a subagent onto
/// different backends. `ResilientProvider<AnyProvider>` is `Clone` (see
/// `resilience.rs`), so a single `Arc<RwLock<..>>` is enough: ticket 29's
/// read-clone-drop-guard pattern (clone the current provider out from
/// behind a read lock and drop the guard immediately, so `/model` never
/// blocks on an in-flight request's `.await`, which can legitimately run as
/// long as the retry policy's `max_elapsed`) clones the whole
/// `ResilientProvider<AnyProvider>` value directly rather than an inner
/// `Arc`.
pub type SharedProvider =
    Arc<tokio::sync::RwLock<rokr_provider::ResilientProvider<rokr_provider::AnyProvider>>>;

/// The permission surface `run_submission` bridges rokr-core's
/// `PermissionRequest` onto. In production this is
/// `rokr_tui::PermissionHandle` (the impl below), round-tripping a gated
/// tool call through the TUI's render loop exactly as the pre-extraction
/// `submit` closure did. Abstracting it as a trait (rather than hardcoding
/// `PermissionHandle`) lets `SessionRunner` be unit-tested without a live
/// TUI: `rokr_tui::PermissionHandle` has no public constructor, so a test
/// substitutes its own grant/deny implementation. The method signature
/// mirrors `PermissionHandle::request` exactly, so the behavior the runner
/// drives is identical whichever implementation is supplied.
pub trait PermissionRequester: Clone + Send + Sync + 'static {
    fn request(
        &self,
        request: rokr_tui::PermissionRequest,
    ) -> impl std::future::Future<Output = rokr_tui::PermissionDecision> + Send;
}

impl PermissionRequester for rokr_tui::PermissionHandle {
    fn request(
        &self,
        request: rokr_tui::PermissionRequest,
    ) -> impl std::future::Future<Output = rokr_tui::PermissionDecision> + Send {
        rokr_tui::PermissionHandle::request(self, request)
    }
}

/// Owns everything one submission needs and drives it to a terminal state.
///
/// Every field here is exactly what the pre-extraction `submit` closure
/// captured by move from `main.rs`; [`SessionRunner::run_submission`] clones
/// them per-invocation (as the closure did in its own prologue) so the
/// returned future owns everything and borrows nothing from `&self` -- which
/// is what keeps it `Send + 'static` and lets `main.rs` hand a thin
/// `move |input, permission| runner.run_submission(input, permission)`
/// closure to `rokr_tui::run` (whose `submit` bound is
/// `Fn(String, PermissionHandle) -> Future + Send + Sync + 'static`).
pub struct SessionRunner {
    /// The active, resilience-wrapped provider (or the startup construction
    /// error, surfaced on the first submit rather than crashing the TUI).
    pub provider: Result<SharedProvider, String>,
    /// In-memory running conversation history, accumulated across submits.
    pub transcript: Arc<tokio::sync::Mutex<Vec<rokr_core::Message>>>,
    /// The assembled base system prompt (agent tier prompt + project
    /// context + `SessionStart` hook output), built once at startup.
    pub system_prompt: String,
    /// The repo map, regenerated on `/compact` (written by `command`), read
    /// here per turn.
    pub repo_map: Arc<std::sync::Mutex<Option<String>>>,
    /// The most recent turn's real (non-zero) FINAL-CALL usage figure
    /// (`TurnUsage::final_call`, not `cumulative` -- see that type's doc
    /// comment), shared so an unreported turn can fall back to it. Used only
    /// for the auto-compaction/context-percent math below, which cares about
    /// the live context window's occupancy, not this turn's total spend.
    pub last_known_usage: Arc<std::sync::Mutex<Option<rokr_core::Usage>>>,
    /// F-001: the most recently completed submission's full [`rokr_core::TurnUsage`]
    /// (both `final_call` and `cumulative`), read by `crate::headless::run_result_object`
    /// after `run_submission` returns to report/cost the turn's TOTAL spend
    /// (`cumulative`) rather than just its final round trip
    /// (`last_known_usage`/`final_call`). Headless drives exactly one
    /// submission per process, so this is unambiguously "this run's usage"
    /// there; the TUI updates it every submit but doesn't read it back.
    pub last_turn_usage: Arc<std::sync::Mutex<Option<rokr_core::TurnUsage>>>,
    /// The user-scope config dir, used to load a named subagent's prompt.
    pub config_dir: std::path::PathBuf,
    /// The currently-active session writer (swappable via `/resume`).
    pub session_handle: Arc<tokio::sync::RwLock<Option<Arc<rokr_session::SessionHandle>>>>,
    /// Counter equal to the count of prior `Turn` records; the index the
    /// next submitted turn's own `Turn` record will occupy.
    pub turn_index: Arc<std::sync::Mutex<usize>>,
    /// Central session-data dir, used to build a `CheckpointStore`.
    pub data_dir: std::path::PathBuf,
    /// Status-line channel into the render loop (context %, notices).
    pub status_tx: std::sync::mpsc::Sender<rokr_tui::SessionStatus>,
    /// Handles to every spawned MCP server, snapshotted for the tool set.
    pub mcp_server_handles: Arc<Vec<rokr_mcp::McpServerHandle>>,
    /// MCP degraded-server notice channel, threaded into `snapshot_tools`.
    pub mcp_notice_tx: std::sync::mpsc::Sender<String>,
    /// HTTP MCP server origins (name -> URL) for permission-prompt text.
    pub mcp_http_origins: Arc<std::collections::HashMap<String, String>>,
    /// Configured hooks by event name.
    pub hooks_config: Arc<std::collections::HashMap<String, Vec<rokr_config::HookEntry>>>,
    /// The agent tool tier (Plan/Build), fixed for the session.
    pub agent: AgentTier,
    /// Token budget for the context-percent / auto-compaction math.
    pub context_window_size: u32,
    /// Fraction of `context_window_size` that triggers auto-compaction.
    pub auto_compact_threshold: f64,
    /// F-005: caps `rokr_core::run_tool_loop`'s `Provider::send` round trips
    /// for every submission this runner drives. `None` (the TUI's choice)
    /// preserves unbounded looping, since a human at the keyboard can always
    /// interrupt; headless/eval callers pass a conservative `Some(_)` (see
    /// `crate::headless::HEADLESS_MAX_ITERATIONS`).
    pub max_iterations: Option<u32>,
    /// Ticket 72 (`tui-session-allowlist-grant`): accumulates "remember for
    /// this session" grants recorded via
    /// [`rokr_tui::PermissionDecision::AllowAndRemember`], consulted via
    /// `crate::permission_policy::PermissionPolicy::resolve` at the top of
    /// `run_submission`'s (parent-only, not subagent's) permission closure
    /// so a later gated call to an already-granted tool this session never
    /// re-reaches the interactive prompt. Mirrors `last_known_usage`'s
    /// `Arc<std::sync::Mutex<..>>` shape exactly -- a plain sync mutex is
    /// fine here since the lock is never held across an `.await`.
    pub session_grants: Arc<std::sync::Mutex<crate::permission_policy::SessionGrants>>,
}

impl SessionRunner {
    /// Drives ONE submission (`input` plus a per-call permission surface)
    /// through to a terminal state -- the reply text on success, an error
    /// string on failure -- reproducing the pre-extraction `submit`
    /// closure's orchestration exactly. Returns an owned `Send + 'static`
    /// future (it clones every field up front and borrows nothing from
    /// `&self`), so `main.rs` can forward it straight into `rokr_tui::run`.
    pub fn run_submission<H: PermissionRequester>(
        &self,
        input: String,
        permission: H,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send + 'static {
        let provider = self.provider.clone();
        let transcript = self.transcript.clone();
        let system_prompt = self.system_prompt.clone();
        let repo_map = self.repo_map.clone();
        let last_known_usage = self.last_known_usage.clone();
        let last_turn_usage = self.last_turn_usage.clone();
        let config_dir = self.config_dir.clone();
        let session_handle = self.session_handle.clone();
        let turn_index = self.turn_index.clone();
        let data_dir = self.data_dir.clone();
        let status_tx = self.status_tx.clone();
        let mcp_server_handles = self.mcp_server_handles.clone();
        let mcp_notice_tx = self.mcp_notice_tx.clone();
        let mcp_http_origins = self.mcp_http_origins.clone();
        let hooks_config = self.hooks_config.clone();
        let agent = self.agent;
        let context_window_size = self.context_window_size;
        let auto_compact_threshold = self.auto_compact_threshold;
        let max_iterations = self.max_iterations;
        let session_grants = self.session_grants.clone();
        async move {
            let provider = provider?;
            // F-003: ONE read lock, ONE clone of the current
            // resilience-wrapped provider snapshot, shared by both
            // the send path below and `SubagentTool` (F-004) --
            // guard dropped immediately, never held across an
            // `.await`, so `/model` never blocks on an in-flight
            // request's `.await` (which can legitimately run as
            // long as the retry policy's `max_elapsed`).
            let provider: rokr_provider::ResilientProvider<rokr_provider::AnyProvider> =
                provider.read().await.clone();

            // All eight tools are constructed unconditionally
            // (they're cheap zero-sized unit structs); which ones
            // actually land in `tools` depends on the agent tier.
            // read/glob/grep/ls auto-approve (ADR 0005: none are
            // `PreviewableTool`s); bash, write, edit, and webfetch
            // are gated and round-trip through the permission
            // callback below, and only exist in the tool set for
            // the `Build` tier.
            let read = rokr_tools::read::ReadTool;
            let glob = rokr_tools::glob::GlobTool;
            let grep = rokr_tools::grep::GrepTool;
            let ls = rokr_tools::ls::LsTool;
            // Ticket 69 (bash-command-sandbox-confinement): confine `bash`
            // to the process's cwd via `SeatbeltSandbox`, computed once per
            // submission rather than once per process so a later ticket can
            // vary it per-session without touching this call site.
            let workspace_root = std::env::current_dir().map_err(|e| e.to_string())?;
            let write = rokr_tools::write::WriteTool::new(workspace_root.clone());
            let edit = rokr_tools::edit::EditTool::new(workspace_root.clone());
            let bash = rokr_tools::bash::BashTool::new(workspace_root);
            let webfetch = rokr_tools::webfetch::WebfetchTool;

            // Ticket 28 (websearch-tool): `websearch` needs an
            // actual `rokr_tools::websearch::NativeSearchCapability`
            // object to delegate to — `provider.native_search_capable()`
            // alone is just a thin bool signal (see that method's
            // doc comment on why `rokr-core` can't hand back more
            // than that). No adapter in this codebase constructs a
            // real capability object yet (`rokr-provider` doesn't
            // depend on `rokr-tools`, and bridging a real
            // Anthropic-backed capability through here is out of
            // scope for this ticket), so `native_search_capability`
            // is provably `None` today. The `native_search_capable()`
            // query below is still real, not hardcoded, so a
            // follow-up ticket that supplies a genuine capability
            // object lights `websearch` up without touching this
            // gating logic again.
            let native_search_capability: Option<
                Arc<dyn rokr_tools::websearch::NativeSearchCapability>,
            > = None;
            let websearch = if provider.native_search_capable() {
                rokr_tools::websearch::for_capability(native_search_capability)
            } else {
                None
            };

            // Ticket 30 (subagent-tool): cloned before
            // `request_permission` below moves `permission` into
            // its own closure -- the subagent tool's callback needs
            // its own clone of the SAME handle so a subagent's
            // gated tool calls round-trip through the identical
            // channel the parent's own gated tool calls do.
            let subagent_permission = permission.clone();

            // Ticket 38 (checkpoint-pre-images): both
            // `request_permission` below and the mirrored
            // `subagent_request_permission` need their OWN owned
            // clones of `session_handle`/`turn_index`/`data_dir` to
            // `move` into themselves, since `session_handle` and
            // `turn_index` are still read again later in this same
            // `submit` body (append_turn / the increment above's
            // sibling call site) -- cloning here, not moving the
            // originals, mirrors `subagent_permission`'s existing
            // pattern immediately above.
            let request_permission_session_handle = session_handle.clone();
            let request_permission_turn_index = turn_index.clone();
            let request_permission_data_dir = data_dir.clone();
            let request_permission_mcp_http_origins = mcp_http_origins.clone();
            // Ticket 72: cloned into the PARENT `request_permission` closure.
            // Ticket 74 (`subagent-permission-queue-serialization`): the
            // subagent closure below no longer needs its own clone here --
            // it forwards the SAME `session_grants` handle straight into
            // `SubagentTool::new` (see the `subagent_tool` construction
            // below), which is where `run_subagent` actually consults it
            // (on the call's untagged tool name, before subagent-tagging).
            let request_permission_session_grants = session_grants.clone();
            let subagent_request_permission_session_handle = session_handle.clone();
            let subagent_request_permission_turn_index = turn_index.clone();
            let subagent_request_permission_data_dir = data_dir.clone();
            let subagent_request_permission_mcp_http_origins = mcp_http_origins.clone();

            // Bridges rokr-core's `PermissionRequest` (tool name +
            // `PermissionPayload`) to rokr-tui's primitive
            // `PermissionRequest` (tool name + a display string),
            // round-tripping through the TUI's render loop via
            // `permission`. This is the seam rokr-tui's `run` doc
            // comment calls out: rokr-tui stays decoupled from
            // rokr-core's specific types, so the app crate bridges them.
            // Ticket 38 (checkpoint-pre-images): on GRANT, also
            // captures a pre-image snapshot for a `Diff` payload
            // (write/edit) and appends a correlating `Checkpoint`
            // record -- see `capture_checkpoint_if_granted_diff`'s
            // doc comment. `PermissionPayload::Command` (bash)
            // snapshots nothing, falling out structurally from the
            // match below rather than a runtime special-case.
            let request_permission = move |request: rokr_core::PermissionRequest| {
                let permission = permission.clone();
                let session_handle = request_permission_session_handle.clone();
                let turn_index = request_permission_turn_index.clone();
                let data_dir = request_permission_data_dir.clone();
                let mcp_http_origins = request_permission_mcp_http_origins.clone();
                let session_grants = request_permission_session_grants.clone();
                async move {
                    let (detail, diff_path_and_old) = match request.payload {
                        rokr_core::PermissionPayload::Command(command) => {
                            (rokr_tui::PermissionDetail::Text(command), None)
                        }
                        rokr_core::PermissionPayload::Diff { path, old, new } => (
                            rokr_tui::PermissionDetail::Diff {
                                old: old.clone(),
                                new,
                            },
                            Some((path, old)),
                        ),
                        rokr_core::PermissionPayload::ToolCall {
                            server,
                            tool,
                            input_pretty,
                        } => {
                            let origin = mcp_http_origins.get(&server).map(String::as_str);
                            (
                                rokr_tui::PermissionDetail::Text(
                                    format_tool_call_permission_text(
                                        &server,
                                        &tool,
                                        &input_pretty,
                                        origin,
                                    ),
                                ),
                                None,
                            )
                        }
                    };
                    // Ticket 72 (`tui-session-allowlist-grant`): resolved
                    // BEFORE ever reaching the interactive prompt below.
                    // `mode: None` -- the interactive TUI has no ambient
                    // permission mode (`--permission-mode` stays
                    // headless-only, see `permission_policy::resolve`'s doc
                    // comment). The lock is dropped immediately (not held
                    // across the `.await` in the `Prompt` arm below).
                    let resolution = {
                        let grants = session_grants.lock().unwrap();
                        crate::permission_policy::PermissionPolicy::resolve(
                            None,
                            &request.tool_name,
                            None,
                            &grants,
                        )
                    };
                    let granted = match resolution {
                        // A prior "remember for this session" grant already
                        // covers this tool -- skip the prompt entirely.
                        crate::permission_policy::Resolution::Allow => true,
                        // Structurally unreachable with `mode: None` today
                        // (`None` only ever yields `Allow` or `Prompt`, see
                        // `permission_policy::resolve`) -- kept so the match
                        // stays exhaustive against `Resolution`'s full shape.
                        crate::permission_policy::Resolution::Deny => false,
                        crate::permission_policy::Resolution::Prompt => {
                            let decision = permission
                                .request(rokr_tui::PermissionRequest {
                                    tool_name: request.tool_name.clone(),
                                    detail,
                                })
                                .await;
                            if decision == rokr_tui::PermissionDecision::AllowAndRemember {
                                session_grants
                                    .lock()
                                    .unwrap()
                                    .grant(request.tool_name.clone());
                            }
                            matches!(
                                decision,
                                rokr_tui::PermissionDecision::Allow
                                    | rokr_tui::PermissionDecision::AllowAndRemember
                            )
                        }
                    };
                    if granted {
                        capture_checkpoint_if_granted_diff(
                            diff_path_and_old,
                            &data_dir,
                            &session_handle,
                            &turn_index,
                        )
                        .await;
                    }
                    granted
                }
            };

            // Ticket 30 (subagent-tool): bridges rokr-core's
            // PermissionRequest to the SAME permission surface the
            // parent's own request_permission above uses (PRD
            // Phase 4 "Subagents": "Permission inheritance").
            // Tagging with the subagent's name happens inside
            // `subagent::run_subagent`, not here -- this closure
            // only forwards the (already-tagged) request. Ticket 38
            // (checkpoint-pre-images): wired the SAME way as
            // `request_permission` above -- a subagent's gated tool
            // calls happen within the same parent turn, so they
            // share the SAME `turn_index` (not a subagent-local
            // counter).
            let subagent_request_permission: subagent::PermissionCallback =
                Box::new(move |request: rokr_core::PermissionRequest| {
                    let permission = subagent_permission.clone();
                    let session_handle = subagent_request_permission_session_handle.clone();
                    let turn_index = subagent_request_permission_turn_index.clone();
                    let data_dir = subagent_request_permission_data_dir.clone();
                    let mcp_http_origins =
                        subagent_request_permission_mcp_http_origins.clone();
                    Box::pin(async move {
                        let (detail, diff_path_and_old) = match request.payload {
                            rokr_core::PermissionPayload::Command(command) => {
                                (rokr_tui::PermissionDetail::Text(command), None)
                            }
                            rokr_core::PermissionPayload::Diff { path, old, new } => (
                                rokr_tui::PermissionDetail::Diff {
                                    old: old.clone(),
                                    new,
                                },
                                Some((path, old)),
                            ),
                            rokr_core::PermissionPayload::ToolCall {
                                server,
                                tool,
                                input_pretty,
                            } => {
                                let origin =
                                    mcp_http_origins.get(&server).map(String::as_str);
                                (
                                    rokr_tui::PermissionDetail::Text(
                                        format_tool_call_permission_text(
                                            &server,
                                            &tool,
                                            &input_pretty,
                                            origin,
                                        ),
                                    ),
                                    None,
                                )
                            }
                        };
                        // Ticket 72: mechanical adaptation to
                        // `PermissionDecision` only. Ticket 74
                        // (`subagent-permission-queue-serialization`): the
                        // `PermissionPolicy`/`session_grants` consultation
                        // for a subagent's gated call now happens one layer
                        // up, in `subagent::run_subagent`, on the call's
                        // ORIGINAL (untagged) tool name -- BEFORE this
                        // closure is ever invoked at all when a session-wide
                        // grant already covers it. This closure only runs
                        // for the `Resolution::Prompt` case (or when
                        // `run_subagent`'s own consultation isn't reachable,
                        // which doesn't happen in practice): it still does
                        // NOT re-consult `PermissionPolicy` itself, nor
                        // record new grants from a subagent's own
                        // `AllowAndRemember` choice (that would require
                        // widening this closure's `bool`-only return type to
                        // carry the full `PermissionDecision`, which this
                        // ticket's acceptance criteria don't require -- see
                        // ticket 74's report for the full rationale).
                        let decision = permission
                            .request(rokr_tui::PermissionRequest {
                                tool_name: request.tool_name,
                                detail,
                            })
                            .await;
                        let granted = matches!(
                            decision,
                            rokr_tui::PermissionDecision::Allow
                                | rokr_tui::PermissionDecision::AllowAndRemember
                        );
                        if granted {
                            capture_checkpoint_if_granted_diff(
                                diff_path_and_old,
                                &data_dir,
                                &session_handle,
                                &turn_index,
                            )
                            .await;
                        }
                        granted
                    })
                });
            // F-003/F-004: the SAME single provider snapshot the
            // send path below uses, resilience-wrapped -- no longer
            // a separately-tracked bare `AnyProvider`.
            let subagent_tool = subagent::SubagentTool::new(
                provider.clone(),
                config_dir.clone(),
                subagent_request_permission,
                session_grants.clone(),
            );

            // PC-1 ruling (supersedes ticket 46's whole-session
            // `OnceLock` freeze): the session's MCP tool-set
            // snapshot -- sorted deterministically by (server,
            // tool) via `rokr_mcp::snapshot_tools` -- is now
            // recomputed fresh every Build-tier `submit`, but each
            // individual server's contribution within it is
            // ALREADY frozen (`McpServerHandle::joined`, written at
            // most once per server: that server's own first
            // `Ready`, or again on a later explicit `/mcp
            // reconnect` success -- never on an automatic
            // Ready-after-Fail flap). So a server that's still
            // `Starting` on turn 1 and reaches `Ready` before turn
            // 2 DOES appear starting turn 2 (this is the intended,
            // one-time auto-join, not "mutation"); a server that
            // was already joined on turn 1 contributes the EXACT
            // SAME tools on turn 2 even if its live tool list
            // somehow changed, since this reads each server's
            // frozen `joined` snapshot, never its live one.
            // Declared here, as owned `Arc<McpTool>`s, BEFORE
            // `tools` below so the `&dyn ExecutableTool` references
            // pushed into `tools` borrow from something that
            // outlives the `run_tool_loop` call.
            let mcp_tools_snapshot: Vec<Arc<rokr_mcp::McpTool>> =
                if matches!(agent, AgentTier::Build) {
                    rokr_mcp::snapshot_tools(&mcp_server_handles, &mcp_notice_tx)
                } else {
                    Vec::new()
                };

            let mut tools: Vec<&dyn rokr_core::ExecutableTool> = match agent {
                AgentTier::Plan => vec![&read, &glob, &grep, &ls],
                AgentTier::Build => {
                    vec![
                        &read, &glob, &grep, &ls, &bash, &write, &edit, &webfetch,
                        &subagent_tool,
                    ]
                }
            };
            if let (AgentTier::Build, Some(websearch)) = (agent, &websearch) {
                tools.push(websearch);
            }
            // MCP tools are gated (`McpTool::preview` always returns
            // `Some(...)`), so -- like bash/write/edit/webfetch
            // above -- they only join the tool set for the `Build`
            // tier, never `Plan` (ticket 44's original gating
            // behavior, preserved here).
            for tool in &mcp_tools_snapshot {
                tools.push(tool.as_ref() as &dyn rokr_core::ExecutableTool);
            }

            // Expand any `@path` mentions in the raw input BEFORE it
            // joins the transcript, so resolved file contents (or a
            // not-found note) land in the same user-role message
            // rather than as a separate synthetic message — at
            // least one supported provider rejects an orphan
            // tool-role message on the wire, so this deliberately
            // reuses the plain user-text path rather than a
            // ToolResult block. The resolver is real filesystem IO
            // (the only IO `rokr-core`'s pure `mentions` module
            // doesn't perform itself), matching `rokr-tools`'
            // `read` tool's own io-error-to-failure behavior: any
            // read error (missing file, permissions, non-UTF-8,
            // ...) is treated as `NotFound`.
            let mut expanded_input =
                rokr_core::mentions::expand_mentions(&input, |path| {
                    match std::fs::read_to_string(path) {
                        Ok(contents) => rokr_core::mentions::MentionResolution::Found(contents),
                        Err(_) => rokr_core::mentions::MentionResolution::NotFound,
                    }
                });

            // `UserPromptSubmit` (PRD "Hooks"; architect decision:
            // "UserPromptSubmit before each prompt is sent"): runs
            // BEFORE this turn's user message joins the transcript
            // at all, so a blocking deny (exit 2, unless the entry
            // opts out via `blocking: false`) can short-circuit the
            // whole submission with an early `Err` -- same "denied
            // before anything is recorded" shape `PreToolUse`'s
            // ordering rule gives tool calls, just one level up.
            // Every matching hook's exit-0 stdout is concatenated
            // and appended to `expanded_input`, injecting fresh
            // context into THIS turn's own user message (reusing
            // the plain user-text path, same reasoning as the
            // `@path`-mention expansion above: at least one
            // supported provider rejects an orphan tool-role
            // message on the wire).
            let mut injected_user_prompt_context = String::new();
            for entry in matching_hook_entries(&hooks_config, "UserPromptSubmit", None) {
                let payload = rokr_hooks::HookPayload::UserPromptSubmit {
                    prompt: expanded_input.clone(),
                };
                match run_hook_entry(entry, &payload).await {
                    rokr_hooks::HookResult::Success { stdout } => {
                        if !stdout.trim().is_empty() {
                            if !injected_user_prompt_context.is_empty() {
                                injected_user_prompt_context.push_str("\n\n");
                            }
                            injected_user_prompt_context.push_str(stdout.trim());
                        }
                    }
                    rokr_hooks::HookResult::Blocked { stderr } => {
                        if entry.blocking.unwrap_or(true) {
                            return Err(stderr);
                        }
                        eprintln!(
                            "UserPromptSubmit hook exited 2 but its config entry sets \
                             blocking: false, allowing the prompt through: {stderr}"
                        );
                    }
                    rokr_hooks::HookResult::NonBlockingFailure { message } => {
                        eprintln!(
                            "UserPromptSubmit hook failed non-blocking, continuing \
                             without its injected context: {message}"
                        );
                    }
                }
            }
            if !injected_user_prompt_context.is_empty() {
                expanded_input =
                    format!("{expanded_input}\n\n{injected_user_prompt_context}");
            }

            let mut transcript = transcript.lock().await;
            // Schema v2 (architect ruling, phase-5): capture the
            // transcript length BEFORE this turn's user message and
            // its whole exchange are appended, so the `Turn` record
            // below can persist EXACTLY the slice this submit
            // produced -- the user prompt plus every
            // assistant/tool-use/tool-result/final message
            // `run_tool_loop` appends in place.
            let start = transcript.len();
            accumulate_user_turn(&mut transcript, expanded_input);

            let repo_map_snapshot: Option<String> = repo_map.lock().unwrap().clone();

            // `PreToolUse` (ticket 49, hooks-tracer-bullet; replaced
            // here by ticket 50, hooks-remaining-events-and-config,
            // with the real `hooks` config schema -- the interim
            // `ROKR_PRETOOLUSE_HOOK` env var this superseded is
            // gone, mirroring how ticket 45's `mcp` config schema
            // superseded ticket 44's `ROKR_MCP_SERVER` env var):
            // runs every configured `PreToolUse` hook whose
            // `matcher` glob matches the tool name about to be
            // called (`matching_hook_entries`), in order, stopping
            // at the first that denies. A hook that exits 2 vetoes
            // UNLESS its own config entry sets `blocking: false`,
            // in which case the veto is downgraded to a logged
            // non-blocking notice -- see `HookEntry::blocking`'s doc
            // comment in `rokr-config` for why that escape hatch
            // exists. Any other outcome (success, non-blocking
            // failure, or a downgraded block) falls through to
            // `Allow`, matching `execute_hook`'s own
            // non-blocking-failure contract
            // (`docs/adr/0012-hooks-execution-trust-model.md`).
            let pre_tool_hook: &rokr_core::PreToolHookCallback<'_> =
                &|request: rokr_core::PreToolHookRequest| {
                    let hooks_config = hooks_config.clone();
                    Box::pin(async move {
                        let entries = matching_hook_entries(
                            &hooks_config,
                            "PreToolUse",
                            Some(&request.tool_name),
                        );
                        for entry in entries {
                            let payload = rokr_hooks::HookPayload::PreToolUse {
                                tool_name: request.tool_name.clone(),
                                tool_input: request.tool_input.clone(),
                            };
                            match run_hook_entry(entry, &payload).await {
                                rokr_hooks::HookResult::Success { .. } => {}
                                rokr_hooks::HookResult::Blocked { stderr } => {
                                    if entry.blocking.unwrap_or(true) {
                                        return rokr_core::PreToolHookOutcome::Deny(
                                            stderr,
                                        );
                                    }
                                    eprintln!(
                                        "PreToolUse hook exited 2 but its config entry \
                                         sets blocking: false, allowing the tool call \
                                         through: {stderr}"
                                    );
                                }
                                rokr_hooks::HookResult::NonBlockingFailure { message } => {
                                    eprintln!(
                                        "PreToolUse hook failed non-blocking, allowing \
                                         the tool call through: {message}"
                                    );
                                }
                            }
                        }
                        rokr_core::PreToolHookOutcome::Allow
                    })
                };

            // `PostToolUse` (ticket 50, hooks-remaining-events-and-config;
            // PRD "Hooks", architect decision: "mirrors PreToolUse's
            // callback shape, non-blocking observational"): runs
            // every configured `PostToolUse` hook whose `matcher`
            // matches the tool that just ran, AFTER its result is
            // already decided. `PostToolHookCallback` returns `()`
            // (see that type's doc comment) -- nothing this closure
            // does can change the `ToolResult` already produced;
            // every outcome (including a stray exit 2) is just
            // logged via `log_observational_hook_outcome`.
            let post_tool_hook: &rokr_core::PostToolHookCallback<'_> =
                &|request: rokr_core::PostToolHookRequest| {
                    let hooks_config = hooks_config.clone();
                    Box::pin(async move {
                        let entries = matching_hook_entries(
                            &hooks_config,
                            "PostToolUse",
                            Some(&request.tool_name),
                        );
                        for entry in entries {
                            let payload = rokr_hooks::HookPayload::PostToolUse {
                                tool_name: request.tool_name.clone(),
                                tool_input: request.tool_input.clone(),
                                tool_output: request.tool_output.clone(),
                                is_error: request.is_error,
                            };
                            let result = run_hook_entry(entry, &payload).await;
                            log_observational_hook_outcome("PostToolUse", &result);
                        }
                    })
                };

            let (reply, turn_usage) = rokr_core::run_tool_loop(
                &provider,
                &system_prompt,
                repo_map_snapshot.as_deref(),
                &mut transcript,
                &tools,
                request_permission,
                Some(pre_tool_hook),
                Some(post_tool_hook),
                max_iterations,
            )
            .await
            .map_err(|err| err.to_string())?;
            // F-001: `usage` (the pre-existing local name every use below
            // reads) is the FINAL call's figure -- compaction/context-percent
            // math must keep seeing only that, never the cumulative total
            // (see `TurnUsage`'s doc comment). The full `turn_usage` (both
            // fields) is separately stashed into `last_turn_usage` below for
            // `crate::headless::run_result_object` to report/cost the turn's
            // TOTAL spend.
            let usage = turn_usage.final_call;
            *last_turn_usage.lock().unwrap() = Some(turn_usage);

            // `Stop` (ticket 50, hooks-remaining-events-and-config;
            // PRD "Hooks", architect decision: "Stop when agent
            // finishes a turn"): fires here, once `run_tool_loop`
            // has produced this turn's final reply, fire-and-observe
            // like `PostToolUse` above (no veto semantics -- the
            // turn has already finished).
            for entry in matching_hook_entries(&hooks_config, "Stop", None) {
                let result = run_hook_entry(entry, &rokr_hooks::HookPayload::Stop).await;
                log_observational_hook_outcome("Stop", &result);
            }

            // Schema v2 (architect ruling, phase-5): exactly ONE
            // `Turn` record per submit, appended after
            // `run_tool_loop` returns and BEFORE the auto-compaction
            // check below, carrying the FULL exchange
            // (`transcript[start..]`) -- the @path-mention-EXPANDED
            // user message (what actually went out on the wire) plus
            // every assistant/tool-use/tool-result/final message the
            // loop appended. Atomic: the whole exchange or nothing (a
            // crash mid-loop drops the in-flight turn), intentionally
            // not split into an early user append and a later
            // assistant append. Note this supersedes ticket 34's
            // earlier behavior of persisting the raw pre-expansion
            // `input`.
            {
                let session_handle_guard = session_handle.read().await;
                if let Some(session_handle) = session_handle_guard.as_ref() {
                    // F-001: persists the turn's CUMULATIVE usage (every
                    // `Provider::send` round trip this submission made, not
                    // just the final one) -- `/cost`'s
                    // `fold_session_usage_and_model` (crates/rokr/src/main.rs)
                    // sums every `Turn` record's `UsageRecord` across a
                    // session, so persisting `final_call` here would silently
                    // drop every intermediate tool round-trip's tokens from
                    // that total.
                    session_handle.append_turn(
                        transcript[start..].to_vec(),
                        rokr_session::UsageRecord::from(turn_usage.cumulative),
                        now_timestamp(),
                    );
                }
            }

            // Ticket 38 (checkpoint-pre-images): incremented AFTER
            // this turn's own `Turn` record has been appended above
            // (or would have been, if persistence is degraded) --
            // every gated tool call's snapshot taken during THIS
            // turn's `run_tool_loop` call above used the
            // pre-increment value, which is exactly the index this
            // turn's own `Turn` record occupies once appended (see
            // `turn_index`'s own doc comment, and `fold`'s
            // `next_turn_index` semantics in rokr-session).
            *turn_index.lock().unwrap() += 1;

            // Auto-compaction (ticket 20): checked once per
            // submitted turn using that turn's own final usage
            // figure. Runs inside this same async submit future —
            // no new thread, nothing here blocks the render loop.
            // On failure the transcript is left untouched and a
            // notice is prepended to this turn's reply instead of
            // losing history.
            let (prior_usage, effective_usage_for_percent) = {
                let mut guard = last_known_usage.lock().unwrap();
                let prior = *guard;
                let effective = if usage.input_tokens != 0 || usage.output_tokens != 0 {
                    *guard = Some(usage);
                    usage
                } else {
                    // F-011 (argus review): some OpenAI-compatible
                    // proxies intermittently omit real usage for a
                    // turn (an all-zero figure) -- treating that as
                    // "this turn used zero tokens" would visibly
                    // drop the status line's percentage to 0%. Fall
                    // back to whatever was last known (or the same
                    // all-zero `usage` if nothing real has ever
                    // been reported this session, matching today's
                    // very first-turn behavior).
                    guard.unwrap_or(usage)
                };
                (prior, effective)
            };

            // Ticket 43 (mouse-scroll-status-line): sent after every
            // turn's usage is known, regardless of whether it was
            // just folded into `last_known_usage` above (even an
            // all-zero usage figure is still worth reflecting in the
            // status line rather than leaving the previous turn's
            // percentage stale). `context_window_size` is the same
            // `u32` already captured by this closure for
            // `should_compact` below. F-011: uses
            // `effective_usage_for_percent` (falls back to the last
            // known real usage on an all-zero report) rather than
            // the raw `usage` -- scoped ONLY to this status-line
            // percentage; `should_compact` below still sees the raw
            // `usage`/`prior_usage` unchanged.
            let context_percent = (effective_usage_for_percent.input_tokens
                + effective_usage_for_percent.output_tokens)
                as f64
                / context_window_size as f64;
            let _ = status_tx.send(rokr_tui::SessionStatus {
                context_percent,
                notice: None,
            });

            let notice = if rokr_core::should_compact(
                usage,
                prior_usage,
                &transcript,
                context_window_size,
                auto_compact_threshold,
            ) {
                match rokr_core::compact_transcript(&provider, &transcript).await {
                    Ok(rokr_core::CompactionOutcome::Compacted(compacted)) => {
                        // RULING 2: persist a Compaction record. At
                        // this point `turn_index` has already been
                        // incremented for THIS turn (see above), so
                        // its value is the raw turn count; the retained
                        // tail turn is `raw_turn_count - 1` and the
                        // summary replaces through `raw_turn_count - 2`.
                        let raw_turn_count = *turn_index.lock().unwrap();
                        append_compaction_record(
                            &session_handle,
                            &compacted,
                            raw_turn_count,
                        )
                        .await;
                        *transcript = compacted;
                        None
                    }
                    Ok(rokr_core::CompactionOutcome::NothingToCompact) => None,
                    Err(err) => Some(format!(
                        "[auto-compaction failed, continuing with full history: {err}]"
                    )),
                }
            } else {
                None
            };

            Ok(match notice {
                Some(notice) => format!("{notice}\n{}", reply.text()),
                None => reply.text(),
            })
        }
    }
}

/// Runs `entry`'s command against `payload`, honoring its `timeout_ms`
/// override (falling back to `rokr_hooks::DEFAULT_TIMEOUT` when absent).
/// Ticket 50 (hooks-remaining-events-and-config): shared by every hook call
/// site (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `SessionStart`,
/// `Stop`, `SessionEnd`) -- the runner's turn-scoped events here and the
/// startup/shutdown events in `main.rs` -- so the timeout-override lookup
/// lives in exactly one place.
pub async fn run_hook_entry(
    entry: &rokr_config::HookEntry,
    payload: &rokr_hooks::HookPayload,
) -> rokr_hooks::HookResult {
    let timeout = entry
        .timeout_ms
        .map(std::time::Duration::from_millis)
        .unwrap_or(rokr_hooks::DEFAULT_TIMEOUT);
    rokr_hooks::execute_hook(&entry.command, payload, timeout).await
}

/// Every hook entry configured for `event`, matcher-filtered against
/// `tool_name` when `Some` (`PreToolUse`/`PostToolUse`) or left unfiltered
/// when `None` -- every lifecycle event (`SessionStart`, `UserPromptSubmit`,
/// `Stop`, `SessionEnd`) ignores `matcher` entirely, per the PRD's "Matcher
/// shape" note, simply by always being called with `tool_name: None` at its
/// call sites. An entry with no `matcher` set behaves like `"*"`
/// (matches every tool), same as a missing `matcher` string would via
/// `rokr_hooks::matches_tool_name`.
pub fn matching_hook_entries<'a>(
    hooks_config: &'a std::collections::HashMap<String, Vec<rokr_config::HookEntry>>,
    event: &str,
    tool_name: Option<&str>,
) -> Vec<&'a rokr_config::HookEntry> {
    hooks_config
        .get(event)
        .into_iter()
        .flatten()
        .filter(|entry| match tool_name {
            Some(tool_name) => entry
                .matcher
                .as_deref()
                .is_none_or(|matcher| rokr_hooks::matches_tool_name(matcher, tool_name)),
            None => true,
        })
        .collect()
}

/// Logs a one-line notice for a fire-and-observe hook's outcome
/// (`PostToolUse`, `Stop`, `SessionEnd` -- none of which can veto anything,
/// architect decision: "Stop/SessionEnd: fire-and-observe, exit codes
/// logged, non-blocking"), extended here to `PostToolUse` for the identical
/// reason (it always runs after the tool it's attached to has already
/// executed, so there's nothing left to veto). A `Success` outcome is the
/// silent default (nothing to report); `Blocked` (exit 2) is logged as an
/// IGNORED veto attempt, since these events have no veto to honor;
/// `NonBlockingFailure` is logged as-is.
pub fn log_observational_hook_outcome(event: &str, result: &rokr_hooks::HookResult) {
    match result {
        rokr_hooks::HookResult::Success { .. } => {}
        rokr_hooks::HookResult::Blocked { stderr } => {
            eprintln!(
                "{event} hook exited 2 (a blocking exit code), but {event} is fire-and-observe \
                 only and cannot veto anything -- ignoring: {stderr}"
            );
        }
        rokr_hooks::HookResult::NonBlockingFailure { message } => {
            eprintln!("{event} hook failed non-blocking: {message}");
        }
    }
}

/// Appends a new user-turn message onto the running conversation transcript.
/// `run_tool_loop` appends the corresponding assistant/tool-call/tool-result
/// messages as it executes; this is the seam where a fresh prompt joins that
/// running history.
pub fn accumulate_user_turn(transcript: &mut Vec<rokr_core::Message>, input: String) {
    transcript.push(rokr_core::Message::user_text(input));
}

/// Ticket 47 (mcp-permission-polish): formats a `PermissionPayload::ToolCall`
/// into the `rokr_tui::PermissionDetail::Text` shown in the permission
/// prompt -- one `label: value` line per field, server then tool then the
/// pretty-printed input. Kept as discrete lines (rather than one
/// interpolated blob) so ticket 48 (Streamable HTTP) can append an
/// `origin: ...` line for a remote server's permission prompt without
/// reshaping this format. Shared by both `request_permission` and
/// `subagent_request_permission`, which build identical prompt text
/// for a `ToolCall` payload.
///
/// Ticket 48 (mcp-http-transport), PRD "MCP permissions": `origin` is
/// `Some(url)` when `server` is an HTTP-transport MCP server (looked up by
/// the caller from config, NOT carried on `PermissionPayload::ToolCall`
/// itself -- see `mcp_http_origins`'s doc comment for why), `None`
/// for a stdio server. An HTTP server's origin is a data-exfiltration
/// signal, so it's appended as its own line when present.
pub fn format_tool_call_permission_text(
    server: &str,
    tool: &str,
    input_pretty: &str,
    origin: Option<&str>,
) -> String {
    let mut text = format!("server: {server}\ntool: {tool}\ninput: {input_pretty}");
    if let Some(origin) = origin {
        text.push_str(&format!("\norigin: {origin}"));
    }
    text
}

/// Ticket 38 (checkpoint-pre-images), PRD phase-5-session-management
/// decision 4: on a GRANTED write/edit permission decision, captures the
/// file's pre-image under `sessions/<id>/snapshots/` (reusing the `old`
/// content already computed for the permission-preview diff -- no new file
/// read) and appends a correlating `Checkpoint` record. Called from BOTH
/// `run_submission`'s own `request_permission` closure and the mirrored
/// `subagent_request_permission` closure, after `permission.request(...)`'s
/// decision comes back `true` -- a DENY must produce no snapshot and no
/// `Checkpoint` record, which this signature enforces structurally: callers
/// only invoke it once already inside their own `if granted` branch.
///
/// `diff_path_and_old` is `Some((path, old))` for a `PermissionPayload::Diff`
/// (write/edit) and `None` for `PermissionPayload::Command` (bash) --
/// bash-driven mutations are explicitly out of scope for checkpointing
/// (documented gap, not oversight), so this is a no-op for that case,
/// falling out of the match in the caller rather than a runtime
/// special-case here.
///
/// No-ops (logging a warning, never panicking) if no session is currently
/// active (persistence degraded at startup) -- checkpointing is best-effort
/// alongside session persistence, not a separate hard requirement.
///
/// First-write-wins de-duplication (ticket 38 scope-amendment, F-001 per
/// argus review): a turn's tool loop can mutate the SAME path more than
/// once (e.g. `write` then `edit`) -- `CheckpointStore::snapshot`'s
/// `newly_written` return only reports `true` for the first capture of a
/// given `(turn_index, path)` key, and a `Checkpoint` record is appended
/// ONLY when it's `true`. This avoids appending a second `Checkpoint`
/// record with a duplicate `snapshot_id` for a later mutation whose `old`
/// is already post-first-mutation content, not the real turn-start
/// pre-image.
///
/// Known limitation (see ticket 38's `## Scope Amendment`):
/// `PermissionPayload::Diff`'s `old: String` field collapses "file did not
/// exist" and "file existed but was empty" into the same empty string, with
/// no separate boolean carried through (the architect's ruling fixed
/// `Preview::Diff`/`PermissionPayload::Diff`'s shape to exactly `{ path,
/// old, new }`, and adding a second field for this was judged out of scope
/// for that ticket). So an empty `old` is treated here as "absent"
/// (`None`), which is lossy for the rare case of a genuinely-empty
/// pre-existing file -- `CheckpointStore::snapshot` itself DOES support the
/// real distinction (`Option<&str>`), this call site just cannot supply it
/// today.
pub async fn capture_checkpoint_if_granted_diff(
    diff_path_and_old: Option<(String, String)>,
    data_dir: &std::path::Path,
    session_handle: &tokio::sync::RwLock<Option<Arc<rokr_session::SessionHandle>>>,
    turn_index: &std::sync::Mutex<usize>,
) {
    let Some((path, old)) = diff_path_and_old else {
        return;
    };

    let session_handle_guard = session_handle.read().await;
    let Some(session_handle) = session_handle_guard.as_ref() else {
        // F-002 (argus review): matches this fn's own doc comment ("no-ops,
        // logging a warning") -- persistence being degraded at startup is
        // the same class of condition `create_session`'s own failure path
        // (near the top of `main`) already logs via `eprintln!` rather than
        // silently swallowing.
        eprintln!(
            "skipping pre-image checkpoint snapshot for {path}: no session is currently active"
        );
        return;
    };

    let current_turn_index = *turn_index.lock().unwrap();
    let checkpoint_store = rokr_session::CheckpointStore::open(data_dir, session_handle.session_id());
    let old_content: Option<&str> = if old.is_empty() { None } else { Some(old.as_str()) };

    match checkpoint_store.snapshot(current_turn_index, &path, old_content) {
        Ok((snapshot_id, newly_written)) => {
            if newly_written {
                session_handle.append_checkpoint(current_turn_index, snapshot_id);
            }
        }
        Err(err) => {
            eprintln!("failed to capture pre-image checkpoint snapshot for {path}: {err}");
        }
    }
}

/// The exact wrapper `rokr_core::compact_transcript` prepends to a fresh
/// summary (and `rokr_session::fold` re-applies on resume). RULING 2 strips
/// it back off before persisting a `Compaction` record's `summary` so the
/// stored text is RAW -- storing the already-wrapped text would double-wrap
/// it on the next resume.
pub const COMPACTION_SUMMARY_WRAPPER_PREFIX: &str =
    "[Earlier conversation summary — compacted to save context]\n\n";

/// RULING 2 (architect ruling, phase-5): appends a `Compaction` record for a
/// just-completed compaction, shared by BOTH the auto-compaction branch in
/// `run_submission` and the manual `/compact` handler in `main.rs`'s
/// `command` closure.
///
/// `compacted` is `compact_transcript`'s output: `compacted[0]` is the
/// summary message with the wrapper prefix already baked in, and
/// `compacted[1..]` is the untouched tail turn. This strips the wrapper back
/// off `compacted[0]`'s text (falling back to the raw text if the prefix
/// somehow isn't present) to recover the RAW summary to store, because
/// `fold` re-applies that same wrapper on resume.
///
/// `raw_turn_count` is `*turn_index` at the compaction decision point (AFTER
/// the per-turn increment in `run_submission`; the plain current value in
/// `/compact`, which never increments). Compaction always retains exactly the
/// tail turn (raw index `raw_turn_count - 1`), so the summary replaces
/// through `(raw_turn_count - 1) - 1 == raw_turn_count - 2`. If that
/// underflows (fewer than 2 raw turns -- `compact_transcript` should never
/// return `Compacted` then, but guard defensively), NO record is appended.
///
/// No-ops if no session is currently active (persistence degraded at
/// startup), matching `capture_checkpoint_if_granted_diff`'s handling.
pub async fn append_compaction_record(
    session_handle: &tokio::sync::RwLock<Option<Arc<rokr_session::SessionHandle>>>,
    compacted: &[rokr_core::Message],
    raw_turn_count: usize,
) {
    let Some(replaced_through) = raw_turn_count.checked_sub(2) else {
        return;
    };

    let wrapped = compacted.first().map(|m| m.text()).unwrap_or_default();
    let raw_summary = wrapped
        .strip_prefix(COMPACTION_SUMMARY_WRAPPER_PREFIX)
        .unwrap_or(&wrapped)
        .to_string();

    let guard = session_handle.read().await;
    if let Some(handle) = guard.as_ref() {
        handle.append_compaction(raw_summary, replaced_through);
    }
}

/// Resolves the central data directory for session persistence:
/// `$XDG_DATA_HOME/rokr` if `XDG_DATA_HOME` is set and non-empty,
/// otherwise `$HOME/.local/share/rokr`. Mirrors
/// `rokr_config::default_config_dir`'s exact resolution pattern (ticket 34:
/// persist-new-sessions -- PRD decision "Central storage, not per-project":
/// all session data lives under `$XDG_DATA_HOME/rokr/`, not inside the
/// project being worked on).
///
/// Ticket 55 (headless-output-formats-and-permission-mode): moved here from
/// a private fn of the same name in `crates/rokr/src/main.rs` so headless's
/// new orchestration (`crate::headless::run`) can build a
/// `rokr_session::SessionStore`/`CheckpointStore` the same way the TUI path
/// does, without a second private copy -- `main.rs` now imports this one
/// instead of defining its own.
pub fn default_data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".local/share"));
    base.join("rokr")
}

/// Returns the current time as a plain Unix-epoch-seconds string. There is
/// no date/time-formatting crate in this workspace today (see
/// `rokr-provider::auth`'s own `expires_at` field, which is a plain `u64`
/// seconds-since-epoch for the same reason) — `SessionRecord`'s
/// `created_at`/`timestamp` fields are typed `String` with no enforced
/// format, so epoch seconds serialized as a string satisfies that type
/// without pulling in a new dependency.
pub fn now_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A test permission surface that grants everything without a live TUI.
    /// (`rokr_tui::PermissionHandle` has no public constructor, which is the
    /// whole reason `run_submission` is generic over [`PermissionRequester`]
    /// rather than hardcoding it.)
    #[derive(Clone)]
    struct AlwaysGrant;

    impl PermissionRequester for AlwaysGrant {
        fn request(
            &self,
            _request: rokr_tui::PermissionRequest,
        ) -> impl std::future::Future<Output = rokr_tui::PermissionDecision> + Send {
            async { rokr_tui::PermissionDecision::Allow }
        }
    }

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rokr-runner-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `SessionRunner` must reproduce the pre-extraction `submit` closure's
    /// orchestration: a single Plan-tier submission, driven against a mocked
    /// provider, assembles the tool set, runs `rokr_core::run_tool_loop`,
    /// and returns the provider's assistant text as the terminal `Ok`
    /// value. No gated tools fire for a plain Plan-tier reply, so the
    /// grant-everything permission surface is never actually consulted --
    /// exactly matching the old closure's behavior for this path.
    #[tokio::test]
    async fn session_runner_drives_one_submission_to_terminal_state_matching_prior_closure_behavior(
    ) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const MARKER: &str = "RunnerDrivenReply7788";

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": MARKER},
                        "finish_reason": "stop"
                    }
                ]
            })))
            .mount(&mock)
            .await;

        let any = rokr_provider::AnyProvider::OpenAi(rokr_provider::OpenAiProvider::new(
            mock.uri(),
            "gpt-4o-mini",
            "test-key",
        ));
        let policy = rokr_provider::RetryPolicy {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            max_elapsed: std::time::Duration::from_secs(5),
        };
        let resilient = rokr_provider::ResilientProvider::with_policy(any, policy);
        let provider: Result<SharedProvider, String> =
            Ok(Arc::new(tokio::sync::RwLock::new(resilient)));

        let (status_tx, _status_rx) = std::sync::mpsc::channel::<rokr_tui::SessionStatus>();
        let (mcp_notice_tx, _mcp_notice_rx) = std::sync::mpsc::channel::<String>();

        let config_dir = unique_temp_dir("runner-config");
        let data_dir = unique_temp_dir("runner-data");

        let runner = SessionRunner {
            provider,
            transcript: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            system_prompt: "you are a test agent".to_string(),
            repo_map: Arc::new(Mutex::new(None)),
            last_known_usage: Arc::new(Mutex::new(None)),
            last_turn_usage: Arc::new(Mutex::new(None)),
            config_dir: config_dir.clone(),
            session_handle: Arc::new(tokio::sync::RwLock::new(None)),
            turn_index: Arc::new(Mutex::new(0)),
            data_dir: data_dir.clone(),
            status_tx,
            mcp_server_handles: Arc::new(Vec::new()),
            mcp_notice_tx,
            mcp_http_origins: Arc::new(HashMap::new()),
            hooks_config: Arc::new(HashMap::new()),
            agent: AgentTier::Plan,
            context_window_size: 200_000,
            auto_compact_threshold: 0.7,
            max_iterations: None,
            session_grants: Arc::new(Mutex::new(crate::permission_policy::SessionGrants::new())),
        };

        let reply = runner
            .run_submission("hello runner".to_string(), AlwaysGrant)
            .await
            .expect(
                "a Plan-tier submission with a mocked provider reply should drive to a \
                 terminal Ok state",
            );

        assert!(
            reply.contains(MARKER),
            "expected the runner-driven submission to return the provider's assistant text \
             (proving SessionRunner reproduces the moved closure's run_tool_loop path), got: \
             {reply}"
        );

        // `turn_index` was incremented exactly once for this single turn --
        // the same post-`append_turn` increment the old closure did.
        assert_eq!(*runner.turn_index.lock().unwrap(), 1);

        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// Ticket 72 (`tui-session-allowlist-grant`): a permission surface
    /// (NOT `AlwaysGrant`) that always decides `AllowAndRemember`, while
    /// counting how many times its `request` was actually invoked -- the
    /// count is the behavioral proof that a SECOND gated call to the same
    /// tool, in the same session, never reaches this stub at all (it must
    /// be short-circuited by `PermissionPolicy::resolve` reading the grant
    /// recorded from the FIRST call).
    #[derive(Clone)]
    struct RememberOnceRequester {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PermissionRequester for RememberOnceRequester {
        fn request(
            &self,
            _request: rokr_tui::PermissionRequest,
        ) -> impl std::future::Future<Output = rokr_tui::PermissionDecision> + Send {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { rokr_tui::PermissionDecision::AllowAndRemember }
        }
    }

    /// Proves `run_submission`'s wiring of `PermissionPolicy::resolve` +
    /// `SessionGrants` end to end (ticket 72): a first gated `bash` call,
    /// decided `AllowAndRemember`, records a grant on `runner.session_grants`;
    /// a SECOND gated `bash` call in a LATER submission on the SAME runner
    /// must be auto-allowed via that grant WITHOUT the permission surface's
    /// `request` ever being invoked again (the call counter stays at 1),
    /// while the second bash command's real side effect (a second marker
    /// file) still proves the tool loop actually ran it rather than
    /// silently skipping it.
    #[tokio::test]
    async fn remembered_grant_is_recorded_after_user_chooses_remember_for_session() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Ticket 69 (bash-command-sandbox-confinement): `run_submission`
        // confines `bash` to `std::env::current_dir()` via a Seatbelt
        // sandbox profile on macOS -- a marker file outside that directory
        // would be silently blocked. So, unlike the other `unique_temp_dir`
        // calls below (config_dir/data_dir, never touched by the sandboxed
        // subprocess), the bash-writable target dir must live UNDER the
        // test process's actual current directory.
        let target_dir = std::env::current_dir()
            .unwrap()
            .join(format!(
                "rokr-runner-remember-target-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&target_dir).unwrap();
        let marker_one = target_dir.join("remember-marker-one");
        let marker_two = target_dir.join("remember-marker-two");
        let command_one = format!("touch {}", marker_one.to_string_lossy());
        let command_two = format!("touch {}", marker_two.to_string_lossy());

        let first_reply_text = "FirstReplyAfterRememberedBash";
        let second_reply_text = "SecondReplyAfterAutoAllowedBash";

        // 1st request (first submission): the model asks to run `bash`.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-remember-bash-1",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": serde_json::json!({ "command": command_one }).to_string()
                                    }
                                }
                            ]
                        },
                        "finish_reason": "tool_calls"
                    }
                ]
            })))
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // 2nd request (still first submission): the tool result feeds back,
        // and the model gives a tool-call-free final reply.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-remember-bash-1-final",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": { "role": "assistant", "content": first_reply_text },
                        "finish_reason": "stop"
                    }
                ]
            })))
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // 3rd request (second submission): another `bash` call, a
        // DIFFERENT marker file -- this one must be auto-allowed via the
        // grant recorded during the first submission.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-remember-bash-2",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "id": "call_2",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": serde_json::json!({ "command": command_two }).to_string()
                                    }
                                }
                            ]
                        },
                        "finish_reason": "tool_calls"
                    }
                ]
            })))
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // 4th+ request(s) (still second submission): final reply.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-remember-bash-2-final",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": { "role": "assistant", "content": second_reply_text },
                        "finish_reason": "stop"
                    }
                ]
            })))
            .mount(&mock)
            .await;

        let any = rokr_provider::AnyProvider::OpenAi(rokr_provider::OpenAiProvider::new(
            mock.uri(),
            "gpt-4o-mini",
            "test-key",
        ));
        let policy = rokr_provider::RetryPolicy {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            max_elapsed: std::time::Duration::from_secs(5),
        };
        let resilient = rokr_provider::ResilientProvider::with_policy(any, policy);
        let provider: Result<SharedProvider, String> =
            Ok(Arc::new(tokio::sync::RwLock::new(resilient)));

        let (status_tx, _status_rx) = std::sync::mpsc::channel::<rokr_tui::SessionStatus>();
        let (mcp_notice_tx, _mcp_notice_rx) = std::sync::mpsc::channel::<String>();

        let config_dir = unique_temp_dir("runner-remember-config");
        let data_dir = unique_temp_dir("runner-remember-data");

        let runner = SessionRunner {
            provider,
            transcript: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            system_prompt: "you are a test agent".to_string(),
            repo_map: Arc::new(Mutex::new(None)),
            last_known_usage: Arc::new(Mutex::new(None)),
            last_turn_usage: Arc::new(Mutex::new(None)),
            config_dir: config_dir.clone(),
            session_handle: Arc::new(tokio::sync::RwLock::new(None)),
            turn_index: Arc::new(Mutex::new(0)),
            data_dir: data_dir.clone(),
            status_tx,
            mcp_server_handles: Arc::new(Vec::new()),
            mcp_notice_tx,
            mcp_http_origins: Arc::new(HashMap::new()),
            hooks_config: Arc::new(HashMap::new()),
            // Build tier: `bash` must be in the tool set for this test's
            // gated tool call to happen at all (ticket 72's design doc:
            // "Build-tier agent so bash is in the tool set").
            agent: AgentTier::Build,
            context_window_size: 200_000,
            auto_compact_threshold: 0.7,
            max_iterations: None,
            session_grants: Arc::new(Mutex::new(crate::permission_policy::SessionGrants::new())),
        };

        let requester = RememberOnceRequester {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        let first_reply = runner
            .run_submission("please run the first bash command".to_string(), requester.clone())
            .await
            .expect("the first submission should drive to a terminal Ok state");

        assert!(
            first_reply.contains(first_reply_text),
            "expected the first submission's reply to contain the provider's final text, got: \
             {first_reply}"
        );
        assert!(
            marker_one.exists(),
            "expected the first bash command to have run (marker file created)"
        );
        assert_eq!(
            requester.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the permission surface should have been consulted exactly once for the first, \
             ungranted bash call"
        );

        let second_reply = runner
            .run_submission("please run the second bash command".to_string(), requester.clone())
            .await
            .expect("the second submission should drive to a terminal Ok state");

        assert!(
            second_reply.contains(second_reply_text),
            "expected the second submission's reply to contain the provider's final text, got: \
             {second_reply}"
        );
        assert!(
            marker_two.exists(),
            "expected the second bash command to have actually run (marker file created), \
             proving the tool loop executed it rather than silently skipping it"
        );
        assert_eq!(
            requester.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the permission surface's request() must NOT have been invoked again for the \
             second, already-granted bash call -- PermissionPolicy::resolve should have \
             short-circuited to Allow using the grant recorded from the first call"
        );

        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
    }
}
