//! The agent loop, message and content-block model, context compaction.

use std::future::Future;
use std::pin::Pin;

pub mod context;
pub mod mentions;
pub mod message;

pub use message::{CacheControl, CacheControlKind, ContentBlock, Message, Role};

/// Re-exported so `rokr-mcp` (ticket 44, mcp-tracer-bullet), which per
/// `docs/adr/0011-rokr-mcp-crate-boundary.md` depends on `rokr-core` only
/// (never `rokr-tools` directly), can name the error type
/// `ExecutableTool::execute_boxed` requires without its own `rokr-tools`
/// dependency. `rokr-core` already depends on `rokr-tools` for the
/// built-in `impl_executable_tool!`/`impl_executable_tool_gated!` macros
/// above, so this adds no new dependency edge -- just a narrow re-export
/// of a type that already flows through `ExecutableTool`'s public API.
pub use rokr_tools::ToolError;

/// A tool a provider may call, described in rokr-core-native terms. The
/// minimal shape a `Provider` needs to advertise tools on the wire: a name,
/// a human-readable description, and a JSON Schema for its input. Built from
/// a `rokr_tools::Tool` by [`ExecutableTool::to_tool_spec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Optional trailing cache-breakpoint marker (Phase 3). When set on the
    /// last `ToolSpec` in a request's tools list, this places a cache
    /// breakpoint after the whole tools segment, the same way a
    /// `ContentBlock::Text`'s `cache_control` places one after that block.
    /// Populated during context assembly (`context::assemble` sets `Extended`
    /// on the last spec); `None` as constructed by `to_tool_spec`.
    pub cache_control: Option<CacheControl>,
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
    /// `path` (ticket 38, checkpoint-pre-images) mirrors
    /// `rokr_tools::Preview::Diff`'s `path` field one-for-one — see that
    /// type's doc comment for why it's needed here: a pre-image snapshot
    /// keyed by `(turn_index, path)` is taken on the permission-decision
    /// path in `crates/rokr/src/main.rs`, which only sees this payload, not
    /// the raw tool-call JSON.
    Diff { path: String, old: String, new: String },
    /// An MCP tool call (ticket 47, mcp-permission-polish), replacing
    /// ticket 44's interim `Command(String)` encoding for MCP calls: the
    /// server name, tool name, and pretty-printed input JSON are carried
    /// as separate fields rather than pre-flattened into one opaque
    /// string, so `crates/rokr/src/main.rs`'s permission bridge can format
    /// them explicitly (and, per the PRD's "MCP permissions" section, a
    /// later ticket can add an `origin` line for a remote HTTP server
    /// without re-encoding this payload). Deliberately not
    /// `#[non_exhaustive]` -- this codebase's house style is
    /// compile-enforced match updates when a new variant lands.
    ToolCall {
        server: String,
        tool: String,
        input_pretty: String,
    },
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

/// A tool-call attempt about to go through `run_tool_loop`'s permission
/// gate, handed to an optional `PreToolUse` hook callback (ticket 49,
/// hooks-tracer-bullet; PRD "Hooks") BEFORE that gate ever runs. Mirrors
/// [`PermissionRequest`]'s "primitives only" shape, but carries the raw
/// tool-input JSON rather than a preview-computed [`PermissionPayload`]:
/// a hook may want to veto a call to ANY tool the model named, gated or
/// not, not just ones that implement `PreviewableTool`.
#[derive(Debug, Clone, PartialEq)]
pub struct PreToolHookRequest {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
}

/// What a `PreToolUse` hook decided about a [`PreToolHookRequest`] (PRD
/// "Hooks", exit-code contract). `rokr-core` never depends on `rokr-hooks`
/// (architect decision) -- `rokr-hooks`' own richer exit-code/timeout
/// result type (`rokr_hooks::HookResult`) maps down to this two-variant
/// outcome at the one place both crates are known: `crates/rokr/src/main.rs`'s
/// wiring of a real hook callback into `run_tool_loop`.
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolHookOutcome {
    /// Exit 0, a non-blocking nonzero exit, or a timeout -- the call
    /// proceeds to the normal preview/permission/execute path unchanged.
    Allow,
    /// Exit 2 (blocking) -- the tool call is vetoed before the permission
    /// prompt ever runs. The `String` is the hook's stderr, used verbatim
    /// as the error `ToolResult` content: identical shape to an
    /// interactive permission rejection (same `ContentBlock::ToolResult`
    /// with `is_error: true`), just a different content string and a
    /// different (earlier) short-circuit point in the loop.
    Deny(String),
}

