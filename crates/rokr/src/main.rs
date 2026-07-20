use std::process::ExitCode;
use std::sync::Arc;

use rokr_core::Provider;

mod subagent;

const USAGE: &str =
    "Usage: rokr [--version] [--agent <plan|build>] [--resume <id>] [--continue] [auth login]";

/// Ticket 35 (resume-session): which prior session (if any) this run should
/// resume into, extracted from the raw CLI args before the existing
/// `--version`/`auth login`/`parse_agent_tier` matching runs.
enum ResumeMode {
    None,
    Id(String),
    Continue,
}

/// Pulls `--resume <id>` / `--continue` (in any position) out of `args`,
/// returning the resolved `ResumeMode` plus the remaining args untouched --
/// so the existing `--version` / `auth login` / `parse_agent_tier` matching
/// keeps working exactly as today, just against the filtered remainder.
fn extract_resume_mode(args: &[String]) -> (ResumeMode, Vec<String>) {
    let mut mode = ResumeMode::None;
    let mut remaining = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--resume" => {
                if let Some(id) = iter.next() {
                    mode = ResumeMode::Id(id.clone());
                }
            }
            "--continue" => mode = ResumeMode::Continue,
            other => remaining.push(other.to_string()),
        }
    }
    (mode, remaining)
}

/// The agent's tool tier, selected via `--agent` and defaulting to `Plan`
/// when no flag is given. `Plan` is read-only: read/glob/grep/ls only, so
/// the agent can explore and reason about a codebase without being able to
/// change anything. `Build` adds bash/write/edit on top, unlocking actual
/// mutation. Each tier's tools are all wired through the same
/// `rokr_core::run_tool_loop`; the tier only changes which tools are handed
/// in and which system prompt (`{config_dir}/agents/{tier}.md`) is seeded.
#[derive(Clone, Copy)]
enum AgentTier {
    Plan,
    Build,
}

impl AgentTier {
    fn prompt_name(self) -> &'static str {
        match self {
            AgentTier::Plan => "plan",
            AgentTier::Build => "build",
        }
    }
}

/// F-003: the real send path (`run_tool_loop`, `compact_transcript`) AND
/// `subagent::SubagentTool` (ticket 30, F-004) share this SINGLE
/// resilience-wrapped provider — previously this was two separate locks (a
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
type SharedProvider = Arc<tokio::sync::RwLock<rokr_provider::ResilientProvider<rokr_provider::AnyProvider>>>;