/// A caller-supplied `PreToolUse` hook check, injected into `run_tool_loop`
/// the same way `request_permission` is (a caller-supplied async closure),
/// but as a boxed `dyn Fn` behind an `Option<&_>` rather than a second
/// generic type parameter. Seam choice (architect decision left open,
/// "a small trait or a second optional closure"): `request_permission` is a
/// REQUIRED generic closure `F`, monomorphized once per call site, which
/// works because every call site always has a real closure to pass. This
/// callback is OPTIONAL -- most call sites (e.g. `crates/rokr/src/subagent.rs`'s
/// subagent loop, this ticket) pass `None` -- and a generic `Option<F>`
/// parameter would force even those call sites to spell out a concrete
/// "no-op" closure type by hand just to name `None::<F>`. A non-generic
/// trait-object type sidesteps that: `None` just works. The cost is one
/// boxed-future allocation per tool call when a hook IS configured, an
/// acceptable trade against `run_tool_loop` gaining a second monomorphized
/// generic parameter for a rarely-exercised path.
///
/// Ticket 50 is expected to add a second, identically-shaped
/// `PostToolHookCallback` parameter alongside this one (PRD "Core seam":
/// "`run_tool_loop` gains optional pre-tool and post-tool hook callback
/// parameters") -- this type's shape (a standalone type alias, not
/// entangled with `PreToolHookCallback`) is chosen so that lands as a pure
/// addition, not a reshape of this one.
pub type PreToolHookCallback<'a> = dyn Fn(PreToolHookRequest) -> Pin<Box<dyn Future<Output = PreToolHookOutcome> + Send + 'a>>
    + Send
    + Sync
    + 'a;

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
    /// Relaxed from `-> &'static str` to `-> &str` (ticket 44,
    /// mcp-tracer-bullet): every built-in tool's name is still a
    /// `&'static str` literal and coerces unchanged, but `rokr-mcp`'s
    /// `McpTool` builds its namespaced `mcp__<server>__<tool>` name at
    /// runtime from an owned `String`, which cannot satisfy `'static`. No
    /// other call site is affected -- `run_tool_loop`'s `tool.name() ==
    /// name.as_str()` lookup only ever compares by value.
    fn name(&self) -> &str;

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
                    cache_control: None,
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
// `websearch` is a plain (non-gated) tool like the four above — it performs
// no local side effect for the user to approve, it only delegates to the
// provider (see `rokr_tools::websearch`'s doc comment) — so it uses the
// plain-tool macro, not `impl_executable_tool_gated!`. `WebsearchTool` is a
// concrete, non-generic type (its `Arc<dyn NativeSearchCapability>` field
// doesn't make the type itself generic), so the macro's bare `$ty:ty`
// pattern applies unchanged.
impl_executable_tool!(rokr_tools::websearch::WebsearchTool);

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
                    cache_control: None,
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
                        rokr_tools::Preview::Diff { path, old, new } => {
                            PermissionPayload::Diff { path, old, new }
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
impl_executable_tool_gated!(rokr_tools::webfetch::WebfetchTool);

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
///
/// Returns the final reply paired with the provider-reported [`Usage`] of
/// the call that produced it (Phase 3), so a caller can decide whether that
/// turn's usage crosses the auto-compaction threshold (see
/// [`should_compact`]) before submitting the next one.
///
/// `pre_tool_hook` (ticket 49, hooks-tracer-bullet; PRD "Hooks", "Ordering
/// rule") is consulted for EVERY tool-call attempt, before the
/// preview/permission gate described above ever runs -- a
/// [`PreToolHookOutcome::Deny`] short-circuits the call entirely (the
/// permission machinery, interactive or otherwise, never runs for a call a
/// hook already vetoed) and produces an error `ToolResult` from the hook's
/// message, identical in shape to a rejected permission request.
/// [`PreToolHookOutcome::Allow`] (or `None`, meaning no hook is configured)
/// falls through to the unchanged preview/permission/execute path.
pub async fn run_tool_loop<P, F, Fut>(
    provider: &P,
    system_prompt: &str,
    repo_map: Option<&str>,
    transcript: &mut Vec<Message>,
    tools: &[&dyn ExecutableTool],
    request_permission: F,
    pre_tool_hook: Option<&PreToolHookCallback<'_>>,
) -> Result<(Message, Usage), P::Error>
where
    P: Provider,
    F: Fn(PermissionRequest) -> Fut,
    Fut: Future<Output = bool>,
{
    let tool_specs: Vec<ToolSpec> = tools.iter().map(|tool| tool.to_tool_spec()).collect();

    loop {
        // Re-assembled on every send (not just once before the loop): a
        // single user submission can trigger multiple tool round-trips, and
        // every one of those wire sends needs the breakpoint-marked static
        // prefix (tools + system segment), not just the first. `transcript`
        // itself stays system-prompt-free (pure conversation history) —
        // `assemble()` only prepends the system segment for this outgoing
        // call, it never mutates the caller's stored transcript.
        let assembled = context::assemble(context::ContextInputs {
            system_prompt: system_prompt.to_string(),
            tools: tool_specs.clone(),
            repo_map: repo_map.map(|repo_map| repo_map.to_string()),
            transcript: transcript.clone(),
        });

        let (reply, usage) = provider
            .send(&assembled.messages[..], &assembled.tools)
            .await?;

        let tool_uses: Vec<(String, String, serde_json::Value)> = reply
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id.clone(), name.clone(), input.clone())),
                ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
            })
            .collect();

        if tool_uses.is_empty() {
            transcript.push(reply.clone());
            return Ok((reply, usage));
        }

        transcript.push(reply);

        let mut result_blocks = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            // Ticket 49 (hooks-tracer-bullet), PRD "Hooks" ordering rule:
            // `PreToolUse` hooks run first, before the permission prompt --
            // for EVERY tool-call attempt, gated or not, since a hook may
            // want to veto a call to any tool the model named. A deny
            // short-circuits the whole match below (`request_permission`
            // and `execute_boxed` are never reached for a vetoed call).
            let hook_denial = match pre_tool_hook {
                Some(hook) => {
                    let outcome = hook(PreToolHookRequest {
                        tool_name: name.clone(),
                        tool_input: input.clone(),
                    })
                    .await;
                    match outcome {
                        PreToolHookOutcome::Deny(message) => Some(message),
                        PreToolHookOutcome::Allow => None,
                    }
                }
                None => None,
            };

            let (content, is_error) = if let Some(message) = hook_denial {
                (message, true)
            } else {
                match tools.iter().find(|tool| tool.name() == name.as_str()) {
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
                }
            };
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
                cache_control: None,
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

    /// Sends `messages`/`tools` to the provider and returns the assistant's
    /// reply paired with the parsed token [`Usage`] for this call (Phase 3).
    /// A provider whose wire response doesn't report a given usage figure
    /// (e.g. no cache-write concept at all) reports `0` for it rather than
    /// failing — callers should treat `0` as "not reported", not "definitely
    /// zero".
    async fn send(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<(Message, Usage), Self::Error>;

    /// Whether this provider exposes a native, server-side search capability
    /// (e.g. Anthropic's web-search server tool) that the `websearch` tool
    /// (`rokr_tools::websearch`) can delegate queries to. `main.rs` queries
    /// this to decide whether `websearch` belongs in a session's tool set:
    /// per the PRD, the tool is omitted entirely — not replaced with a
    /// degraded client-side implementation — when this reports `false`.
    /// Defaults to `false`, since most providers have no such capability.
    ///
    /// This is deliberately a thin `bool` signal rather than handing back an
    /// instance of `rokr_tools::websearch`'s local `NativeSearchCapability`
    /// trait: `rokr-tools` does not depend on `rokr-core` (a one-way edge —
    /// `rokr-core` depends on `rokr-tools`, never the reverse), so this trait
    /// structurally cannot name that type. Wiring a real adapter override
    /// (e.g. on the Anthropic provider) that also bridges an actual
    /// capability object is out of scope for this ticket (ticket 28:
    /// websearch-tool) — no adapter overrides this today.
    fn native_search_capable(&self) -> bool {
        false
    }
}

/// Provider-reported token accounting for a single [`Provider::send`] call
/// (Phase 3). Authoritative once available, replacing the rough
/// character-based estimate token accounting used before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// The system prompt for the dedicated summarization call [`compact_transcript`]
/// makes to shrink a long-running transcript. Distinct from the agent's own
/// system prompt (`main.rs`'s `system_prompt`) — this one instructs the
/// provider to *summarize*, not to keep acting as the coding agent, and asks
/// it to preserve exactly what a continuing agent still needs: the task and
/// decisions made so far, files touched, current state, open TODOs, and any
/// recent tool results still relevant.
const COMPACTION_PROMPT: &str = "\
You are compacting a long-running coding agent's conversation history to \
free up context window space. Summarize the conversation transcript below \
into a single, dense passage that preserves everything a continuing agent \
still needs:
- the current task and overall goal
- decisions already made, and why
- files touched or modified so far
- the present state of the work (what's done, what's in progress)
- open follow-ups or TODOs
- any recent tool results still needed for future steps

Do not restate these instructions or add pleasantries. Output only the \
summary text.";

/// Decides whether the transcript should be compacted before the next turn
/// is sent, per the Phase 3 auto-compaction design, using a three-tier
/// fallback for the estimated token count:
/// 1. the most recent turn's provider-reported `usage`, if it was reported
///    (`input_tokens != 0 || output_tokens != 0` — see [`Provider::send`]'s
///    doc comment on `0` meaning "not reported");
/// 2. otherwise the last real usage figure seen this session
///    (`last_known_usage`, F-003 fix): some OpenAI-compatible proxies
///    intermittently omit usage, and reusing the prior real figure keeps the
///    threshold from flapping based on nothing but raw transcript byte count;
/// 3. only if neither a current nor any prior real usage has ever arrived,
///    the rough chars/4 estimate of the whole transcript.
///
/// Cache read/write tokens are not counted here: they reflect tokens the
/// provider served from (or wrote to) its cache, not tokens occupying the
/// live context window budget this threshold is protecting.
pub fn should_compact(
    usage: Usage,
    last_known_usage: Option<Usage>,
    transcript: &[Message],
    context_window_size: u32,
    auto_compact_threshold: f64,
) -> bool {
    let budget = auto_compact_threshold * context_window_size as f64;
    let estimated_tokens = if usage.input_tokens != 0 || usage.output_tokens != 0 {
        (usage.input_tokens + usage.output_tokens) as f64
    } else if let Some(prior) =
        last_known_usage.filter(|u| u.input_tokens != 0 || u.output_tokens != 0)
    {
        (prior.input_tokens + prior.output_tokens) as f64
    } else {
        estimate_tokens_from_chars(transcript)
    };
    estimated_tokens >= budget
}