/// Parses the raw CLI args (already stripped of argv[0]) into an
/// `AgentTier`. No args at all defaults to `Plan`; `--agent plan` and
/// `--agent build` select explicitly; anything else is a usage error.
fn parse_agent_tier(args: &[String]) -> Result<AgentTier, ()> {
    match args {
        [] => Ok(AgentTier::Plan),
        [flag, value] if flag == "--agent" => match value.as_str() {
            "plan" => Ok(AgentTier::Plan),
            "build" => Ok(AgentTier::Build),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Ticket 35 (resume-session): pulled out BEFORE the existing match so
    // `--resume <id>` / `--continue` can appear anywhere on the command
    // line without disturbing the `--version` / `auth login` /
    // `parse_agent_tier` matching below, which now runs against
    // `remaining_args` instead of the raw `args`.
    let (resume_mode, remaining_args) = extract_resume_mode(&args);

    match remaining_args.as_slice() {
        [flag] if flag == "--version" => {
            println!("rokr {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        // Ticket 31 (oauth-pkce-login): runs the PKCE login flow and exits
        // rather than entering the TUI. Parallel to the `--version` arm
        // above -- there's no clap/subcommand framework in this binary yet,
        // so this is a second explicit match arm, same as that one.
        [a, b] if a == "auth" && b == "login" => {
            let config_dir = rokr_config::default_config_dir();
            let token_store = rokr_provider::auth::default_token_store(&config_dir);
            match rokr_provider::auth::login(token_store.as_ref()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("login failed: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            let agent = match parse_agent_tier(&remaining_args) {
                Ok(agent) => agent,
                Err(()) => {
                    eprintln!("{USAGE}");
                    return ExitCode::FAILURE;
                }
            };

            let config = match rokr_config::load_or_init_default() {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("failed to initialize config: {err}");
                    return ExitCode::FAILURE;
                }
            };

            // Ticket 30 (subagent-tool): named so it can be cloned into
            // `submit`'s closure below and reused there to load a *named
            // subagent's* own prompt (`{config_dir}/agents/{name}.md`) via
            // the same `rokr_config::read_agent_prompt` call this crate
            // already makes for the top-level agent tier's prompt.
            let config_dir = rokr_config::default_config_dir();

            let mut system_prompt = match rokr_config::read_agent_prompt(
                &config_dir,
                agent.prompt_name(),
            ) {
                Ok(prompt) => prompt,
                Err(err) => {
                    eprintln!("failed to read agent prompt: {err}");
                    return ExitCode::FAILURE;
                }
            };

            // Resolved once at startup and reused for both the project-
            // context load below and repo-map (re)generation (F-001 fix):
            // repo-map regeneration on `/compact` needs the same root the
            // initial generation used. A cwd that can't be resolved is
            // treated the same as no project context / no repo map.
            let cwd: Option<std::path::PathBuf> = std::env::current_dir().ok();

            // One-time, unconditional, side-effect-free read of project-level
            // context (AGENTS.md, falling back to CLAUDE.md) from the
            // current working directory, folded into the system prompt
            // alongside the active agent tier's prompt. Not a tool, not
            // permission-gated — this is how the system prompt is built, not
            // a model-invoked action.
            if let Some(cwd) = cwd.as_deref() {
                if let Some(project_context) = rokr_config::load_project_context(cwd) {
                    system_prompt.push_str("\n\n");
                    system_prompt.push_str(&project_context);
                }
            }

            // Generated once per session (ticket 18: repo-map-generation)
            // and held in shared, mutable state (F-001 fix) rather than a
            // plain `Option<String>` captured immutably by `submit`: the
            // PRD requires the map to regenerate on `/compact` and never
            // per turn. `submit` only ever reads the current value; the
            // `/compact` handler in `command` below is the sole writer.
            let repo_map: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(
                cwd.as_deref().map(rokr_tools::repo_map::generate),
            ));

            // Constructed once at startup (so a missing/invalid env var
            // doesn't crash the TUI — it's reported the first time the
            // user submits a prompt instead) via `rokr_provider::build_provider`
            // (ticket 32: provider-factory-seam) and wired through
            // `rokr_core::run_tool_loop`. `rokr-tui` stays decoupled from
            // `rokr-core`/`rokr-provider`, so this closure is where the
            // message model and provider abstraction meet the TUI.
            //
            // Ticket 29 (model-session-switch): held behind a
            // `tokio::sync::RwLock` (never written to `rokr_config::Config`
            // or disk — there is no field for it in the config schema) so
            // `/model <name>` can swap the active provider at runtime.
            // Readers (`submit`, `/compact`) clone the provider out from
            // behind the read lock and drop the guard before any `.await`
            // that uses it.
            // Ticket 31 (oauth-pkce-login): auth is resolved (config auth
            // block -- always `None` today, no field for it in
            // `rokr_config::Config` yet -- then a stored keychain/file
            // OAuth token, then the existing `ROKR_ANTHROPIC_API_KEY` env
            // var) before falling through to the unchanged
            // `AnyProvider::from_env()` path when nothing is stored. This
            // preserves today's behavior exactly for anyone who has never
            // run `rokr auth login`.
            //
            // F-003: a SINGLE piece of shared state comes out of the
            // `build_provider` call now, backing both the real send path
            // (`run_tool_loop`, `compact_transcript`) and
            // `subagent::SubagentTool` (F-004: also resilience-wrapped now,
            // see `subagent.rs`'s doc comment). Previously this was two
            // separate `Arc<RwLock<..>>`s (`SharedProvider` +
            // `SharedSubagentProvider`) written and read non-atomically —
            // a `/model` switch racing `submit`'s two reads could split the
            // parent and a subagent onto different backends. Collapsing to
            // one lock, made possible by `ResilientProvider<P>` now being
            // `Clone` (see `resilience.rs`), removes that race entirely:
            // there is exactly one write in `set_active_provider` and
            // exactly one read in `submit`.
            let token_store = rokr_provider::auth::default_token_store(&config_dir);
            let resolved_auth = rokr_provider::auth::resolve_auth(
                None,
                token_store.as_ref(),
                rokr_provider::anthropic::ENV_API_KEY,
            );
            let built = rokr_provider::build_provider(
                None,
                resolved_auth,
                rokr_provider::RetryPolicy::default(),
            );

            // Ticket 34 (persist-new-sessions): the Header record needs a
            // provider/model string pair captured at the SAME construction
            // pass as `provider` itself, before `built.resilient` is moved
            // into the shared lock -- `ResilientProvider` exposes no
            // accessor to recover this after the fact (see its own doc
            // comment on why), so it must be read off `built.selected`
            // (the plain `AnyProvider` `BuiltProvider` also returns) right
            // here.
            let (provider, provider_name, model_name): (Result<SharedProvider, String>, String, String) =
                match built {
                    Ok(built) => {
                        let (provider_name, model_name) = match &built.selected {
                            rokr_provider::AnyProvider::OpenAi(_) => (
                                "openai".to_string(),
                                std::env::var(rokr_provider::openai::ENV_MODEL)
                                    .unwrap_or_else(|_| "unknown".to_string()),
                            ),
                            rokr_provider::AnyProvider::Anthropic(_) => (
                                "anthropic".to_string(),
                                std::env::var(rokr_provider::anthropic::ENV_MODEL)
                                    .unwrap_or_else(|_| "unknown".to_string()),
                            ),
                        };
                        (
                            Ok(Arc::new(tokio::sync::RwLock::new(built.resilient))),
                            provider_name,
                            model_name,
                        )
                    }
                    Err(err) => (Err(err), "unknown".to_string(), "unknown".to_string()),
                };

            // Ticket 34 (persist-new-sessions) / ticket 35 (resume-session):
            // constructed once at startup, central storage (not
            // per-project) per the PRD. `ResumeMode::None` preserves
            // exactly today's behavior (`create_session` + `append_header`,
            // empty transcript, no known usage). `ResumeMode::Id`/`Continue`
            // instead resolve a concrete prior session id, fold its log via
            // `resume_session` to seed `transcript`/`last_known_usage`, and
            // open (not create) that session's log for continued appends --
            // no Header is re-written, it's already the first line of the
            // log. A store/creation failure degrades gracefully (no
            // persistence this run) rather than crashing the TUI, matching
            // this function's existing pattern for other optional startup
            // concerns (e.g. repo map generation); a resume failure,
            // though, is a hard error -- there is nothing sensible to fall
            // back to if the session the user explicitly asked to resume
            // can't be read. Wrapped in `Arc` (rather than requiring
            // `SessionHandle: Clone`) so it can be cloned into `submit`'s
            // closure below without touching rokr-session's type.
            // Ticket 38 (checkpoint-pre-images): kept as its own binding
            // (rather than re-derived from `store`, which exposes no
            // accessor for its own root) so `submit`'s closure below can
            // build a `CheckpointStore` for whichever session is currently
            // active without threading a second lookup through
            // `SessionStore`.
            let data_dir = default_data_dir();
            let store = rokr_session::SessionStore::open(&data_dir);

            // Ticket 40 (prompt-history), PRD decision 5: loaded once at
            // startup -- lives at `data_dir/history`, a sibling of
            // `sessions/`, entirely separate from any one session's own
            // log (cross-session, not session-scoped). A load failure
            // degrades gracefully (empty history this run) rather than
            // crashing the TUI, matching this function's existing pattern
            // for other optional startup concerns.
            let prompt_history = rokr_session::PromptHistory::load(&data_dir).unwrap_or_default();

            let (initial_transcript, initial_last_known_usage, session_handle, initial_turn_index): (
                Vec<rokr_core::Message>,
                Option<rokr_core::Usage>,
                Option<Arc<rokr_session::SessionHandle>>,
                usize,
            ) = match resume_mode {
                ResumeMode::None => {
                    let session_handle = match store.create_session() {
                        Ok(handle) => {
                            handle.append_header(
                                1,
                                handle.session_id().to_string(),
                                now_timestamp(),
                                cwd.as_ref()
                                    .map(|c| c.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                agent.prompt_name().to_string(),
                                provider_name.clone(),
                                model_name.clone(),
                            );
                            Some(Arc::new(handle))
                        }
                        Err(err) => {
                            eprintln!("failed to create session log: {err}");
                            None
                        }
                    };
                    // A brand-new session has zero prior Turn records, so
                    // ticket 38's turn_index counter (which must equal the
                    // count of prior Turn records, per `fold`'s
                    // `next_turn_index` semantics) starts at 0.
                    (Vec::new(), None, session_handle, 0)
                }
                ResumeMode::Continue => {
                    let session_id = match store.most_recent_session_id() {
                        Ok(Some(id)) => id,
                        Ok(None) => {
                            eprintln!("no prior session found to continue");
                            return ExitCode::FAILURE;
                        }
                        Err(err) => {
                            eprintln!("failed to look up most recent session: {err}");
                            return ExitCode::FAILURE;
                        }
                    };
                    match resolve_resumed_session(&store, session_id) {
                        Ok(resolved) => resolved,
                        Err(code) => return code,
                    }
                }
                ResumeMode::Id(session_id) => match resolve_resumed_session(&store, session_id) {
                    Ok(resolved) => resolved,
                    Err(code) => return code,
                },
            };

            // Ticket 36 (session-index-list-jump): wrapped behind a
            // `tokio::sync::RwLock` so `/resume <id> --yes` can repoint the
            // active session writer mid-session, mirroring how
            // `SharedProvider`/`/model` already swap the active provider
            // behind a lock.
            let session_handle: Arc<tokio::sync::RwLock<Option<Arc<rokr_session::SessionHandle>>>> =
                Arc::new(tokio::sync::RwLock::new(session_handle));

            // In-memory only (no persistence beyond the session log, per
            // the PRD): accumulates every turn across submits for the
            // lifetime of the process, so each new prompt is sent with the
            // full prior conversation history rather than in isolation.
            // Seeded from a resumed session's folded messages when
            // applicable (ticket 35), otherwise starts empty. Stays
            // system-prompt-free — pure conversation history;
            // `rokr_core::run_tool_loop` prepends the system segment itself
            // (via `context::assemble()`) on every outgoing send, so it
            // never needs to live here.
            let transcript: Arc<tokio::sync::Mutex<Vec<rokr_core::Message>>> =
                Arc::new(tokio::sync::Mutex::new(initial_transcript));

            // Ticket 20 (auto-compaction-threshold): both `Copy`, captured
            // by value once by the outer `move` closure below and reused
            // on every invocation, mirroring `agent`'s existing capture
            // pattern in this same closure.
            let context_window_size = config.context_window_size;
            let auto_compact_threshold = config.auto_compact_threshold;

            // F-003 fix: the most recent turn's real (non-zero) usage
            // figure, shared across submits so a turn whose usage goes
            // unreported (some OpenAI-compatible proxies intermittently
            // omit it) can fall back to the last real figure instead of
            // the much cruder chars/4 estimate, which is reserved for the
            // case no real usage has ever arrived yet this session. Ticket
            // 35 (resume-session): seeded from the resumed session's
            // restored usage when applicable, otherwise `None` as before.
            let last_known_usage: Arc<std::sync::Mutex<Option<rokr_core::Usage>>> =
                Arc::new(std::sync::Mutex::new(initial_last_known_usage));

            // Ticket 38 (checkpoint-pre-images): mirrors `last_known_usage`'s
            // exact `Arc<std::sync::Mutex<>>` shape immediately above. Its
            // value equals the count of prior `Turn` records (0-based, per
            // `fold`'s `next_turn_index` semantics) -- the index the NEXT
            // submitted turn's own `Turn` record will occupy once appended.
            // `submit`'s tool loop reads the CURRENT value for every
            // snapshot taken during that turn's `run_tool_loop` call, and
            // increments it by exactly one right after that turn's
            // `append_turn` call (never before, so every gated tool call
            // within one in-flight turn shares the same pre-increment
            // index). Seeded from a resumed session's real prior `Turn`
            // count (see `resolve_resumed_session`'s doc comment) when
            // applicable, otherwise 0 as for a brand-new session.
            let turn_index: Arc<std::sync::Mutex<usize>> =
                Arc::new(std::sync::Mutex::new(initial_turn_index));

            // Ticket 21 (manual-compact-command): cloned here, before
            // `submit`'s `move` closure below takes ownership of the
            // original `provider`/`transcript` bindings, so `command` can
            // independently share the same provider lock and running
            // transcript. `command_cwd`/`command_repo_map` (F-001 fix) let
            // the `/compact` handler regenerate the repo map in the same
            // shared state `submit` reads from. Ticket 29
            // (model-session-switch): `command_provider` is also how
            // `/model` writes a newly selected provider into the same
            // `SharedProvider` that `submit` reads.
            let command_provider = provider.clone();
            let command_transcript = transcript.clone();
            let command_cwd = cwd.clone();
            let command_repo_map = repo_map.clone();
            let command_store = store.clone();
            // Ticket 39 (rollback-command): `/rollback`'s handler in
            // `command` below needs its own clone of `data_dir` to build a
            // `CheckpointStore` for whichever session is currently active,
            // mirroring `submit`'s own `data_dir` clone.
            let command_data_dir = data_dir.clone();
            // Ticket 38 (checkpoint-pre-images): `/resume <id> --yes` in
            // `command` below repoints the active session writer, and must
            // re-seed `turn_index` to the TARGET session's real prior `Turn`
            // count the same way startup resume-seeding does -- otherwise a
            // turn submitted after an in-session jump would capture
            // checkpoints keyed by the WRONG session's turn numbering.
            let command_turn_index = turn_index.clone();
            // Ticket 36 (session-index-list-jump): `/resume <id>`'s handler
            // in `command` below needs its own clones of the swappable
            // `session_handle` lock and `last_known_usage`, so it can read
            // the currently-active session id, and (on `--yes`) repoint the
            // writer and restore the folded usage figure.
            let command_session_handle = session_handle.clone();
            let command_last_known_usage = last_known_usage.clone();
            // F-005: `/model`'s handler in `command` below needs its own
            // clone of `config_dir` (to resolve auth for the requested
            // backend) -- cloned here, before `submit`'s `move` closure
            // takes ownership of the original binding, same reasoning as
            // `command_provider` above.
            let command_config_dir = config_dir.clone();
            // Ticket 40 (prompt-history): `on_history_append`'s closure
            // (built below, after `submit`/`command`) needs its own clone
            // of `data_dir`, cloned here before `submit`'s `move` closure
            // takes ownership of the original binding -- mirrors
            // `command_data_dir`/`command_config_dir` above for the same
            // reason.
            let history_append_data_dir = data_dir.clone();

            // Ticket 43 (mouse-scroll-status-line): created here (like
            // `on_history_append`'s data_dir clone above) so `submit`'s
            // `move` closure below can capture its own clone of the sender
            // -- the receiver crosses into `rokr_tui::run` unchanged,
            // mirroring how `history`/`on_history_append` are threaded
            // through from main.rs rather than being created inside
            // rokr-tui's own event loop (unlike the permission channel,
            // which rokr-tui owns end-to-end since only it needs both
            // ends).
            let (status_tx, status_rx) = std::sync::mpsc::channel::<rokr_tui::SessionStatus>();

            let submit = move |input: String, permission: rokr_tui::PermissionHandle| {
                let provider = provider.clone();
                let transcript = transcript.clone();
                let system_prompt = system_prompt.clone();
                let repo_map = repo_map.clone();
                let last_known_usage = last_known_usage.clone();
                let config_dir = config_dir.clone();
                let session_handle = session_handle.clone();
                let turn_index = turn_index.clone();
                let data_dir = data_dir.clone();
                let status_tx = status_tx.clone();
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
                    let bash = rokr_tools::bash::BashTool;
                    let write = rokr_tools::write::WriteTool;
                    let edit = rokr_tools::edit::EditTool;
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
                    let subagent_request_permission_session_handle = session_handle.clone();
                    let subagent_request_permission_turn_index = turn_index.clone();
                    let subagent_request_permission_data_dir = data_dir.clone();

                    // Bridges rokr-core's `PermissionRequest` (tool name +
                    // `PermissionPayload`) to rokr-tui's primitive
                    // `PermissionRequest` (tool name + a display string),
                    // round-tripping through the TUI's render loop via
                    // `permission`. This is the seam rokr-tui's `run` doc
                    // comment calls out: rokr-tui stays decoupled from
                    // rokr-core's specific types, so main.rs bridges them.
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
                            };
                            let granted = permission
                                .request(rokr_tui::PermissionRequest {
                                    tool_name: request.tool_name,
                                    detail,
                                })
                                .await;
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
                    // PermissionRequest to the SAME rokr_tui::PermissionHandle
                    // the parent's own request_permission above uses (PRD
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
                                };
                                let granted = permission
                                    .request(rokr_tui::PermissionRequest {
                                        tool_name: request.tool_name,
                                        detail,
                                    })
                                    .await;
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
                    );

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
                    let expanded_input = rokr_core::mentions::expand_mentions(&input, |path| {
                        match std::fs::read_to_string(path) {
                            Ok(contents) => rokr_core::mentions::MentionResolution::Found(contents),
                            Err(_) => rokr_core::mentions::MentionResolution::NotFound,
                        }
                    });

                    let mut transcript = transcript.lock().await;
                    accumulate_user_turn(&mut transcript, expanded_input);

                    let repo_map_snapshot: Option<String> = repo_map.lock().unwrap().clone();

                    let (reply, usage) = rokr_core::run_tool_loop(
                        &provider,
                        &system_prompt,
                        repo_map_snapshot.as_deref(),
                        &mut transcript,
                        &tools,
                        request_permission,
                    )
                    .await
                    .map_err(|err| err.to_string())?;

                    // Ticket 34 (persist-new-sessions): the Turn record's
                    // message is built from the ORIGINAL submitted `input`
                    // (before `@path`-mention expansion), matching what the
                    // acceptance test asserts -- the persisted log should
                    // show exactly what the user typed, not the expanded
                    // form that actually goes out on the wire.
                    {
                        let session_handle_guard = session_handle.read().await;
                        if let Some(session_handle) = session_handle_guard.as_ref() {
                            session_handle.append_turn(
                                rokr_core::Message::user_text(input.clone()),
                                rokr_session::UsageRecord::from(usage),
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
                    let _ = status_tx.send(rokr_tui::SessionStatus { context_percent });

                    let notice = if rokr_core::should_compact(
                        usage,
                        prior_usage,
                        &transcript,
                        context_window_size,
                        auto_compact_threshold,
                    ) {
                        match rokr_core::compact_transcript(&provider, &transcript).await {
                            Ok(rokr_core::CompactionOutcome::Compacted(compacted)) => {
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
            };

            // Ticket 36 (session-index-list-jump): `/sessions` is added to
            // this same match the same way ticket 29 added `/model <name>`
            // above -- a new arm reading through `command_store`, no new
            // dispatch mechanism.
            // Ticket 21 (manual-compact-command): rokr-tui only knows
            // "slash-prefixed input goes to `command`" (see `route_input`);
            // this closure is where a literal command string like
            // `/compact` gets meaning. Deliberately a single minimal match
            // arm, not a registry/parser framework (a later phase extends
            // this seam). Ticket 29 (model-session-switch) adds `/model
            // <name>` ahead of that match: it performs provider
            // *selection* ("openai" or "anthropic"), not per-field
            // model-string editing. F-003: the switch now writes through
            // `set_active_provider` into the SINGLE `SharedProvider` lock
            // (see that type's doc comment) — no second lock to keep in
            // sync, so a `/model` switch can no longer split the parent and
            // a subagent onto different backends. F-005: name resolution
            // itself routes through `resolve_auth` + `build_provider` (see
            // `set_active_provider`'s doc comment) rather than
            // `AnyProvider::from_name`, so an OAuth-only user (no
            // `ROKR_ANTHROPIC_API_KEY` env var) can still switch to
            // anthropic.
            let command = move |input: String| {
                let provider = command_provider.clone();
                let config_dir = command_config_dir.clone();
                let transcript = command_transcript.clone();
                let cwd = command_cwd.clone();
                let repo_map = command_repo_map.clone();
                let store = command_store.clone();
                let session_handle = command_session_handle.clone();
                let last_known_usage = command_last_known_usage.clone();
                let turn_index = command_turn_index.clone();
                let data_dir = command_data_dir.clone();
                async move {
                    if let Some(name) = input.strip_prefix("/model ") {
                        let name = name.trim();
                        return match &provider {
                            Ok(lock) => match set_active_provider(lock, &config_dir, name).await {
                                Ok(()) => "Active provider switched.".to_string(),
                                Err(err) => format!("failed to switch provider: {err}"),
                            },
                            Err(err) => {
                                format!("cannot switch provider: {err}")
                            }
                        };
                    }

                    // Ticket 36 (session-index-list-jump): `/resume <id>`
                    // (warn-first) and `/resume <id> --yes` (confirm-and-
                    // swap) both start with this prefix -- everything after
                    // it (including a trailing `--yes`) is handled by
                    // `handle_resume_command`, which this closure just
                    // delegates to, same shape as `/model <name>` above.
                    if let Some(arg) = input.strip_prefix("/resume ") {
                        return handle_resume_command(
                            &store,
                            &transcript,
                            &session_handle,
                            &last_known_usage,
                            &turn_index,
                            arg,
                        )
                        .await;
                    }

                    // Ticket 39 (rollback-command): `/rollback` (bare, uses
                    // a sensible default target) and `/rollback <turn>`
                    // both start with this prefix -- everything after it
                    // (trimmed) is the target turn argument, delegated to
                    // `handle_rollback_command`, mirroring the `/resume `
                    // seam immediately above. Matched via `==`/`starts_with`
                    // rather than `strip_prefix("/rollback ")` (unlike
                    // `/resume`) specifically so the BARE `/rollback` (no
                    // trailing space, no argument) is also routed here.
                    if input == "/rollback" || input.starts_with("/rollback ") {
                        let arg = input.strip_prefix("/rollback").unwrap_or("").trim();
                        return handle_rollback_command(
                            &data_dir,
                            &store,
                            &transcript,
                            &session_handle,
                            &last_known_usage,
                            &turn_index,
                            arg,
                        )
                        .await;
                    }

                    // Ticket 37 (session-search): `/search <term>` is a
                    // lazy, on-demand scan of every session's on-disk body
                    // (PRD decision 2) -- it never consults
                    // `sessions/index.jsonl`, so a term that only appears
                    // inside a `Compaction` summary is still found. Mirrors
                    // the `/resume ` seam above: everything after the
                    // literal prefix is the search term, delegated straight
                    // to `SessionStore::search`.
                    if let Some(term) = input.strip_prefix("/search ") {
                        let term = term.trim();
                        return match store.search(term) {
                            Ok(matches) if matches.is_empty() => {
                                format!("No sessions found matching {term:?}.")
                            }
                            Ok(matches) => matches.join("\n"),
                            Err(err) => format!("failed to search sessions: {err}"),
                        };
                    }

                    match input.as_str() {
                        "/sessions" => match store.list_sessions() {
                            Ok(entries) if entries.is_empty() => {
                                "No prior sessions found.".to_string()
                            }
                            Ok(entries) => entries
                                .iter()
                                .map(|entry| {
                                    format!(
                                        "{} | {} | \"{}\" | turns={} | model={} | created={} | updated={}",
                                        entry.session_id,
                                        entry.project_path,
                                        entry.title,
                                        entry.turn_count,
                                        entry.last_model,
                                        entry.created_at,
                                        entry.updated_at,
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                            Err(err) => format!("failed to list sessions: {err}"),
                        },
                        "/compact" => {
                            let provider_snapshot = match &provider {
                                Ok(lock) => lock.read().await.clone(),
                                Err(err) => {
                                    return format!(
                                        "compaction failed, transcript left intact: {err}"
                                    )
                                }
                            };
                            let mut transcript = transcript.lock().await;
                            match rokr_core::compact_transcript(
                                &provider_snapshot,
                                &transcript,
                            )
                            .await
                            {
                                Ok(rokr_core::CompactionOutcome::Compacted(compacted)) => {
                                    *transcript = compacted;
                                    if let Some(cwd) = cwd.as_deref() {
                                        let regenerated = rokr_tools::repo_map::generate(cwd);
                                        *repo_map.lock().unwrap() = Some(regenerated);
                                    }
                                    "Transcript compacted.".to_string()
                                }
                                Ok(rokr_core::CompactionOutcome::NothingToCompact) => {
                                    "Nothing to compact.".to_string()
                                }
                                Err(err) => {
                                    format!("compaction failed, transcript left intact: {err}")
                                }
                            }
                        }
                        _ => format!("unknown command: {input}"),
                    }
                }
            };

            let on_history_append = move |prompt: String| {
                if let Err(err) =
                    rokr_session::PromptHistory::append(&history_append_data_dir, &prompt)
                {
                    eprintln!("failed to append prompt to history: {err}");
                }
            };

            match rokr_tui::run(submit, command, prompt_history, on_history_append, status_rx).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) if err.is_not_a_tty() => {
                    // Not an error in a scripting/piping context: config is
                    // already initialized, there's just no terminal to draw
                    // into. Report it clearly on stderr without treating it
                    // as a hard failure.
                    eprintln!("{err}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Appends a new user-turn message onto the running conversation transcript.
/// `run_tool_loop` appends the corresponding assistant/tool-call/tool-result
/// messages as it executes; this is the seam where a fresh prompt joins that
/// running history.
fn accumulate_user_turn(transcript: &mut Vec<rokr_core::Message>, input: String) {
    transcript.push(rokr_core::Message::user_text(input));
}

/// Ticket 38 (checkpoint-pre-images), PRD phase-5-session-management
/// decision 4: on a GRANTED write/edit permission decision, captures the
/// file's pre-image under `sessions/<id>/snapshots/` (reusing the `old`
/// content already computed for the permission-preview diff -- no new file
/// read) and appends a correlating `Checkpoint` record. Called from BOTH
/// `submit`'s own `request_permission` closure and the mirrored
/// `subagent_request_permission` closure in `crates/rokr/src/main.rs`,
/// after `permission.request(...)`'s decision comes back `true` -- a DENY
/// must produce no snapshot and no `Checkpoint` record, which this
/// signature enforces structurally: callers only invoke it once already
/// inside their own `if granted` branch.
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
/// Known limitation (see this ticket's `## Scope Amendment`):
/// `PermissionPayload::Diff`'s `old: String` field collapses "file did not
/// exist" and "file existed but was empty" into the same empty string, with
/// no separate boolean carried through (the architect's ruling fixed
/// `Preview::Diff`/`PermissionPayload::Diff`'s shape to exactly `{ path,
/// old, new }`, and adding a second field for this was judged out of scope
/// for this ticket). So an empty `old` is treated here as "absent"
/// (`None`), which is lossy for the rare case of a genuinely-empty
/// pre-existing file -- `CheckpointStore::snapshot` itself DOES support the
/// real distinction (`Option<&str>`), this call site just cannot supply it
/// today.
async fn capture_checkpoint_if_granted_diff(
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

/// Ticket 35 (resume-session): resolves a concrete `session_id` (already
/// looked up from `--continue` if needed) into the seed values `main`'s
/// `_` arm needs -- the folded prior transcript, the restored
/// `last_known_usage`, and a `SessionHandle` opened (not created) for
/// continued appends, since the resumed session's `Header` record is
/// already the first line of its log. Both `store.resume_session` failing
/// (the session the user explicitly asked to resume can't be read -- a
/// hard error, unlike `create_session` failing at plain startup, which
/// degrades gracefully) and `store.open_session` failing (degrades
/// gracefully, same as `create_session` failing at plain startup) are
/// handled here so both `ResumeMode::Id` and `ResumeMode::Continue` share
/// the identical resolution logic.
///
/// Ticket 38 (checkpoint-pre-images) / F-003 (argus review, phase-5): also
/// seeds a starting `turn_index` (the 4th tuple element) for the resumed
/// session. This comes directly from `fold`'s own real `next_turn_index`
/// (carried through `ResumeState`, see `rokr-session`), NOT from a separate
/// `store.list_sessions()` lookup against `sessions/index.jsonl` -- the
/// index is a denormalized, appended-incrementally read-optimization over
/// the session's own `session.jsonl` log, and can be missing an entry
/// entirely (or stale) for a session that legitimately exists on disk. Per
/// `fold`'s doc comment, a `Rollback` record does not rewind
/// `next_turn_index` -- later genuinely-new turns keep incrementing from
/// where the raw `Turn` count left off -- and `ResumeState::next_turn_index`
/// already reflects that correctly because it's the exact same value `fold`
/// itself computed while building `messages` above.
#[allow(clippy::type_complexity)]
fn resolve_resumed_session(
    store: &rokr_session::SessionStore,
    session_id: String,
) -> Result<
    (
        Vec<rokr_core::Message>,
        Option<rokr_core::Usage>,
        Option<Arc<rokr_session::SessionHandle>>,
        usize,
    ),
    ExitCode,
> {
    let (messages, _meta, resume_state) = store.resume_session(&session_id).map_err(|err| {
        eprintln!("failed to resume session {session_id}: {err}");
        ExitCode::FAILURE
    })?;

    let session_handle = match store.open_session(session_id) {
        Ok(handle) => Some(Arc::new(handle)),
        Err(err) => {
            eprintln!("failed to reopen session log for continued appends: {err}");
            None
        }
    };

    Ok((
        messages,
        resume_state.last_known_usage,
        session_handle,
        resume_state.next_turn_index,
    ))
}

/// Ticket 36 (session-index-list-jump): the decision + mutation logic
/// behind the in-session `/resume <id>` (warn-first) and `/resume <id>
/// --yes` (confirm-and-swap) jump commands. Kept as its own testable async
/// fn (mirroring `set_active_provider`'s existing pattern in this same test
/// module) so its branches can be exercised directly against a real
/// `SessionStore` and real temp-dir fixtures without a full PTY round-trip
/// for every case.
///
/// Per the architect's scope-amendment ruling (recorded on the ticket): the
/// swap warning is re-grounded to "target session id != current session
/// id" rather than "a tool loop is in-flight" -- `rokr-tui`'s `command`
/// closure has no visibility into `AppState::pending`/the prompt buffer,
/// and extending that crate boundary was ruled out of scope. `state.pending`'s
/// existing keystroke-drop behavior (see `rokr-tui::event_loop`) already
/// makes `/resume` and a live tool loop mutually exclusive for free -- see
/// the regression-guard test in `tui_test.rs`.
///
/// `arg` is everything after `/resume ` (e.g. `"01ABC... --yes"` or just
/// `"01ABC..."`); this fn splits off a trailing `--yes` itself.
///
/// F-007 (argus review): flushes the ORIGIN session's writer BEFORE
/// resolving/opening the TARGET, so an append enqueued just before this
/// jump (this turn's own `Turn` record, or an earlier `Checkpoint` record
/// from the same turn) is guaranteed to reach disk before this session's
/// log is later re-read by a future jump back to it.
///
/// F-003 / F-008 (argus review): both the unconfirmed warning message and
/// the confirmed swap re-seed `turn_index` from `fold`'s real
/// `next_turn_index` (via `ResumeState`), not from `sessions/index.jsonl`'s
/// `turn_count` -- a target session that exists on disk but is absent from
/// the index (e.g. the index file was lost or is stale) still needs to
/// work correctly rather than falling back to "no such session" or a wrong
/// turn_index of 0.
async fn handle_resume_command(
    store: &rokr_session::SessionStore,
    transcript: &tokio::sync::Mutex<Vec<rokr_core::Message>>,
    session_handle: &tokio::sync::RwLock<Option<Arc<rokr_session::SessionHandle>>>,
    last_known_usage: &std::sync::Mutex<Option<rokr_core::Usage>>,
    turn_index: &std::sync::Mutex<usize>,
    arg: &str,
) -> String {
    let (target_id, confirmed) = match arg.trim().strip_suffix("--yes") {
        Some(id) => (id.trim(), true),
        None => (arg.trim(), false),
    };

    let indexed_entry = match store.list_sessions() {
        Ok(entries) => entries.into_iter().find(|entry| entry.session_id == target_id),
        Err(err) => return format!("failed to look up session {target_id}: {err}"),
    };

    let current_id = session_handle
        .read()
        .await
        .as_ref()
        .map(|handle| handle.session_id().to_string());
    if current_id.as_deref() == Some(target_id) {
        return format!("already on session {target_id}");
    }

    if !confirmed {
        // F-008: an indexed entry gives a real title/turn-count for the
        // warning message. A session that exists on disk but is absent
        // from sessions/index.jsonl (e.g. the index file was lost or is
        // stale) still needs a warning rather than a wrong "no such
        // session" -- fall back to `resume_session` just to confirm the
        // session actually exists and to derive a REAL turn count from the
        // fold (not a placeholder), with a placeholder title since only
        // the index ever carried one.
        return match indexed_entry {
            Some(entry) => format!(
                "Switching to {target_id} ({}, {} turns) replaces your current context. \
                 Run '/resume {target_id} --yes' to confirm.",
                entry.title, entry.turn_count
            ),
            None => match store.resume_session(target_id) {
                Ok((_, _, resume_state)) => format!(
                    "Switching to {target_id} (<not in session index>, {} turns) replaces \
                     your current context. Run '/resume {target_id} --yes' to confirm.",
                    resume_state.next_turn_index
                ),
                Err(_) => format!("no such session {target_id}"),
            },
        };
    }

    // F-007: flush the ORIGIN session's writer BEFORE resolving/opening the
    // TARGET. Without this, an append enqueued just before this jump (e.g.
    // this turn's own Turn record, or an earlier Checkpoint record from the
    // same turn) has no guaranteed opportunity to reach disk before this
    // session's log is later re-read by a FUTURE jump back to it -- and
    // briefly having two writer tasks alive against the same origin file
    // (the old one draining its queue, a hypothetical new one) is also
    // avoided by making sure the old one's queue is fully drained first.
    if let Some(origin_handle) = session_handle.read().await.as_ref() {
        origin_handle.flush().await;
    }

    let (messages, _meta, resume_state) = match store.resume_session(target_id) {
        Ok(resolved) => resolved,
        Err(err) => return format!("failed to resume session {target_id}: {err}"),
    };
    let new_handle = match store.open_session(target_id.to_string()) {
        Ok(handle) => Arc::new(handle),
        Err(err) => {
            return format!("failed to reopen session {target_id} for continued appends: {err}")
        }
    };

    *transcript.lock().await = messages;
    *last_known_usage.lock().unwrap() = resume_state.last_known_usage;
    // F-003: re-seed turn_index from fold's real next_turn_index (NOT
    // index.jsonl's turn_count, which can be missing/stale for a session
    // absent from the index -- see F-008 above).
    *turn_index.lock().unwrap() = resume_state.next_turn_index;
    *session_handle.write().await = Some(new_handle);

    format!("Resumed {target_id}; continuing from its context")
}

/// Ticket 39 (rollback-command): the decision + mutation logic behind
/// `/rollback [turn]`. Mirrors `handle_resume_command`'s shape -- kept as
/// its own testable async fn so its branches can be exercised directly
/// against a real `SessionStore`/`CheckpointStore` and real temp-dir
/// fixtures without a full PTY round-trip for every case.
///
/// `arg` is everything after `/rollback` trimmed (may be empty for the bare
/// `/rollback` command). An empty `arg` defaults to `turn_index - 1` -- the
/// most recently submitted turn ("undo the last turn"); if no turns have
/// been submitted yet (`turn_index == 0`), this is an error, not a
/// mutation.
///
/// PRD decision 4: restores every captured pre-image at turn indices >= the
/// target, in reverse-chronological order, via
/// `CheckpointStore::rollback_to`; appends a `Rollback` record; then
/// truncates the running in-memory transcript to the target turn's
/// boundary by re-reading and re-folding the session's own log (now
/// including the just-appended `Rollback` record) via
/// `store.resume_session` -- the SAME mechanism `handle_resume_command`'s
/// `--yes` path already uses to swap in a session's folded transcript,
/// reused here rather than reimplemented, since `fold`'s `Rollback`
/// handling (ticket 33) already IS "truncate the working output to
/// turn_index <= target". `last_known_usage` is restored from that same
/// re-fold, consistent with the truncated transcript.
///
/// `turn_index` itself is deliberately left untouched -- per `fold`'s own
/// doc comment, a `Rollback` record does not rewind `next_turn_index`, so a
/// later genuinely-new turn keeps incrementing from where it left off
/// (matching `resume_session`'s replay of a post-rollback log, which
/// assigns the next NEW `Turn` record the next sequential index regardless
/// of the intervening `Rollback` record).
///
/// Validates `arg` before any mutation: a non-numeric target, or a target
/// that isn't a real prior turn (`>= turn_index`, the count of turns
/// submitted so far), returns an error string and touches nothing. Also a
/// no-op (returns an error string, no mutation) if no session is currently
/// active, matching `capture_checkpoint_if_granted_diff`'s degraded-startup
/// handling elsewhere in this file.
async fn handle_rollback_command(
    data_dir: &std::path::Path,
    store: &rokr_session::SessionStore,
    transcript: &tokio::sync::Mutex<Vec<rokr_core::Message>>,
    session_handle: &tokio::sync::RwLock<Option<Arc<rokr_session::SessionHandle>>>,
    last_known_usage: &std::sync::Mutex<Option<rokr_core::Usage>>,
    turn_index: &std::sync::Mutex<usize>,
    arg: &str,
) -> String {
    let current_turn_index = *turn_index.lock().unwrap();

    let trimmed = arg.trim();
    let target: usize = if trimmed.is_empty() {
        match current_turn_index.checked_sub(1) {
            Some(target) => target,
            None => return "no turns to roll back".to_string(),
        }
    } else {
        match trimmed.parse::<usize>() {
            Ok(target) => target,
            Err(_) => return format!("invalid turn: {trimmed:?}"),
        }
    };

    if target >= current_turn_index {
        return format!(
            "turn {target} is out of range (only turns 0..{current_turn_index} exist)"
        );
    }

    let session_handle_guard = session_handle.read().await;
    let Some(active_handle) = session_handle_guard.as_ref() else {
        return "cannot roll back: no session is currently active".to_string();
    };
    let session_id = active_handle.session_id().to_string();

    let checkpoint_store = rokr_session::CheckpointStore::open(data_dir, &session_id);
    let touched = match checkpoint_store.rollback_to(target) {
        Ok(touched) => touched,
        Err(err) => return format!("rollback failed, no changes applied: {err}"),
    };

    active_handle.append_rollback(target);
    active_handle.flush().await;

    let (messages, _meta, resume_state) = match store.resume_session(&session_id) {
        Ok(resolved) => resolved,
        Err(err) => {
            return format!(
                "rollback restored {} file(s) but failed to re-fold the transcript: {err}",
                touched.len()
            )
        }
    };
    *transcript.lock().await = messages;
    *last_known_usage.lock().unwrap() = resume_state.last_known_usage;

    format!(
        "Rolled back to turn {target}; restored {} file(s).",
        touched.len()
    )
}

/// Resolves the central data directory for session persistence:
/// `$XDG_DATA_HOME/rokr` if `XDG_DATA_HOME` is set and non-empty,
/// otherwise `$HOME/.local/share/rokr`. Mirrors
/// `rokr_config::default_config_dir`'s exact resolution pattern (ticket 34:
/// persist-new-sessions — PRD decision "Central storage, not per-project":
/// all session data lives under `$XDG_DATA_HOME/rokr/`, not inside the
/// project being worked on).
fn default_data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
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
/// without pulling in a new dependency for this ticket.
fn now_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Resolves `name` to a concrete backend and writes it into the shared
/// active-provider state under a single write lock (ticket 29: `/model`;
/// F-003: one lock, one write, no second lock to keep in sync -- see
/// `SharedProvider`'s doc comment).
///
/// F-005: resolution itself now routes through the SAME
/// `auth::resolve_auth` + `factory::build_provider` path startup uses,
/// rather than `AnyProvider::from_name` -- `from_name` can only ever
/// construct an API-key-backed provider from env vars, so a user
/// authenticated only via a stored OAuth token (no
/// `ROKR_ANTHROPIC_API_KEY` env var) could never switch to the anthropic
/// backend via `/model` under the old code. Also preserves the
/// currently-active `RetryPolicy` (read off the current provider snapshot
/// before the switch) instead of resetting to `RetryPolicy::default()` on
/// every switch.
async fn set_active_provider(
    active_provider: &tokio::sync::RwLock<rokr_provider::ResilientProvider<rokr_provider::AnyProvider>>,
    config_dir: &std::path::Path,
    name: &str,
) -> Result<(), String> {
    let current_policy = active_provider.read().await.policy();

    // Scoped so `token_store` (a `Box<dyn TokenStore>`, not `Send`) is
    // dropped before the `.write().await` below -- otherwise it would stay
    // live across that await point and make this whole future not `Send`,
    // which `rokr_tui::run`'s `command` bound requires.
    let resolved_auth = {
        let token_store = rokr_provider::auth::default_token_store(config_dir);
        rokr_provider::auth::resolve_auth(
            None,
            token_store.as_ref(),
            rokr_provider::anthropic::ENV_API_KEY,
        )
    };

    let built = rokr_provider::build_provider(Some(name), resolved_auth, current_policy)?;

    *active_provider.write().await = built.resilient;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rokr_core::{Message, Provider, Role};
    use rokr_session::UsageRecord;

    /// Serializes tests below that mutate process-global env vars
    /// (`ROKR_ANTHROPIC_*`, `ROKR_AUTH_FORCE_FILE_STORE`), mirroring the
    /// same `ENV_GUARD` convention already used in `rokr-provider`'s own
    /// test modules (`lib.rs`, `factory.rs`).
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Creates a fresh, uniquely-named directory under the system temp dir,
    /// mirroring `subagent.rs`'s own `unique_temp_dir` test helper.
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rokr-main-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn running_transcript_accumulates_turns() {
        let mut transcript: Vec<Message> = Vec::new();

        accumulate_user_turn(&mut transcript, "first prompt".to_string());
        transcript.push(Message::assistant_text("first reply"));

        accumulate_user_turn(&mut transcript, "second prompt".to_string());
        transcript.push(Message::assistant_text("second reply"));

        assert_eq!(transcript.len(), 4);

        assert_eq!(transcript[0].role, Role::User);
        assert_eq!(transcript[0].text(), "first prompt");

        assert_eq!(transcript[1].role, Role::Assistant);
        assert_eq!(transcript[1].text(), "first reply");

        assert_eq!(transcript[2].role, Role::User);
        assert_eq!(transcript[2].text(), "second prompt");

        assert_eq!(transcript[3].role, Role::Assistant);
        assert_eq!(transcript[3].text(), "second reply");
    }

    /// F-003 (single-lock refactor): `set_active_provider` now writes
    /// through exactly ONE lock (`SharedProvider`'s doc comment) instead of
    /// two kept in sync by hand. F-005: the requested backend now resolves
    /// via `set_active_provider`'s own `auth::resolve_auth` +
    /// `factory::build_provider` call, not `AnyProvider::from_name`
    /// directly -- proven here via env-var-backed anthropic credentials
    /// (no OAuth token stored). The companion OAuth-only case is
    /// `model_command_switches_to_anthropic_via_stored_oauth_token_without_api_key_env_var`
    /// below. `ResilientProvider` exposes no accessor to its wrapped inner
    /// provider (deliberately -- see `resilience.rs`), so the proof the
    /// switch actually landed is a real `.send()` against a mock server
    /// only the Anthropic backend would ever reach (the untouched `initial`
    /// OpenAI provider points at an unreachable `http://initial.invalid`
    /// and would error, not succeed).
    #[tokio::test]
    async fn model_command_switches_active_provider_to_requested_backend() {
        let _lock = ENV_GUARD.lock().unwrap();

        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&mock_server)
            .await;

        let config_dir = unique_temp_dir("model-switch-env");
        std::env::set_var(rokr_provider::auth::ENV_FORCE_FILE_STORE, "1");
        std::env::set_var(rokr_provider::anthropic::ENV_BASE_URL, mock_server.uri());
        std::env::set_var(rokr_provider::anthropic::ENV_MODEL, "claude-3-5-sonnet-20241022");
        std::env::set_var(rokr_provider::anthropic::ENV_API_KEY, "test-key");

        let initial = rokr_provider::AnyProvider::OpenAi(rokr_provider::OpenAiProvider::new(
            "http://initial.invalid",
            "gpt-4o-mini",
            "initial-key",
        ));
        let active_provider =
            tokio::sync::RwLock::new(rokr_provider::ResilientProvider::new(initial));

        let result = set_active_provider(&active_provider, &config_dir, "anthropic").await;

        std::env::remove_var(rokr_provider::auth::ENV_FORCE_FILE_STORE);
        std::env::remove_var(rokr_provider::anthropic::ENV_BASE_URL);
        std::env::remove_var(rokr_provider::anthropic::ENV_MODEL);
        std::env::remove_var(rokr_provider::anthropic::ENV_API_KEY);
        let _ = std::fs::remove_dir_all(&config_dir);

        result.expect("switching to anthropic via env-backed credentials should succeed");

        let guard = active_provider.read().await;
        let messages = vec![Message::user_text("hello")];
        guard.send(&messages, &[]).await.expect(
            "the switched-to provider should be the Anthropic backend hitting the mock server",
        );
    }

    /// F-005: `/model anthropic` must succeed for a user authenticated
    /// ONLY via a stored OAuth token, with NO `ROKR_ANTHROPIC_API_KEY` env
    /// var set. Before this fix, `/model` resolved the requested provider
    /// via `AnyProvider::from_name`, which can only ever construct an
    /// API-key-backed provider from env vars -- it has no way to build an
    /// OAuth-backed one -- so this exact scenario failed with a
    /// missing-env-var error every time.
    #[tokio::test]
    async fn model_command_switches_to_anthropic_via_stored_oauth_token_without_api_key_env_var() {
        let _lock = ENV_GUARD.lock().unwrap();

        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&mock_server)
            .await;

        let config_dir = unique_temp_dir("model-switch-oauth");
        std::env::set_var(rokr_provider::auth::ENV_FORCE_FILE_STORE, "1");
        std::env::set_var(rokr_provider::anthropic::ENV_BASE_URL, mock_server.uri());
        std::env::set_var(rokr_provider::anthropic::ENV_MODEL, "claude-3-5-sonnet-20241022");
        // Deliberately absent -- this is the whole point of the test.
        std::env::remove_var(rokr_provider::anthropic::ENV_API_KEY);

        let token_store = rokr_provider::auth::default_token_store(&config_dir);
        rokr_provider::auth::TokenStore::save(
            token_store.as_ref(),
            &rokr_provider::auth::Auth::OAuth {
                access_token: "test-oauth-access-token".to_string(),
                refresh_token: None,
                expires_at: None,
            },
        )
        .expect("saving the stored OAuth token should succeed");

        let initial = rokr_provider::AnyProvider::OpenAi(rokr_provider::OpenAiProvider::new(
            "http://initial.invalid",
            "gpt-4o-mini",
            "initial-key",
        ));
        let active_provider =
            tokio::sync::RwLock::new(rokr_provider::ResilientProvider::new(initial));

        let result = set_active_provider(&active_provider, &config_dir, "anthropic").await;

        std::env::remove_var(rokr_provider::auth::ENV_FORCE_FILE_STORE);
        std::env::remove_var(rokr_provider::anthropic::ENV_BASE_URL);
        std::env::remove_var(rokr_provider::anthropic::ENV_MODEL);
        let _ = std::fs::remove_dir_all(&config_dir);

        result.expect(
            "switching to anthropic via a stored OAuth token, with no ROKR_ANTHROPIC_API_KEY \
             env var set, should succeed",
        );

        let guard = active_provider.read().await;
        let messages = vec![Message::user_text("hello")];
        guard.send(&messages, &[]).await.expect(
            "the switched-to provider should be the OAuth-backed Anthropic backend hitting the \
             mock server",
        );
    }

    /// Ticket 36 scope-amendment: `/resume <id>` reports "no such session"
    /// for an id absent from the index, and "already on session" for the
    /// currently active session's own id -- neither mutates the transcript.
    #[tokio::test]
    async fn resume_command_reports_no_such_session_and_already_on_current_without_mutating_transcript(
    ) {
        let dir = unique_temp_dir("resume-command-lookup");
        let store = rokr_session::SessionStore::open(&dir);

        let current_handle = store
            .create_session()
            .expect("create_session should succeed for the current session");
        current_handle.append_header(
            1,
            current_handle.session_id().to_string(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/current".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        current_handle.flush().await;
        let current_session_id = current_handle.session_id().to_string();

        let transcript = tokio::sync::Mutex::new(vec![Message::user_text("existing turn")]);
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(current_handle)));
        let last_known_usage = std::sync::Mutex::new(None);
        let turn_index = std::sync::Mutex::new(0usize);

        let not_found_reply = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            "nonexistent-session-id",
        )
        .await;
        assert_eq!(not_found_reply, "no such session nonexistent-session-id");
        assert_eq!(
            transcript.lock().await.as_slice(),
            &[Message::user_text("existing turn")],
            "a not-found lookup must not mutate the transcript"
        );

        let already_on_reply = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            &current_session_id,
        )
        .await;
        assert_eq!(already_on_reply, format!("already on session {current_session_id}"));
        assert_eq!(
            transcript.lock().await.as_slice(),
            &[Message::user_text("existing turn")],
            "resuming the currently active session must not mutate the transcript"
        );
    }

    /// Ticket 36 scope-amendment: `/resume <id>` (no `--yes`) against a
    /// DIFFERENT existing session returns the warning naming the exact
    /// confirm command, and does not mutate the transcript.
    #[tokio::test]
    async fn resume_command_without_confirm_flag_returns_warning_without_mutating_transcript() {
        let dir = unique_temp_dir("resume-command-warn");
        let store = rokr_session::SessionStore::open(&dir);

        let current_handle = store
            .create_session()
            .expect("create_session should succeed for the current session");
        current_handle.append_header(
            1,
            current_handle.session_id().to_string(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/current".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        current_handle.flush().await;
        let current_session_id_before = current_handle.session_id().to_string();

        let target_handle = store
            .create_session()
            .expect("create_session should succeed for the target session");
        let target_session_id = target_handle.session_id().to_string();
        target_handle.append_header(
            1,
            target_session_id.clone(),
            "2026-07-20T01:00:00Z".to_string(),
            "/projects/target".to_string(),
            "plan".to_string(),
            "openai".to_string(),
            "gpt-test".to_string(),
        );
        target_handle.append_turn(
            Message::user_text("target session first prompt"),
            UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T01:00:01Z".to_string(),
        );
        target_handle.flush().await;

        let transcript = tokio::sync::Mutex::new(vec![Message::user_text("existing turn")]);
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(current_handle)));
        let last_known_usage = std::sync::Mutex::new(None);
        let turn_index = std::sync::Mutex::new(0usize);

        let reply = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            &target_session_id,
        )
        .await;

        assert!(
            reply.contains(&format!("/resume {target_session_id} --yes")),
            "expected warning to echo the exact confirm command, got: {reply:?}"
        );
        assert!(
            reply.contains("target session first prompt"),
            "expected warning to include the target session's title, got: {reply:?}"
        );
        assert!(
            reply.contains('1'),
            "expected warning to include the target session's turn count, got: {reply:?}"
        );
        assert_eq!(
            transcript.lock().await.as_slice(),
            &[Message::user_text("existing turn")],
            "an unconfirmed warning must not mutate the transcript"
        );
        assert_eq!(
            session_handle
                .read()
                .await
                .as_ref()
                .map(|h| h.session_id().to_string()),
            Some(current_session_id_before),
            "an unconfirmed warning must not repoint the active session handle"
        );
    }

    /// Ticket 36 scope-amendment: `/resume <id> --yes` against a different
    /// existing session replaces the running transcript with that
    /// session's FOLDED output (including collapsing a `Compaction` record
    /// -- proving real `fold()` is used, not a reimplementation), restores
    /// `last_known_usage`, and repoints the active session writer so a
    /// subsequently appended turn lands in the TARGET session's
    /// `session.jsonl`, not the origin's.
    #[tokio::test]
    async fn resume_command_with_confirm_flag_swaps_transcript_restores_usage_and_repoints_writer()
    {
        let dir = unique_temp_dir("resume-command-confirm");
        let store = rokr_session::SessionStore::open(&dir);

        let current_handle = store
            .create_session()
            .expect("create_session should succeed for the current session");
        let current_session_id = current_handle.session_id().to_string();
        current_handle.append_header(
            1,
            current_session_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/current".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        current_handle.flush().await;

        let target_handle = store
            .create_session()
            .expect("create_session should succeed for the target session");
        let target_session_id = target_handle.session_id().to_string();
        target_handle.append_header(
            1,
            target_session_id.clone(),
            "2026-07-20T01:00:00Z".to_string(),
            "/projects/target".to_string(),
            "plan".to_string(),
            "openai".to_string(),
            "gpt-test".to_string(),
        );
        target_handle.append_turn(
            Message::user_text("target turn zero"),
            UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T01:00:01Z".to_string(),
        );
        target_handle.append_turn(
            Message::assistant_text("target turn one"),
            UsageRecord {
                input_tokens: 2,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T01:00:02Z".to_string(),
        );
        target_handle.flush().await;

        // A Compaction record: proves `handle_resume_command` delegates to
        // the real `fold()` (which collapses records up to and including
        // `replaced_through`) rather than just replaying raw Turn records.
        // `SessionHandle::enqueue` is private and this task is scoped to
        // `main.rs` only (no touching `rokr-session`), so the record is
        // hand-appended directly to the on-disk log instead, mirroring
        // `tui_test.rs`'s existing fixture-building convention.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.join("sessions").join(&target_session_id).join("session.jsonl"))
                .expect("target session.jsonl should be appendable");
            let line = serde_json::to_string(&rokr_session::SessionRecord::Compaction {
                summary: "target session compacted summary".to_string(),
                replaced_through: 1,
            })
            .unwrap();
            writeln!(file, "{line}").expect("hand-appending the compaction record should succeed");
        }

        target_handle.append_turn(
            Message::user_text("target turn two after compaction"),
            UsageRecord {
                input_tokens: 3,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T01:00:03Z".to_string(),
        );
        target_handle.flush().await;

        let transcript = tokio::sync::Mutex::new(vec![Message::user_text("origin session turn")]);
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(current_handle)));
        let last_known_usage = std::sync::Mutex::new(None);
        // Seeded to an origin-session-shaped value (not 0) so the assertion
        // below actually proves re-seeding happened, rather than trivially
        // matching a coincidental starting value of 0.
        let turn_index = std::sync::Mutex::new(99usize);

        let reply = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            &format!("{target_session_id} --yes"),
        )
        .await;
        assert_eq!(reply, format!("Resumed {target_session_id}; continuing from its context"));
        assert_eq!(
            *turn_index.lock().unwrap(),
            3,
            "expected turn_index to be re-seeded to the target session's real raw Turn count \
             (ticket 38: checkpoint-pre-images)"
        );

        let (expected_messages, expected_last_usage) = {
            let (messages, _meta, resume_state) = store
                .resume_session(&target_session_id)
                .expect("resume_session should succeed for assembling the expected fixture");
            (messages, resume_state.last_known_usage)
        };
        assert_eq!(transcript.lock().await.as_slice(), expected_messages.as_slice());
        assert!(
            transcript
                .lock()
                .await
                .iter()
                .any(|m| m.text().contains("target session compacted summary")),
            "expected the swapped-in transcript to contain the compaction summary message"
        );
        assert_eq!(*last_known_usage.lock().unwrap(), expected_last_usage);

        let repointed_session_id = session_handle
            .read()
            .await
            .as_ref()
            .map(|handle| handle.session_id().to_string());
        assert_eq!(repointed_session_id, Some(target_session_id.clone()));

        // Writer-repoint proof: append a NEW turn through the repointed
        // handle and confirm it lands in the TARGET session's
        // session.jsonl, not the origin's.
        {
            let guard = session_handle.read().await;
            let handle = guard.as_ref().expect("session_handle should be repointed to target");
            handle.append_turn(
                Message::user_text("post-jump new turn"),
                UsageRecord {
                    input_tokens: 4,
                    output_tokens: 4,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                "2026-07-20T01:00:04Z".to_string(),
            );
            handle.flush().await;
        }

        let target_contents = std::fs::read_to_string(
            dir.join("sessions").join(&target_session_id).join("session.jsonl"),
        )
        .expect("target session.jsonl should exist");
        assert!(
            target_contents.contains("post-jump new turn"),
            "expected the post-jump turn to be appended to the TARGET session's log, got: {target_contents:?}"
        );

        let origin_contents = std::fs::read_to_string(
            dir.join("sessions").join(&current_session_id).join("session.jsonl"),
        )
        .expect("origin session.jsonl should still exist");
        assert!(
            !origin_contents.contains("post-jump new turn"),
            "the post-jump turn must NOT appear in the ORIGIN session's log, got: {origin_contents:?}"
        );
    }

    /// Ticket 38 scope-amendment (F-001, argus review): a turn's tool loop
    /// doing `write` then `edit` on the SAME path within the SAME turn must
    /// only produce ONE `Checkpoint` record, not two with a duplicate
    /// `snapshot_id` -- `CheckpointStore::snapshot`'s first-write-wins
    /// semantics mean the second `capture_checkpoint_if_granted_diff` call
    /// for the same `(turn_index, path)` key writes no new snapshot, so it
    /// must also skip appending a second `Checkpoint` record for it. Calls
    /// `capture_checkpoint_if_granted_diff` directly (mirroring this test
    /// module's existing pattern of exercising other `main.rs` helper fns,
    /// e.g. `handle_resume_command`/`set_active_provider`, without a full
    /// PTY round-trip) twice for the same path within turn_index 0, then
    /// reads `session.jsonl` and counts `Checkpoint` records.
    #[tokio::test]
    async fn capture_checkpoint_if_granted_diff_does_not_append_duplicate_checkpoint_for_repeated_path_in_same_turn(
    ) {
        let dir = unique_temp_dir("checkpoint-capture-duplicate");
        let store = rokr_session::SessionStore::open(&dir);
        let handle = store
            .create_session()
            .expect("create_session should succeed");
        let session_id = handle.session_id().to_string();
        handle.append_header(
            1,
            session_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/checkpoint-dup".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle.flush().await;

        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(handle)));
        let turn_index = std::sync::Mutex::new(0usize);
        let target_path = "/some/project/write-then-edit-target.txt".to_string();

        // First mutation of this turn: a `write` whose pre-image is
        // "original".
        capture_checkpoint_if_granted_diff(
            Some((target_path.clone(), "original".to_string())),
            &dir,
            &session_handle,
            &turn_index,
        )
        .await;
        // Second mutation of the SAME path within the SAME turn: an `edit`
        // whose "old" is already the post-write content -- must be a no-op,
        // not a second Checkpoint record.
        capture_checkpoint_if_granted_diff(
            Some((target_path.clone(), "intermediate-post-write-content".to_string())),
            &dir,
            &session_handle,
            &turn_index,
        )
        .await;

        {
            let guard = session_handle.read().await;
            guard
                .as_ref()
                .expect("session_handle should still be Some")
                .flush()
                .await;
        }

        let session_jsonl_contents =
            std::fs::read_to_string(dir.join("sessions").join(&session_id).join("session.jsonl"))
                .expect("session.jsonl should exist");
        let checkpoint_records: Vec<rokr_session::SessionRecord> = session_jsonl_contents
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
            .filter(|record| matches!(record, rokr_session::SessionRecord::Checkpoint { .. }))
            .collect();

        assert_eq!(
            checkpoint_records.len(),
            1,
            "expected exactly one Checkpoint record for two mutations of the same path within \
             the same turn, got: {checkpoint_records:?}"
        );
    }

    /// F-003 (argus review, phase-5-session-management): `turn_index` for a
    /// resumed session must be seeded from `fold`'s own real
    /// `next_turn_index` (via `ResumeState`), not from a separate
    /// `store.list_sessions()` lookup against `sessions/index.jsonl` --
    /// which can be missing an entry entirely for a session that
    /// legitimately exists on disk. Builds a session's `session.jsonl`
    /// directly (bypassing `create_session`/`append_header` entirely) so
    /// `sessions/index.jsonl` is NEVER created, mirroring
    /// `resume_command_with_confirm_flag_swaps_transcript_restores_usage_and_repoints_writer`'s
    /// hand-appended-fixture convention.
    #[tokio::test]
    async fn resolve_resumed_session_seeds_turn_index_from_fold_when_no_index_entry_exists() {
        let dir = unique_temp_dir("resolve-resumed-no-index");
        let session_id = "01HANDBUILTNOINDEXSEED".to_string();
        let session_dir = dir.join("sessions").join(&session_id);
        std::fs::create_dir_all(&session_dir).expect("failed to create session dir fixture");

        let mut records = vec![rokr_session::SessionRecord::Header {
            schema_version: 1,
            session_id: session_id.clone(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            project_path: "/projects/no-index".to_string(),
            agent_tier: "build".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
        }];
        for i in 0..3u64 {
            records.push(rokr_session::SessionRecord::Turn {
                message: Message::user_text(format!("turn {i}")),
                usage: UsageRecord {
                    input_tokens: i + 1,
                    output_tokens: i + 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                timestamp: format!("2026-07-20T00:00:0{i}Z"),
            });
        }
        let contents = records
            .iter()
            .map(|record| serde_json::to_string(record).expect("serialize SessionRecord"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(session_dir.join("session.jsonl"), contents)
            .expect("failed to write session.jsonl fixture");

        assert!(
            !dir.join("sessions").join("index.jsonl").exists(),
            "sessions/index.jsonl must not exist for this test to be meaningful"
        );

        let store = rokr_session::SessionStore::open(&dir);
        let (_, _, _, turn_index) = match resolve_resumed_session(&store, session_id.clone()) {
            Ok(resolved) => resolved,
            Err(_) => panic!(
                "resolve_resumed_session should succeed against a hand-built fixture log even \
                 when sessions/index.jsonl has no entry for it"
            ),
        };

        assert_eq!(
            turn_index, 3,
            "expected turn_index to be seeded from fold's real next_turn_index (3 raw Turn \
             records), not a stale/missing index lookup (F-003), got: {turn_index}"
        );
    }

    /// F-003 (jump path) + F-008 (argus review): `/resume <id> --yes` must
    /// still succeed, and correctly re-seed `turn_index` from the real
    /// fold, even when the TARGET session has no `sessions/index.jsonl`
    /// entry at all -- proving the fallback path (`store.resume_session`
    /// used to both confirm existence and derive `next_turn_index`) rather
    /// than failing with "no such session" just because the index lookup
    /// came up empty.
    #[tokio::test]
    async fn resume_command_yes_falls_back_to_resume_session_and_seeds_turn_index_when_target_absent_from_index(
    ) {
        let dir = unique_temp_dir("resume-command-no-index-target");
        let store = rokr_session::SessionStore::open(&dir);

        let current_handle = store
            .create_session()
            .expect("create_session should succeed for the current session");
        current_handle.append_header(
            1,
            current_handle.session_id().to_string(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/current".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        current_handle.flush().await;

        let target_session_id = "01HANDBUILTNOINDEXTARGET".to_string();
        let target_session_dir = dir.join("sessions").join(&target_session_id);
        std::fs::create_dir_all(&target_session_dir)
            .expect("failed to create target session dir fixture");
        let mut records = vec![rokr_session::SessionRecord::Header {
            schema_version: 1,
            session_id: target_session_id.clone(),
            created_at: "2026-07-20T01:00:00Z".to_string(),
            project_path: "/projects/target".to_string(),
            agent_tier: "plan".to_string(),
            provider: "openai".to_string(),
            model: "gpt-test".to_string(),
        }];
        for i in 0..3u64 {
            records.push(rokr_session::SessionRecord::Turn {
                message: Message::user_text(format!("target turn {i}")),
                usage: UsageRecord {
                    input_tokens: i + 1,
                    output_tokens: i + 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                timestamp: format!("2026-07-20T01:00:0{i}Z"),
            });
        }
        let contents = records
            .iter()
            .map(|record| serde_json::to_string(record).expect("serialize SessionRecord"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(target_session_dir.join("session.jsonl"), contents)
            .expect("failed to write target session.jsonl fixture");

        let transcript = tokio::sync::Mutex::new(vec![Message::user_text("current turn")]);
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(current_handle)));
        let last_known_usage = std::sync::Mutex::new(None);
        let turn_index = std::sync::Mutex::new(0usize);

        let reply = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            &format!("{target_session_id} --yes"),
        )
        .await;

        assert_eq!(
            reply,
            format!("Resumed {target_session_id}; continuing from its context"),
            "expected the jump to succeed via the resume_session fallback despite the target \
             session having no sessions/index.jsonl entry (F-008), got: {reply:?}"
        );
        assert_eq!(
            *turn_index.lock().unwrap(),
            3,
            "expected turn_index to be seeded from the real fold (F-003), not a nonexistent \
             index entry"
        );
    }

    /// F-007 (argus review): `/resume <id> --yes` must flush the ORIGIN
    /// session's writer BEFORE resolving/opening the TARGET, so an append
    /// enqueued just before the jump (fire-and-forget, no explicit flush)
    /// is guaranteed to have reached disk before a LATER jump back to that
    /// same origin session re-reads its log. Uses the default
    /// current-thread `#[tokio::test]` flavor deliberately: determinism
    /// here depends on there being no other OS thread that could race the
    /// writer task independently of this test's own explicit yield points.
    #[tokio::test]
    async fn jump_flushes_origin_session_before_swapping_so_a_later_rejump_sees_the_pending_append(
    ) {
        let dir = unique_temp_dir("resume-command-flush-before-jump");
        let store = rokr_session::SessionStore::open(&dir);

        let handle_a = store
            .create_session()
            .expect("create_session should succeed for session A");
        let session_a_id = handle_a.session_id().to_string();
        handle_a.append_header(
            1,
            session_a_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/a".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle_a.flush().await;

        let handle_b = store
            .create_session()
            .expect("create_session should succeed for session B");
        let session_b_id = handle_b.session_id().to_string();
        handle_b.append_header(
            1,
            session_b_id.clone(),
            "2026-07-20T01:00:00Z".to_string(),
            "/projects/b".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle_b.flush().await;

        let pending_text = "pending unflushed turn in session A before jump";
        // Deliberately fire-and-forget: no `.flush().await` here, simulating
        // a write enqueued just before the jump.
        handle_a.append_turn(
            Message::user_text(pending_text),
            UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T00:00:01Z".to_string(),
        );

        let transcript = tokio::sync::Mutex::new(Vec::new());
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(handle_a)));
        let last_known_usage = std::sync::Mutex::new(None);
        let turn_index = std::sync::Mutex::new(0usize);

        let reply_to_b = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            &format!("{session_b_id} --yes"),
        )
        .await;
        assert_eq!(
            reply_to_b,
            format!("Resumed {session_b_id}; continuing from its context")
        );

        let reply_to_a = handle_resume_command(
            &store,
            &transcript,
            &session_handle,
            &last_known_usage,
            &turn_index,
            &format!("{session_a_id} --yes"),
        )
        .await;
        assert_eq!(
            reply_to_a,
            format!("Resumed {session_a_id}; continuing from its context")
        );

        assert!(
            transcript
                .lock()
                .await
                .iter()
                .any(|m| m.text().contains(pending_text)),
            "expected the pending unflushed turn appended to session A before the first jump \
             to have been flushed to session A's session.jsonl by handle_resume_command's \
             F-007 origin-flush, and therefore visible after re-jumping back to A"
        );
    }
}