/// Rough pre-usage token estimate (chars/4) over every block's text content,
/// used by [`should_compact`] only until a real usage figure is available.
fn estimate_tokens_from_chars(transcript: &[Message]) -> f64 {
    let total_chars: usize = transcript
        .iter()
        .flat_map(|message| message.content.iter())
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.len(),
            ContentBlock::ToolUse { input, .. } => input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
        })
        .sum();
    total_chars as f64 / 4.0
}

/// A "genuine user prompt" turn: a `User`-role message carrying at least one
/// `Text` block. This excludes the `User`-role messages `run_tool_loop`
/// synthesizes to carry `ToolResult`s back to the provider (those have no
/// `Text` block), so [`tail_start_index`] can find the boundary of the most
/// recent real user turn rather than stopping at an intermediate tool-result
/// turn partway through it.
fn is_user_prompt_message(message: &Message) -> bool {
    message.role == Role::User
        && message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { .. }))
}

/// Finds where the transcript's "tail" (the part compaction must never touch)
/// begins: the index of the last genuine user-prompt message (see
/// [`is_user_prompt_message`]). Everything from that index onward — the
/// prompt itself, its whole tool-cycle chain, and the final reply — stays
/// byte-identical; everything before it is what gets folded into a summary.
/// Falls back to `0` (the whole transcript is "tail", nothing to compact) if
/// no user-prompt message is found at all.
fn tail_start_index(transcript: &[Message]) -> usize {
    transcript
        .iter()
        .rposition(is_user_prompt_message)
        .unwrap_or(0)
}

/// Flattens the given messages into a single plain-text rendering for the
/// compaction summarization call, tagging each block with its role (and tool
/// name / error state, for tool blocks) so the summarizer can tell turns and
/// speakers apart.
fn render_transcript_for_summary(messages: &[Message]) -> String {
    let mut rendered = String::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    rendered.push_str(&format!("[{:?}] {text}\n", message.role));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    rendered.push_str(&format!("[{:?} tool_use {name}] {input}\n", message.role));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    rendered.push_str(&format!(
                        "[{:?} tool_result{}] {content}\n",
                        message.role,
                        if *is_error { " error" } else { "" }
                    ));
                }
            }
        }
    }
    rendered
}

/// Outcome of a [`compact_transcript`] call (F-005 fix): distinguishes an
/// actual compaction from a no-op so callers can report each accurately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionOutcome {
    /// The transcript was compacted; this is the replacement to store.
    Compacted(Vec<Message>),
    /// There was no earlier turn to fold into a summary (the whole
    /// transcript is already "tail" — see [`tail_start_index`]), so nothing
    /// was compacted and the caller's transcript is untouched.
    NothingToCompact,
}

/// Compacts `transcript` by replacing everything before the most recent
/// user-prompt turn with a single summary message, produced by a dedicated
/// summarization call to `provider` (using [`COMPACTION_PROMPT`], not the
/// agent's own system prompt). The most recent turn — that user prompt plus
/// its entire tool-cycle chain and final reply — is copied through
/// unchanged, so a `ToolUse` is never separated from its `ToolResult`.
///
/// If no user-prompt turn is found at all (an edge case — normally the
/// caller only compacts a transcript that already has at least one), there
/// is nothing to summarize: this returns `Ok(CompactionOutcome::NothingToCompact)`,
/// leaving it to the caller to report that distinctly rather than silently
/// no-op'ing under the same `Ok(...)` shape a real compaction returns. On a
/// provider failure during the summarization call, this returns `Err`
/// without touching `transcript` at all — per the Phase 3 design, a failed
/// compaction must leave the running conversation exactly as it was.
pub async fn compact_transcript<P: Provider>(
    provider: &P,
    transcript: &[Message],
) -> Result<CompactionOutcome, P::Error> {
    let split_at = tail_start_index(transcript);
    if split_at == 0 {
        return Ok(CompactionOutcome::NothingToCompact);
    }

    let (prefix, tail) = transcript.split_at(split_at);

    let compaction_request = vec![
        Message::system_text(COMPACTION_PROMPT),
        Message::user_text(render_transcript_for_summary(prefix)),
    ];
    let (reply, _usage) = provider.send(&compaction_request, &[]).await?;

    let mut compacted = Vec::with_capacity(tail.len() + 1);
    compacted.push(Message::user_text(format!(
        "[Earlier conversation summary — compacted to save context]\n\n{}",
        reply.text()
    )));
    compacted.extend_from_slice(tail);
    Ok(CompactionOutcome::Compacted(compacted))
}

/// Sends a single user turn to `provider` and returns the assistant's reply.
/// Phase 1's minimal orchestration: wrap `input` as a user [`Message`], call
/// the provider with just that one message and no tools, and hand back
/// whatever assistant `Message` comes back. `Usage` is discarded here (no
/// caller of this Phase-1 helper threads it anywhere yet); use
/// `Provider::send` directly if the usage figures are needed.
pub async fn single_turn<P: Provider>(
    provider: &P,
    input: impl Into<String>,
) -> Result<Message, P::Error> {
    let user_message = Message::user_text(input);
    let (reply, _usage) = provider.send(&[user_message], &[]).await?;
    Ok(reply)
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
        ) -> Result<(Message, Usage), StubError> {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role, Role::User);
            assert_eq!(messages[0].text(), "hello");
            assert!(tools.is_empty());
            Ok((Message::assistant_text("hi there"), Usage::default()))
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
        ) -> Result<(Message, Usage), StubError> {
            self.calls.lock().unwrap().push(messages.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(StubError)
                .map(|message| (message, Usage::default()))
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
                cache_control: None,
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

        let (result, _usage) = run_tool_loop(
            &provider,
            "you are a test agent",
            None,
            &mut transcript,
            &tools,
            |_request| async { true },
            None,
        )
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

        // First call: the leading system message (assembled by
        // `run_tool_loop`, never stored in the caller's `transcript`), then
        // the initial user turn.
        assert_eq!(calls[0].len(), 2);
        assert_eq!(calls[0][0].role, Role::System);
        assert_eq!(calls[0][1].role, Role::User);

        // Second call: leading system message, initial user turn, the
        // assistant's tool-call turn, and a new turn carrying the tool's
        // result back to the provider.
        assert_eq!(calls[1].len(), 4);
        assert_eq!(calls[1][0].role, Role::System);
        assert_eq!(calls[1][1].role, Role::User);
        assert_eq!(calls[1][2].role, Role::Assistant);
        assert_eq!(calls[1][3].role, Role::User);

        match &calls[1][3].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
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
                cache_control: None,
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

        let (result, _usage) = run_tool_loop(
            &provider,
            "you are a test agent",
            None,
            &mut transcript,
            &tools,
            |_request| async { false },
            None,
        )
        .await
        .expect("loop should succeed even when permission is rejected");

        assert_eq!(result.text(), "final answer after rejection");
        assert!(
            !executed.load(std::sync::atomic::Ordering::SeqCst),
            "tool must not execute when permission is rejected"
        );

        let calls = provider.calls.lock().unwrap();
        // Second call: leading system message (index 0), initial user turn,
        // the assistant's tool-call turn, and the (rejected) tool result
        // turn at index 3.
        match &calls[1][3].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
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

    /// Ticket 49 (hooks-tracer-bullet), PRD "Hooks" testing decision:
    /// extends `loop_skips_execution_when_permission_rejected`'s exact
    /// fixtures (`ScriptedProvider`/`FakeGatedTool`) with a stubbed
    /// `PreToolUse` hook callback that denies, proving the ordering rule --
    /// a hook deny short-circuits BEFORE `request_permission` is invoked at
    /// all, not merely before the tool executes.
    #[tokio::test]
    async fn loop_skips_permission_prompt_and_execution_when_pretooluse_hook_denies() {
        let tool_call_reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "fake_gated".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            }],
        };
        let final_reply = Message::assistant_text("final answer after hook veto");

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

        let permission_prompt_invoked =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let permission_prompt_invoked_for_closure = permission_prompt_invoked.clone();

        let pre_tool_hook = |_request: PreToolHookRequest| {
            Box::pin(async move { PreToolHookOutcome::Deny("vetoed by hook".to_string()) })
                as Pin<Box<dyn Future<Output = PreToolHookOutcome> + Send>>
        };

        let (result, _usage) = run_tool_loop(
            &provider,
            "you are a test agent",
            None,
            &mut transcript,
            &tools,
            move |_request| {
                permission_prompt_invoked_for_closure
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                async { true }
            },
            Some(&pre_tool_hook),
        )
        .await
        .expect("loop should succeed even when a PreToolUse hook denies");

        assert_eq!(result.text(), "final answer after hook veto");
        assert!(
            !executed.load(std::sync::atomic::Ordering::SeqCst),
            "tool must not execute when the PreToolUse hook denies"
        );
        assert!(
            !permission_prompt_invoked.load(std::sync::atomic::Ordering::SeqCst),
            "request_permission must never be invoked for a call the hook already vetoed"
        );

        let calls = provider.calls.lock().unwrap();
        // Second call: leading system message (index 0), initial user turn,
        // the assistant's tool-call turn, and the (hook-vetoed) tool result
        // turn at index 3.
        match &calls[1][3].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            }] => {
                assert_eq!(tool_use_id, "call_1");
                assert!(
                    *is_error,
                    "a hook-vetoed tool call should be reflected as an error result, identical \
                     shape to an interactive rejection"
                );
                assert_eq!(content, "vetoed by hook");
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
            Some(Ok(PermissionPayload::Diff { path, old, new })) => {
                assert_eq!(path, target_path);
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
                cache_control: None,
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

        let (result, _usage) = run_tool_loop(
            &provider,
            "you are a test agent",
            None,
            &mut transcript,
            &tools,
            |_request| async { false },
            None,
        )
        .await
        .expect("loop should succeed even when permission is rejected");

        assert_eq!(result.text(), "final answer after rejection");
        assert_eq!(
            std::fs::read_to_string(&target_file).unwrap(),
            original_content,
            "a rejected write must never reach execute and must leave the file untouched"
        );

        let calls = provider.calls.lock().unwrap();
        // Second call: leading system message (index 0), initial user turn,
        // the assistant's tool-call turn, and the (rejected) tool result
        // turn at index 3.
        match &calls[1][3].content[..] {
            [ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
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

    /// Ticket 20 (auto-compaction-threshold): compaction must rewrite only
    /// the "middle" of the transcript (everything before the most recent
    /// user prompt turn) into a single summary message, while the most
    /// recent turn — the user prompt plus its whole tool-cycle chain and
    /// final reply — survives byte-for-byte, never splitting a `ToolUse`
    /// from its `ToolResult`.
    #[tokio::test]
    async fn compact_transcript_preserves_prefix_and_recent_turn_replaces_middle_with_summary() {
        let first_turn_user = Message::user_text("first turn user text");
        let first_turn_assistant = Message::assistant_text("first turn assistant text");

        let second_turn_user = Message::user_text("second turn user text");
        let second_turn_tool_use = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path": "/tmp/whatever.txt"}),
                cache_control: None,
            }],
        };
        let second_turn_tool_result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "tool result content".to_string(),
                is_error: false,
                cache_control: None,
            }],
        };
        let second_turn_assistant_reply = Message::assistant_text("second turn final reply");

        let transcript = vec![
            first_turn_user.clone(),
            first_turn_assistant.clone(),
            second_turn_user.clone(),
            second_turn_tool_use.clone(),
            second_turn_tool_result.clone(),
            second_turn_assistant_reply.clone(),
        ];

        let summary_text = "CompactionSummaryTokenForTesting";
        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                Message::assistant_text(summary_text),
            ])),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let outcome = compact_transcript(&provider, &transcript)
            .await
            .expect("compaction should succeed");
        let compacted = match outcome {
            CompactionOutcome::Compacted(compacted) => compacted,
            CompactionOutcome::NothingToCompact => {
                panic!("expected an actual compaction, got NothingToCompact")
            }
        };

        assert_eq!(
            compacted.len(),
            5,
            "expected summary message + the 4 tail messages, got: {compacted:?}"
        );

        let summary_message = &compacted[0];
        assert!(
            summary_message.text().contains(summary_text),
            "summary message should contain the scripted summary text, got: {summary_message:?}"
        );
        assert!(
            !summary_message.text().contains("first turn"),
            "summary message must not contain the first turn's raw text, got: {summary_message:?}"
        );

        assert_eq!(compacted[1], second_turn_user);
        assert_eq!(compacted[2], second_turn_tool_use);
        assert_eq!(compacted[3], second_turn_tool_result);
        assert_eq!(compacted[4], second_turn_assistant_reply);

        let calls = provider.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "exactly one summarization call should have been made"
        );
        let summarization_request_text = calls[0]
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            summarization_request_text.contains("first turn"),
            "summarization call should include the first turn's content, got: \
             {summarization_request_text}"
        );
        assert!(
            !summarization_request_text.contains("second turn"),
            "summarization call must not include the second (most-recent) turn's content, got: \
             {summarization_request_text}"
        );
    }

    /// F-005 fix: when there is no earlier turn to fold into a summary,
    /// `compact_transcript` must report that distinctly rather than silently
    /// no-op'ing under the same `Ok(...)` shape a real compaction returns.
    #[tokio::test]
    async fn compact_transcript_reports_nothing_to_compact_when_no_earlier_turn_exists() {
        let transcript = vec![Message::user_text("the only turn so far")];

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let outcome = compact_transcript(&provider, &transcript)
            .await
            .expect("compaction should succeed even when there's nothing to compact");

        assert_eq!(outcome, CompactionOutcome::NothingToCompact);

        let calls = provider.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            0,
            "no summarization call should be made when there's nothing to compact"
        );
    }

    #[test]
    fn should_compact_reuses_prior_real_usage_when_current_turn_usage_unreported() {
        let context_window_size = 200_000;
        let auto_compact_threshold = 0.7;

        let prior_usage = Usage {
            input_tokens: 150_000,
            output_tokens: 5_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let unreported_usage = Usage::default();
        // A tiny transcript, so the chars/4 fallback alone would stay far below
        // budget — should_compact must reuse the prior real usage instead.
        let transcript = vec![Message::user_text("hi")];

        assert!(
            should_compact(
                unreported_usage,
                Some(prior_usage),
                &transcript,
                context_window_size,
                auto_compact_threshold,
            ),
            "expected should_compact to reuse the prior real usage figure (above threshold) rather \
             than fall back to the chars/4 estimate on a small transcript"
        );
    }

    /// On a provider failure during the compaction call, the transcript must
    /// be left completely untouched so the session can continue with full
    /// history rather than losing it.
    #[tokio::test]
    async fn compact_transcript_leaves_transcript_intact_on_provider_failure() {
        let transcript = vec![
            Message::user_text("first turn user text"),
            Message::assistant_text("first turn assistant text"),
            Message::user_text("second turn user text"),
            Message::assistant_text("second turn assistant text"),
        ];
        let transcript_before = transcript.clone();

        let provider = ScriptedProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let result = compact_transcript(&provider, &transcript).await;

        assert!(
            result.is_err(),
            "compaction should fail when the provider errors"
        );
        assert_eq!(
            transcript, transcript_before,
            "the original transcript must be left untouched on compaction failure"
        );
    }

    /// Ticket 44 (mcp-tracer-bullet): `McpTool` in `rokr-mcp` builds its
    /// model-facing name at runtime (`mcp__<server>__<tool>`, namespaced
    /// per instance), so it cannot satisfy `ExecutableTool::name`'s
    /// original `&'static str` return type. This test hand-implements
    /// `ExecutableTool` for a type whose `name` borrows from an owned
    /// `String` field, proving the relaxed `-> &str` signature actually
    /// accepts a non-`'static` borrow (every existing `&'static str`
    /// built-in impl still coerces to `&str` unchanged, so this is the one
    /// case the relaxation newly allows). Before the relaxation, this is a
    /// compile error (E0053: method `name` has an incompatible signature
    /// for the trait) -- that compile failure IS this test's RED.
    #[test]
    fn executable_tool_name_signature_accepts_non_static_str() {
        struct OwnedNameTool {
            name: String,
        }

        impl ExecutableTool for OwnedNameTool {
            fn name(&self) -> &str {
                &self.name
            }

            fn to_tool_spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.name.clone(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                    cache_control: None,
                }
            }

            fn execute_boxed<'a>(
                &'a self,
                _input: serde_json::Value,
            ) -> Pin<Box<dyn Future<Output = Result<String, rokr_tools::ToolError>> + Send + 'a>>
            {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let tool = OwnedNameTool {
            name: format!("dynamic-{}", 1),
        };

        assert_eq!(tool.name(), "dynamic-1");
    }

    /// Ticket 47 (mcp-permission-polish): `PermissionPayload` gains a new
    /// `ToolCall` variant alongside `Command`/`Diff` so an MCP tool call's
    /// permission prompt can carry the server name, tool name, and
    /// pretty-printed input separately (rather than flattening them into
    /// one opaque `Command(String)`, ticket 44's interim decision) -- the
    /// bridge in `crates/rokr/src/main.rs` matches on these fields directly
    /// to build the rendered prompt text. This is a compile-time check as
    /// much as a runtime one: before this variant exists, constructing it
    /// is a compile error (E0599/E0433), which IS this test's RED.
    #[test]
    fn permission_payload_tool_call_variant_carries_server_tool_and_pretty_input() {
        let payload = PermissionPayload::ToolCall {
            server: "interim".to_string(),
            tool: "echo".to_string(),
            input_pretty: "{\n  \"message\": \"hi\"\n}".to_string(),
        };

        match payload {
            PermissionPayload::ToolCall {
                server,
                tool,
                input_pretty,
            } => {
                assert_eq!(server, "interim");
                assert_eq!(tool, "echo");
                assert_eq!(input_pretty, "{\n  \"message\": \"hi\"\n}");
            }
            other => panic!("expected PermissionPayload::ToolCall, got {other:?}"),
        }
    }
}
