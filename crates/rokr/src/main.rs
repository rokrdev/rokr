use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use rokr_app::cli::{completions_script, AuthAction, Cli, Command, ReportFormat};
use rokr_app::{
    append_compaction_record, default_data_dir, log_observational_hook_outcome,
    matching_hook_entries, now_timestamp, run_hook_entry, select_mode, AgentTier, Mode, ResumeMode,
    SessionRunner, SharedProvider,
};
use rokr_core::ExecutableTool;

#[tokio::main]
async fn main() -> ExitCode {
    // Ticket 52 (clap-and-sessionrunner-extraction): argument parsing is now
    // owned by `clap` (see `rokr_app::cli`). `Cli::parse()` handles
    // `--version` / `--help` and usage errors itself (printing and exiting),
    // replacing the hand-rolled `--version` / `auth login` / `parse_agent_tier`
    // match this function used to open with.
    let cli = Cli::parse();

    match cli.command {
        // Ticket 31 (oauth-pkce-login): runs the PKCE login flow and exits
        // rather than entering the TUI. Now the `auth login` clap subcommand
        // rather than a hand-matched `[a, b]` argv pair.
        Some(Command::Auth {
            action: AuthAction::Login,
        }) => {
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
        // Ticket 53 (shell-completions-subcommand): prints the requested
        // shell's completion script to stdout and exits, rather than
        // entering the TUI -- the same "run and exit" shape as `auth login`
        // above.
        Some(Command::Completions { shell }) => {
            println!("{}", completions_script(shell));
            ExitCode::SUCCESS
        }
        // Ticket 58 (eval-case-runner-and-deterministic-assertions): runs
        // every eval case file in `cases_dir` and exits -- the same
        // "run and exit, no TUI" shape as `auth login`/`completions` above.
        // A thin adapter: all the orchestration (fresh fixture dir per
        // case, fresh headless session, assertion checking) lives in
        // `rokr_eval::run_eval`; this arm just prints its per-case report
        // and maps the outcome to an exit code (0 iff every case passed and
        // the run itself didn't hit a whole-run error, 1 otherwise).
        //
        // Ticket 60 (eval-report-json-and-ci-gate): `report`/`pass_threshold`
        // are new. `report == Some(ReportFormat::Json)` branches to the
        // aggregate, threshold-gated report instead -- built entirely by
        // `rokr_eval::report::build_report` (this arm just prints it and
        // returns its `exit_code()`). `None`/`Some(ReportFormat::Text)`
        // (the ticket's "unchanged" path) falls through to the exact same
        // per-case PASS/FAIL printing and any-case-failed exit code as
        // before this ticket.
        Some(Command::Eval {
            cases_dir,
            dangerously_skip_permissions,
            report,
            pass_threshold,
        }) => match rokr_eval::run_eval(&cases_dir, dangerously_skip_permissions).await {
            Ok(outcomes) => {
                if matches!(report, Some(ReportFormat::Json)) {
                    let report = rokr_eval::report::build_report(&outcomes, pass_threshold);
                    match serde_json::to_string(&report) {
                        Ok(json) => println!("{json}"),
                        Err(err) => eprintln!("failed to serialize report: {err}"),
                    }
                    report.exit_code()
                } else {
                    let mut any_failed = false;
                    for outcome in &outcomes {
                        if outcome.passed {
                            println!("PASS {}", outcome.name);
                        } else {
                            any_failed = true;
                            println!("FAIL {}", outcome.name);
                            if let Some(err) = &outcome.run_error {
                                println!("  run error: {err}");
                            }
                            for assertion in &outcome.assertion_outcomes {
                                if !assertion.passed {
                                    println!(
                                        "  assertion failed: {} ({})",
                                        assertion.description, assertion.detail
                                    );
                                }
                            }
                        }
                    }
                    if any_failed {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::SUCCESS
                    }
                }
            }
            Err(err) => {
                eprintln!("eval failed: {err}");
                ExitCode::FAILURE
            }
        },
        // No subcommand: launch the TUI. `--agent` defaults to `Plan` when
        // absent (the old `parse_agent_tier([])` behavior);
        // `--resume` / `--continue` resolve via `Cli::resume_mode`.
        None => {
            // Ticket 54 (headless-print-mode-text-output): `-p`/`--print`
            // runs a single prompt headless -- no TUI -- and exits, checked
            // before any of the TUI-specific startup below so a
            // scripted/piped invocation never waits on a terminal.
            // `Cli::print`'s value of `-` reads the prompt from stdin
            // instead of literally being the prompt text (see
            // `rokr_app::headless::select_mode`).
            if let Mode::Headless(prompt) = select_mode(cli.print.as_deref(), std::io::stdin()) {
                return rokr_app::headless::run(&cli, prompt).await;
            }

            let agent = cli.agent.unwrap_or(AgentTier::Plan);
            let resume_mode = cli.resume_mode();

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

            let mut system_prompt =
                match rokr_config::read_agent_prompt(&config_dir, agent.prompt_name()) {
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

            // Ticket 61 (memory-file-loading-user-and-project-scope):
            // one-time, unconditional, side-effect-free read of memory for
            // both scopes rokr supports today -- user-scope AGENTS.md
            // (under `config_dir`, always loaded when present) and
            // project-scope AGENTS.md/CLAUDE.md (under the current working
            // directory, `load_project_context`'s existing fallback) --
            // folded into the system prompt as separate labeled segments,
            // user-then-project order, alongside the active agent tier's
            // prompt. Not a tool, not permission-gated — this is how the
            // system prompt is built, not a model-invoked action.
            if let Some(cwd) = cwd.as_deref() {
                for segment in rokr_config::load_memory(&config_dir, cwd) {
                    system_prompt.push_str("\n\n");
                    system_prompt.push_str(&format!("# {}\n", segment.label));
                    system_prompt.push_str(&segment.content);
                }
            }

            // Ticket 50 (hooks-remaining-events-and-config): loaded once
            // here (user-scope config only -- `rokr_config::load_or_init_default`
            // never reads a project-local file, so this can never be
            // influenced by a cloned repo, per
            // docs/adr/0012-hooks-execution-trust-model.md's trust
            // boundary) and shared via `Arc` with `submit` below (a clone
            // moved in) and with the `SessionEnd` firing point after
            // `rokr_tui::run` returns (the original, still in scope here).
            let hooks_config: Arc<std::collections::HashMap<String, Vec<rokr_config::HookEntry>>> =
                Arc::new(config.hooks.clone());
            // A separate clone for `submit` to move wholesale into its own
            // `move` closure below -- `hooks_config` itself stays alive in
            // this scope for the `SessionEnd` firing point after
            // `rokr_tui::run` returns, near the bottom of this function.
            let hooks_config_for_submit = hooks_config.clone();
            // Ticket 51 (mcp-hooks-introspection): a third clone, for
            // `command`'s own `move` closure below (built alongside the
            // other `command_*` clones near `command_provider`), so
            // `/hooks` can list every configured hook without disturbing
            // either of the two clones above.
            let command_hooks_config = hooks_config.clone();
            // Ticket 57 (cost-command-and-headless-reporting): `config.model_pricing`
            // wasn't threaded into `command`'s closure before this ticket --
            // `/cost`/`/cost --all` need it to resolve a session's model
            // string into a dollar rate, mirroring `hooks_config`'s own
            // load-once-and-`Arc`-clone pattern immediately above.
            let model_pricing: Arc<std::collections::HashMap<String, rokr_config::ModelPricing>> =
                Arc::new(config.model_pricing.clone());
            let command_model_pricing = model_pricing.clone();

            // `SessionStart` (PRD "Hooks"; architect decision: "SessionStart
            // at startup"): fires once, here, before the TUI ever renders.
            // Every configured hook's exit-0 stdout is concatenated and
            // folded into the system prompt exactly like AGENTS.md project
            // context above -- "follow existing context-assembly patterns"
            // (architect decision). A hook that fails (non-blocking) or
            // exits 2 never blocks startup: `SessionStart` has no veto
            // semantics (there is nothing yet to veto), so both outcomes
            // just log a one-line notice via `log_observational_hook_outcome`
            // and startup continues.
            for entry in matching_hook_entries(&hooks_config, "SessionStart", None) {
                match run_hook_entry(entry, &rokr_hooks::HookPayload::SessionStart).await {
                    rokr_hooks::HookResult::Success { stdout } => {
                        if !stdout.trim().is_empty() {
                            system_prompt.push_str("\n\n");
                            system_prompt.push_str(stdout.trim());
                        }
                    }
                    other => log_observational_hook_outcome("SessionStart", &other),
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
            let (provider, provider_name, model_name): (
                Result<SharedProvider, String>,
                String,
                String,
            ) = match built {
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
                            // Schema v2 (architect ruling, phase-5): a
                            // brand-new session's Header records schema
                            // version 2 (Turn now carries a `messages` array,
                            // not a singular `message`). Existing v1 logs are
                            // read-shimmed, never rewritten.
                            handle.append_header(
                                2,
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

            // Ticket 45 (mcp-config-and-lifecycle): each enabled stdio
            // server configured in user-scope rokr.json is spawned on its
            // own background tokio task inside rokr-mcp
            // (`rokr_mcp::spawn_stdio_server`), strictly off the render
            // path -- first paint never waits on any MCP server (PRD "MCP
            // lifecycle"). A server's tools become available once it
            // reports ready (`McpServerHandle::tools`); a server whose init
            // fails contributes zero tools and surfaces a one-line status
            // notice instead of crashing or blocking the rest of rokr.
            // Replaces ticket 44's `ROKR_MCP_SERVER` env-var interim wiring
            // wholesale (see docs/adr/0011-rokr-mcp-crate-boundary.md's
            // Consequences section). PC-1 ruling (supersedes ticket 46's
            // whole-session freeze): each server's contribution instead
            // freezes individually at its own first `Ready`
            // (`McpServerHandle::joined`) -- see `submit`'s
            // `mcp_tools_snapshot` below.
            let (mcp_notice_tx, mcp_notice_rx) = std::sync::mpsc::channel::<String>();
            // Bridges rokr-mcp's plain `String` notices (rokr-mcp depends
            // on rokr-core only -- ADR 0011 -- so it can't know about
            // `rokr_tui::SessionStatus` itself) onto the SAME SessionStatus
            // channel ticket 43 built for context-percent updates, so a
            // degraded-server notice shows up in the header status line
            // without a second render-loop channel. Reads the current
            // last-known context percent (rather than hardcoding 0.0) so a
            // notice arriving after a real turn has already reported usage
            // doesn't stomp that figure back to zero. `mcp_notice_rx.recv()`
            // blocks its thread until a notice arrives (or every sender
            // drops), so this runs via `spawn_blocking` rather than inline
            // in an async task, matching this codebase's existing rule
            // (ADR 0008) that blocking work never runs on an async task
            // that could otherwise be polled on the render thread.
            {
                let status_tx = status_tx.clone();
                let last_known_usage = last_known_usage.clone();
                tokio::task::spawn_blocking(move || {
                    while let Ok(notice) = mcp_notice_rx.recv() {
                        let context_percent = last_known_usage
                            .lock()
                            .unwrap()
                            .map(|usage| {
                                (usage.input_tokens + usage.output_tokens) as f64
                                    / context_window_size as f64
                            })
                            .unwrap_or(0.0);
                        let _ = status_tx.send(rokr_tui::SessionStatus {
                            context_percent,
                            notice: Some(notice),
                        });
                    }
                });
            }

            let mcp_server_handles: Arc<Vec<rokr_mcp::McpServerHandle>> = Arc::new(
                config
                    .mcp
                    .iter()
                    .filter(|(_, server)| server.enabled)
                    .map(|(name, server)| match &server.transport {
                        rokr_config::McpTransport::Stdio(stdio) => rokr_mcp::spawn_stdio_server(
                            name.clone(),
                            stdio.command.clone(),
                            stdio.args.clone(),
                            stdio.env.clone(),
                            mcp_notice_tx.clone(),
                            server.auto_approve.clone(),
                        ),
                        // Ticket 48 (mcp-http-transport, stretch scope):
                        // same lifecycle machinery as the stdio arm above,
                        // via `rokr_mcp::spawn_http_server`.
                        rokr_config::McpTransport::Http(http) => rokr_mcp::spawn_http_server(
                            name.clone(),
                            http.url.clone(),
                            http.headers.clone(),
                            mcp_notice_tx.clone(),
                            server.auto_approve.clone(),
                        ),
                    })
                    .collect(),
            );

            // Ticket 51 (mcp-hooks-introspection): clones for `command`'s
            // own `move` closure below (built alongside the other
            // `command_*` clones near `command_provider`), so `/mcp` and
            // `/mcp reconnect` can list/reconnect servers. `command_mcp_configs`
            // is the raw, unfiltered `config.mcp` (not just the handles
            // above) because a server with `enabled: false` never gets a
            // handle at all -- this is `/mcp`'s only way to report it as
            // "disabled" rather than silently omitting it.
            let command_mcp_server_handles = mcp_server_handles.clone();
            let command_mcp_configs: Arc<
                std::collections::HashMap<String, rokr_config::McpServerConfig>,
            > = Arc::new(config.mcp.clone());

            // Ticket 48 (mcp-http-transport), PRD "MCP permissions": an
            // HTTP server's origin is a data-exfiltration signal, so it's
            // surfaced in the permission-prompt text
            // (`format_tool_call_permission_text` below) for a
            // `PermissionPayload::ToolCall` whose server is HTTP-transport.
            // Built once here (server name -> URL, HTTP servers only)
            // rather than adding an `origin` field to
            // `PermissionPayload::ToolCall` itself (`rokr-core`) -- that
            // type's own doc comment (ticket 47) already anticipated this
            // exact bridge-side lookup as the intended seam, so this is
            // the smallest change that satisfies it.
            let mcp_http_origins: Arc<std::collections::HashMap<String, String>> = Arc::new(
                config
                    .mcp
                    .iter()
                    .filter_map(|(name, server)| match &server.transport {
                        rokr_config::McpTransport::Http(http) => {
                            Some((name.clone(), http.url.clone()))
                        }
                        rokr_config::McpTransport::Stdio(_) => None,
                    })
                    .collect(),
            );

            // PC-1 ruling (supersedes ticket 46's "the MCP tool set is
            // frozen for the lifetime of a session" whole-session
            // `OnceLock` freeze): each server's tool contribution now
            // freezes individually, in `McpServerHandle::joined`, at that
            // server's own first `Ready` (or a later explicit `/mcp
            // reconnect` success) -- see `rokr_mcp::snapshot_tools`'s doc
            // comment. `submit` below therefore calls `snapshot_tools`
            // FRESH every turn rather than caching it in a `OnceLock`:
            // that's cheap (no I/O, just iterating handles' already-frozen
            // `joined` state) and always safe, since the underlying
            // per-server freeze is what actually provides the "no
            // turn-to-turn mutation of an already-joined server" guarantee
            // -- a session simply sees whichever servers have joined AS OF
            // that turn, in deterministic sorted order.
            let submit_mcp_notice_tx = mcp_notice_tx.clone();

            let runner = SessionRunner {
                provider,
                transcript,
                system_prompt,
                repo_map,
                last_known_usage,
                config_dir,
                session_handle,
                turn_index,
                data_dir,
                status_tx,
                mcp_server_handles,
                mcp_notice_tx: submit_mcp_notice_tx,
                mcp_http_origins,
                hooks_config: hooks_config_for_submit,
                agent,
                context_window_size,
                auto_compact_threshold,
            };

            // Ticket 52 (clap-and-sessionrunner-extraction): the submit-and-run
            // orchestration that used to be inlined in this closure now lives in
            // `rokr_app::SessionRunner::run_submission`. This closure is a thin
            // adapter -- `rokr_tui::run` hands it each Enter-press's prompt text
            // and a fresh `PermissionHandle`, which it forwards straight into the
            // runner (identical behavior; a pure move).
            let submit = move |input: String, permission: rokr_tui::PermissionHandle| {
                runner.run_submission(input, permission)
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
                let mcp_server_handles = command_mcp_server_handles.clone();
                let mcp_configs = command_mcp_configs.clone();
                let hooks_config = command_hooks_config.clone();
                let model_pricing = command_model_pricing.clone();
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

                    // Ticket 51 (mcp-hooks-introspection): `/mcp reconnect
                    // <server>` is the only MUTATING introspection command
                    // (resets ONE named server's retry state -- never
                    // touches config on disk). Checked before the bare
                    // `/mcp` match arm below, same `strip_prefix` shape as
                    // `/search ` above.
                    if let Some(server_name) = input.strip_prefix("/mcp reconnect ") {
                        let server_name = server_name.trim();
                        return match mcp_server_handles
                            .iter()
                            .find(|handle| handle.name == server_name)
                        {
                            Some(handle) => match mcp_reconnect_gate(&handle.status()) {
                                Ok(()) => {
                                    handle.reconnect();
                                    format!("Reconnecting MCP server '{server_name}'.")
                                }
                                // F-002: PC-1/F-002's "auto-join is
                                // once-per-server, re-entry only via
                                // explicit reconnect" guard only makes
                                // sense if reconnect itself is gated to
                                // `Degraded` servers -- reconnecting an
                                // already-`Ready`/`Starting` server would
                                // spin up a second concurrent lifecycle
                                // task for no reason (F-002's race the
                                // generation fence guards against).
                                Err(state_word) => format!(
                                    "server '{server_name}' is {state_word}; reconnect only \
                                     applies to degraded servers"
                                ),
                            },
                            None => format!("no such MCP server: '{server_name}'"),
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
                                    // RULING 2: persist a Compaction record.
                                    // `/compact` never submits a turn itself,
                                    // so `turn_index` is NOT incremented here
                                    // -- its current value is the raw turn
                                    // count (same derivation as the auto path:
                                    // the retained tail turn is
                                    // `raw_turn_count - 1`, the summary
                                    // replaces through `raw_turn_count - 2`).
                                    let raw_turn_count = *turn_index.lock().unwrap();
                                    append_compaction_record(
                                        &session_handle,
                                        &compacted,
                                        raw_turn_count,
                                    )
                                    .await;
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
                        "/mcp" => format_mcp_listing(&mcp_configs, &mcp_server_handles),
                        "/hooks" => format_hooks_listing(&hooks_config),
                        // Ticket 57: `/cost` folds the CURRENT session's own
                        // `UsageRecord`s (a fresh raw read off disk, not the
                        // in-memory `last_known_usage`, which only carries
                        // the LAST turn's figure -- see
                        // `fold_session_usage_and_model`'s doc comment) into
                        // a token-by-type breakdown, cache-hit rate, and
                        // dollar estimate against its own model's pricing.
                        "/cost" => {
                            match session_handle.read().await.as_ref().map(|handle| {
                                handle.session_id().to_string()
                            }) {
                                Some(session_id) => {
                                    let records = read_session_records(&data_dir, &session_id);
                                    let (usage, model) = fold_session_usage_and_model(&records);
                                    let pricing_entry = model_pricing
                                        .get(&model)
                                        .map(model_pricing_to_pricing_entry);
                                    let cost_usd = rokr_core::pricing::calculate_cost(
                                        usage,
                                        pricing_entry.as_ref(),
                                    );
                                    format_cost_breakdown(usage, cost_usd)
                                }
                                None => "No active session.".to_string(),
                            }
                        }
                        // Ticket 57: `/cost --all` extends the same fold
                        // across every session `SessionStore::list_sessions`
                        // knows about, pricing each session against its OWN
                        // model before summing dollar totals (see
                        // `format_cost_all_summary`'s doc comment for why).
                        "/cost --all" => {
                            format_cost_all_summary(&data_dir, &store, &model_pricing).await
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

            let run_result = rokr_tui::run(
                submit,
                command,
                prompt_history,
                on_history_append,
                status_rx,
            )
            .await;

            // `SessionEnd` (ticket 50, hooks-remaining-events-and-config;
            // PRD "Hooks", architect decision: "SessionEnd at exit"): fires
            // once, here, after the TUI has already returned control
            // (regardless of whether it exited cleanly, hit a non-tty, or
            // errored) and before this process' own exit code is decided.
            // Fire-and-observe like `Stop`/`PostToolUse` -- nothing it does
            // can change `run_result` below. `run_hook_entry`'s existing
            // timeout contract (default 60s, per-entry override) is what
            // keeps this from delaying process exit unboundedly; it is NOT
            // itself further time-boxed beyond that, matching every other
            // hook call site in this file.
            for entry in matching_hook_entries(&hooks_config, "SessionEnd", None) {
                let result = run_hook_entry(entry, &rokr_hooks::HookPayload::SessionEnd).await;
                log_observational_hook_outcome("SessionEnd", &result);
            }

            match run_result {
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

/// F-002: gates `/mcp reconnect <server>` to `Degraded` servers only --
/// reconnecting a `Starting` or already-`Ready` server would spin up a
/// second, redundant lifecycle task racing the one already running (the
/// exact scenario F-002's generation fence in `rokr-mcp` exists to survive,
/// but there's no reason to invite it from a command whose whole point is
/// "this server is broken, try again"). `Ok(())` means reconnect may
/// proceed; `Err(word)` carries the state word for the refusal message.
/// A free function (rather than inlined at the one call site) so it's
/// unit-testable without spinning up a real `McpServerHandle`/lifecycle
/// task.
fn mcp_reconnect_gate(status: &rokr_mcp::McpServerStatus) -> Result<(), &'static str> {
    match status {
        rokr_mcp::McpServerStatus::Degraded { .. } => Ok(()),
        rokr_mcp::McpServerStatus::Starting => Err("starting"),
        rokr_mcp::McpServerStatus::Ready => Err("connected"),
    }
}

#[cfg(test)]
mod mcp_reconnect_gate_tests {
    use super::*;

    /// F-002 done-when: reconnect is refused for a `Ready` (connected)
    /// server -- only a `Degraded` server may reconnect.
    #[test]
    fn reconnect_on_connected_server_is_refused() {
        let result = mcp_reconnect_gate(&rokr_mcp::McpServerStatus::Ready);
        assert_eq!(
            result,
            Err("connected"),
            "expected reconnect to be refused for an already-connected server"
        );
    }

    #[test]
    fn reconnect_on_starting_server_is_refused() {
        let result = mcp_reconnect_gate(&rokr_mcp::McpServerStatus::Starting);
        assert_eq!(result, Err("starting"));
    }

    #[test]
    fn reconnect_on_degraded_server_is_permitted() {
        let result = mcp_reconnect_gate(&rokr_mcp::McpServerStatus::Degraded {
            reason: "boom".to_string(),
        });
        assert_eq!(result, Ok(()));
    }
}

/// Ticket 51 (mcp-hooks-introspection), `/mcp`: renders one line per
/// configured MCP server (PRD "connection state (connected/degraded/
/// disabled)"), sorted by name for deterministic output. `configs` is the
/// raw, unfiltered `config.mcp` -- a server with `enabled: false` never
/// gets a `McpServerHandle` at all (see `mcp_server_handles`'s
/// construction in `main`), so `configs` is what lets this report
/// "disabled" for it instead of silently omitting it; `handles` supplies
/// live status/tools for every server that DID get spawned. State/field
/// values are printed as single `key=value` tokens with no internal
/// spaces (`state=connected`, not `state: connected`) so they survive
/// ratatui's cell-diff rendering (unchanged cells, including blank spaces,
/// aren't redrawn) as one contiguous run of bytes in a raw terminal
/// capture.
fn format_mcp_listing(
    configs: &std::collections::HashMap<String, rokr_config::McpServerConfig>,
    handles: &[rokr_mcp::McpServerHandle],
) -> String {
    if configs.is_empty() {
        return "No MCP servers configured.".to_string();
    }

    let mut names: Vec<&String> = configs.keys().collect();
    names.sort();

    names
        .into_iter()
        .map(|name| {
            let server_config = &configs[name];
            let transport = match server_config.transport {
                rokr_config::McpTransport::Stdio(_) => "stdio",
                rokr_config::McpTransport::Http(_) => "http",
            };

            if !server_config.enabled {
                return format!("{name} | transport={transport} | state=disabled | tools=[]");
            }

            let handle = handles.iter().find(|handle| &handle.name == name);
            let (state, tools) = match handle.map(|handle| handle.status()) {
                Some(rokr_mcp::McpServerStatus::Starting) => {
                    ("state=starting".to_string(), Vec::new())
                }
                Some(rokr_mcp::McpServerStatus::Ready) => {
                    // PC-1 ruling: reflects this server's live JOIN state
                    // (`joined`, frozen at its own first Ready or a later
                    // explicit reconnect success) rather than its raw live
                    // `tools()` -- this is what's actually contributing to
                    // every session's assembled snapshot right now, so
                    // there's no separate "restart to pick up tools" note
                    // needed: whatever's listed here is already active.
                    let tool_names = handle
                        .expect("handle present for a Ready status")
                        .joined()
                        .unwrap_or_default()
                        .iter()
                        .map(|tool| tool.name().to_string())
                        .collect::<Vec<_>>();
                    ("state=connected".to_string(), tool_names)
                }
                Some(rokr_mcp::McpServerStatus::Degraded { reason }) => {
                    (format!("state=degraded (reason: {reason})"), Vec::new())
                }
                None => ("state=unknown".to_string(), Vec::new()),
            };

            format!(
                "{name} | transport={transport} | {state} | tools=[{}]",
                tools.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ticket 51 (mcp-hooks-introspection): the six hook event names actually
/// wired to a hook call site somewhere in this file (`main`'s
/// `SessionStart` firing point; `submit`'s `UserPromptSubmit`,
/// `PreToolUse`, `PostToolUse`, `Stop`; and the `SessionEnd` firing point
/// after `rokr_tui::run` returns) -- matches every literal event string
/// passed to `matching_hook_entries` elsewhere in this file exactly. Used
/// by `format_hooks_listing` below to flag a hook entry configured under
/// any OTHER key (a typo, or one of the events the PRD defers to Phase 7
/// -- `SubagentStop`, `PreCompact`, `Notification`) as `state=inactive`:
/// real config `/hooks` should still list, just flagged since nothing will
/// ever run it.
const SUPPORTED_HOOK_EVENTS: [&str; 6] = [
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "SessionEnd",
];

/// Ticket 51 (mcp-hooks-introspection), `/hooks`: renders one line per
/// configured hook entry, sorted by event name for deterministic output.
/// See `SUPPORTED_HOOK_EVENTS`'s doc comment for what `state=active` /
/// `state=inactive` means; see `format_mcp_listing`'s doc comment for why
/// fields are printed as single `key=value` tokens.
fn format_hooks_listing(
    hooks_config: &std::collections::HashMap<String, Vec<rokr_config::HookEntry>>,
) -> String {
    if hooks_config.is_empty() {
        return "No hooks configured.".to_string();
    }

    let mut events: Vec<&String> = hooks_config.keys().collect();
    events.sort();

    let mut lines = Vec::new();
    for event in events {
        let active = SUPPORTED_HOOK_EVENTS.contains(&event.as_str());
        let state = if active {
            "state=active"
        } else {
            "state=inactive"
        };
        for entry in &hooks_config[event] {
            let matcher = entry.matcher.as_deref().unwrap_or("*");
            let timeout_ms = entry
                .timeout_ms
                .unwrap_or(rokr_hooks::DEFAULT_TIMEOUT.as_millis() as u64);
            let blocking = entry
                .blocking
                .map(|blocking| blocking.to_string())
                .unwrap_or_else(|| "default".to_string());
            lines.push(format!(
                "{event} | matcher={matcher} | command={} | timeout_ms={timeout_ms} | \
                 blocking={blocking} | {state}",
                entry.command
            ));
        }
    }
    lines.join("\n")
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
        Ok(entries) => entries
            .into_iter()
            .find(|entry| entry.session_id == target_id),
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
/// PRD decision 4: restores every captured pre-image at turn indices
/// strictly greater than the target turn, in reverse-chronological order, via
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
///
/// RULING 3 (architect ruling, phase-5): additionally REFUSES (before any
/// mutation, exact message `"cannot roll back past the last compaction —
/// earlier turns were summarized"`) when the target is at or before the last
/// `Compaction` record's `replaced_through` -- those earlier turns were
/// summarized away and cannot be un-folded, so this is a hard refusal, not a
/// partial rollback.
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
        return format!("turn {target} is out of range (only turns 0..{current_turn_index} exist)");
    }

    let session_handle_guard = session_handle.read().await;
    let Some(active_handle) = session_handle_guard.as_ref() else {
        return "cannot roll back: no session is currently active".to_string();
    };
    let session_id = active_handle.session_id().to_string();

    // F-012 (re-review, phase-5): flush the active writer BEFORE reading the
    // last compaction boundary below. `append_compaction`/`append_rollback`
    // are fire-and-forget onto an async mpsc writer task, so a `Compaction`
    // record enqueued earlier in THIS session (a `/compact` the user just ran,
    // or an auto-compaction from the turn that just completed) may still be
    // sitting unflushed in the channel when `last_compaction_replaced_through`
    // reads the file. Without this flush the guard's disk read can miss that
    // record and let `/rollback` proceed into territory a soon-to-land
    // Compaction was about to summarize away -- defeating the guard entirely.
    active_handle.flush().await;

    // RULING 3 (architect ruling, phase-5): rolling back INTO or BEFORE
    // compacted territory is a hard refusal -- the summarized-away turns
    // cannot be un-folded. Checked BEFORE any mutation (no filesystem
    // restore, no Rollback record appended). A lookup error is treated
    // conservatively as "cannot verify" and also refuses rather than risking
    // a rollback past a compaction boundary.
    match store.last_compaction_replaced_through(&session_id) {
        Ok(Some(boundary)) if target <= boundary => {
            return "cannot roll back past the last compaction — earlier turns were summarized"
                .to_string();
        }
        Ok(_) => {}
        Err(err) => {
            return format!("rollback failed, no changes applied: {err}");
        }
    }

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
    active_provider: &tokio::sync::RwLock<
        rokr_provider::ResilientProvider<rokr_provider::AnyProvider>,
    >,
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

/// Ticket 57 (cost-command-and-headless-reporting): sums every `Turn`
/// record's `UsageRecord` across `records` into one `rokr_core::Usage`,
/// plus the model string off the `Header` record (empty if none is
/// present). Deliberately NOT `rokr_session::fold` -- that function only
/// keeps the LAST known usage per session (overwriting on every `Turn`, see
/// its own doc comment), which is exactly wrong for `/cost`'s
/// token-by-type breakdown, which needs the SUM across the whole session's
/// history. A session's model can't change mid-session in today's schema
/// (no per-`Turn` model field, same limitation `SessionIndexEntry::last_model`
/// already documents), so a single `Header`-derived string is enough.
fn fold_session_usage_and_model(records: &[rokr_session::SessionRecord]) -> (rokr_core::Usage, String) {
    let mut usage = rokr_core::Usage::default();
    let mut model = String::new();
    for record in records {
        match record {
            rokr_session::SessionRecord::Header {
                model: header_model, ..
            } => {
                model = header_model.clone();
            }
            rokr_session::SessionRecord::Turn {
                usage: turn_usage, ..
            } => {
                let turn_usage: rokr_core::Usage = (*turn_usage).into();
                usage.input_tokens += turn_usage.input_tokens;
                usage.output_tokens += turn_usage.output_tokens;
                usage.cache_read_tokens += turn_usage.cache_read_tokens;
                usage.cache_write_tokens += turn_usage.cache_write_tokens;
            }
            rokr_session::SessionRecord::Compaction { .. }
            | rokr_session::SessionRecord::Rollback { .. }
            | rokr_session::SessionRecord::Checkpoint { .. } => {}
        }
    }
    (usage, model)
}

/// Reads and parses one session's raw `session.jsonl` log directly off disk
/// (`<data_dir>/sessions/<session_id>/session.jsonl`), the same
/// read-every-line-skip-unparseable pattern `rokr_session::SessionStore`'s
/// own `resume_session`/`search` use internally -- replicated here rather
/// than reused because neither of those methods returns the raw
/// `Vec<SessionRecord>` `/cost` needs (they return already-folded output),
/// and adding a new public method to `rokr-session` is out of this ticket's
/// files-touched scope. A missing/unreadable file yields an empty `Vec`
/// (mirrors `SessionStore::list_sessions`'s own "not found" -> empty
/// handling) rather than surfacing an error `/cost` would have nowhere
/// good to show.
fn read_session_records(
    data_dir: &std::path::Path,
    session_id: &str,
) -> Vec<rokr_session::SessionRecord> {
    let session_jsonl_path = data_dir.join("sessions").join(session_id).join("session.jsonl");
    let contents = std::fs::read_to_string(&session_jsonl_path).unwrap_or_default();
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
        .collect()
}

/// Ticket 57: field-copy bridge from `rokr-config`'s `ModelPricing` (the
/// on-disk-configurable pricing table entry) to `rokr-core::pricing`'s
/// `PricingEntry` (what `calculate_cost` actually takes) -- the two crates
/// have no dependency edge on each other (see `ModelPricing`'s own doc
/// comment), so there's no `From`/`Into` between them to reuse. Same four
/// fields, same names, same types; duplicated in `crates/rokr-app/src/headless.rs`
/// for the same reason (that file already depends on both crates too).
fn model_pricing_to_pricing_entry(
    pricing: &rokr_config::ModelPricing,
) -> rokr_core::pricing::PricingEntry {
    rokr_core::pricing::PricingEntry {
        input_price_per_token: pricing.input_price_per_token,
        output_price_per_token: pricing.output_price_per_token,
        cache_read_price_per_token: pricing.cache_read_price_per_token,
        cache_write_price_per_token: pricing.cache_write_price_per_token,
    }
}

/// Formats `/cost`'s token-by-type breakdown, cache-hit rate, and estimated
/// dollar cost for one already-folded `usage` total. Cache-hit-rate formula
/// mirrors `rokr-provider/src/anthropic.rs`'s own per-call calculation
/// exactly (fraction of total prompt tokens -- input + cache-read +
/// cache-write -- served from cache).
fn format_cost_breakdown(usage: rokr_core::Usage, cost_usd: f64) -> String {
    let total_prompt_tokens =
        usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
    let cache_hit_rate = if total_prompt_tokens > 0 {
        usage.cache_read_tokens as f64 / total_prompt_tokens as f64
    } else {
        0.0
    };
    format!(
        "Input tokens: {}\nOutput tokens: {}\nCache-read tokens: {}\nCache-write tokens: {}\nCache hit rate: {:.1}%\nEstimated cost: ${:.4}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        cache_hit_rate * 100.0,
        cost_usd
    )
}

/// `/cost --all`: extends the same fold across EVERY session on disk
/// (`store.list_sessions()`, PRD decision 2's index rather than a directory
/// scan). Token counts are summed freely across sessions, but each
/// session's dollar cost is computed against ITS OWN model's pricing before
/// summing the dollar totals -- sessions can have different models/prices,
/// so applying one session's rate to another's tokens would misprice it.
async fn format_cost_all_summary(
    data_dir: &std::path::Path,
    store: &rokr_session::SessionStore,
    model_pricing: &std::collections::HashMap<String, rokr_config::ModelPricing>,
) -> String {
    let entries = match store.list_sessions() {
        Ok(entries) => entries,
        Err(err) => return format!("failed to list sessions: {err}"),
    };
    if entries.is_empty() {
        return "No sessions found.".to_string();
    }

    let mut total_usage = rokr_core::Usage::default();
    let mut total_cost_usd = 0.0;
    for entry in &entries {
        let records = read_session_records(data_dir, &entry.session_id);
        let (usage, model) = fold_session_usage_and_model(&records);
        let pricing_entry = model_pricing.get(&model).map(model_pricing_to_pricing_entry);
        total_cost_usd += rokr_core::pricing::calculate_cost(usage, pricing_entry.as_ref());
        total_usage.input_tokens += usage.input_tokens;
        total_usage.output_tokens += usage.output_tokens;
        total_usage.cache_read_tokens += usage.cache_read_tokens;
        total_usage.cache_write_tokens += usage.cache_write_tokens;
    }

    format!(
        "Sessions: {}\n{}",
        entries.len(),
        format_cost_breakdown(total_usage, total_cost_usd)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rokr_app::{
        accumulate_user_turn, capture_checkpoint_if_granted_diff, COMPACTION_SUMMARY_WRAPPER_PREFIX,
    };
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

    /// Ticket 57 (cost-command-and-headless-reporting): `/cost`'s token
    /// totals must be a SUM across every `Turn` record's `UsageRecord`, not
    /// the last-known figure `rokr_session::fold` keeps (that function
    /// overwrites `last_known_usage` on every `Turn`, see its own doc
    /// comment) -- this proves the new, standalone summing fold actually
    /// adds three turns' worth of each of the four token fields together,
    /// and separately resolves the model string off the `Header` record.
    #[test]
    fn cost_command_folds_current_session_usage_records_into_token_and_dollar_summary() {
        let records = vec![
            rokr_session::SessionRecord::Header {
                schema_version: 2,
                session_id: "sess-1".to_string(),
                created_at: "0".to_string(),
                project_path: "/tmp/project".to_string(),
                agent_tier: "plan".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
            },
            rokr_session::SessionRecord::Turn {
                messages: vec![],
                usage: UsageRecord {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read_tokens: 10,
                    cache_write_tokens: 5,
                },
                timestamp: "1".to_string(),
            },
            rokr_session::SessionRecord::Turn {
                messages: vec![],
                usage: UsageRecord {
                    input_tokens: 200,
                    output_tokens: 75,
                    cache_read_tokens: 20,
                    cache_write_tokens: 0,
                },
                timestamp: "2".to_string(),
            },
            rokr_session::SessionRecord::Turn {
                messages: vec![],
                usage: UsageRecord {
                    input_tokens: 300,
                    output_tokens: 125,
                    cache_read_tokens: 0,
                    cache_write_tokens: 15,
                },
                timestamp: "3".to_string(),
            },
        ];

        let (usage, model) = fold_session_usage_and_model(&records);

        assert_eq!(usage.input_tokens, 600, "input tokens must be summed across all turns");
        assert_eq!(usage.output_tokens, 250, "output tokens must be summed across all turns");
        assert_eq!(
            usage.cache_read_tokens, 30,
            "cache-read tokens must be summed across all turns"
        );
        assert_eq!(
            usage.cache_write_tokens, 20,
            "cache-write tokens must be summed across all turns"
        );
        assert_eq!(model, "gpt-4o-mini", "model must be resolved off the Header record");
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "msg_test",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&mock_server)
            .await;

        let config_dir = unique_temp_dir("model-switch-env");
        std::env::set_var(rokr_provider::auth::ENV_FORCE_FILE_STORE, "1");
        std::env::set_var(rokr_provider::anthropic::ENV_BASE_URL, mock_server.uri());
        std::env::set_var(
            rokr_provider::anthropic::ENV_MODEL,
            "claude-3-5-sonnet-20241022",
        );
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
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "msg_test",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&mock_server)
            .await;

        let config_dir = unique_temp_dir("model-switch-oauth");
        std::env::set_var(rokr_provider::auth::ENV_FORCE_FILE_STORE, "1");
        std::env::set_var(rokr_provider::anthropic::ENV_BASE_URL, mock_server.uri());
        std::env::set_var(
            rokr_provider::anthropic::ENV_MODEL,
            "claude-3-5-sonnet-20241022",
        );
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
        assert_eq!(
            already_on_reply,
            format!("already on session {current_session_id}")
        );
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
            vec![Message::user_text("target session first prompt")],
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
            vec![Message::user_text("target turn zero")],
            UsageRecord {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            "2026-07-20T01:00:01Z".to_string(),
        );
        target_handle.append_turn(
            vec![Message::assistant_text("target turn one")],
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
                .open(
                    dir.join("sessions")
                        .join(&target_session_id)
                        .join("session.jsonl"),
                )
                .expect("target session.jsonl should be appendable");
            let line = serde_json::to_string(&rokr_session::SessionRecord::Compaction {
                summary: "target session compacted summary".to_string(),
                replaced_through: 1,
            })
            .unwrap();
            writeln!(file, "{line}").expect("hand-appending the compaction record should succeed");
        }

        target_handle.append_turn(
            vec![Message::user_text("target turn two after compaction")],
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
        assert_eq!(
            reply,
            format!("Resumed {target_session_id}; continuing from its context")
        );
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
        assert_eq!(
            transcript.lock().await.as_slice(),
            expected_messages.as_slice()
        );
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
            let handle = guard
                .as_ref()
                .expect("session_handle should be repointed to target");
            handle.append_turn(
                vec![Message::user_text("post-jump new turn")],
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
            dir.join("sessions")
                .join(&target_session_id)
                .join("session.jsonl"),
        )
        .expect("target session.jsonl should exist");
        assert!(
            target_contents.contains("post-jump new turn"),
            "expected the post-jump turn to be appended to the TARGET session's log, got: {target_contents:?}"
        );

        let origin_contents = std::fs::read_to_string(
            dir.join("sessions")
                .join(&current_session_id)
                .join("session.jsonl"),
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
            Some((
                target_path.clone(),
                "intermediate-post-write-content".to_string(),
            )),
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

    /// RULING 2 (architect ruling, phase-5) done-when #1 (direct numeric
    /// derivation): `append_compaction_record` derives `replaced_through =
    /// raw_turn_count - 2` (compaction retains the tail turn at
    /// `raw_turn_count - 1`, summary replaces through the one before it) and
    /// stores the RAW summary text with the `[Earlier conversation summary
    /// ...]` wrapper stripped back off (so `fold` doesn't double-wrap it on
    /// resume). With `raw_turn_count = 3` (turns 0,1,2 submitted; turn 2 the
    /// retained tail), the record's `replaced_through` must be 1.
    #[tokio::test]
    async fn append_compaction_record_derives_replaced_through_and_stores_raw_summary() {
        let dir = unique_temp_dir("append-compaction-derivation");
        let store = rokr_session::SessionStore::open(&dir);
        let handle = store
            .create_session()
            .expect("create_session should succeed");
        let session_id = handle.session_id().to_string();
        handle.append_header(
            2,
            session_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/compaction".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle.flush().await;

        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(handle)));

        // `compact_transcript`'s output: compacted[0] is the summary with the
        // wrapper already baked in; compacted[1..] is the untouched tail turn.
        let wrapped_summary =
            format!("{COMPACTION_SUMMARY_WRAPPER_PREFIX}RawSummaryTextForDerivationTest");
        let compacted = vec![
            Message::user_text(wrapped_summary),
            Message::user_text("the untouched tail turn prompt"),
        ];

        append_compaction_record(&session_handle, &compacted, 3).await;
        session_handle.read().await.as_ref().unwrap().flush().await;

        let contents =
            std::fs::read_to_string(dir.join("sessions").join(&session_id).join("session.jsonl"))
                .expect("session.jsonl should exist");
        let compaction_records: Vec<rokr_session::SessionRecord> = contents
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
            .filter(|record| matches!(record, rokr_session::SessionRecord::Compaction { .. }))
            .collect();
        assert_eq!(
            compaction_records.len(),
            1,
            "expected exactly one Compaction record"
        );
        match &compaction_records[0] {
            rokr_session::SessionRecord::Compaction {
                summary,
                replaced_through,
            } => {
                assert_eq!(
                    *replaced_through, 1,
                    "raw_turn_count 3 -> replaced_through 1 (tail turn 2 retained)"
                );
                assert_eq!(
                    summary, "RawSummaryTextForDerivationTest",
                    "the stored summary must have the wrapper prefix stripped back off"
                );
            }
            other => panic!("expected a Compaction record, got: {other:?}"),
        }
    }

    /// RULING 2 defensive guard: `append_compaction_record` appends NOTHING
    /// when `raw_turn_count < 2` (`checked_sub(2)` underflows) -- there'd be
    /// nothing before the tail to summarize, so no `Compaction` record should
    /// be written rather than panicking or underflowing.
    #[tokio::test]
    async fn append_compaction_record_skips_when_fewer_than_two_turns() {
        let dir = unique_temp_dir("append-compaction-guard");
        let store = rokr_session::SessionStore::open(&dir);
        let handle = store
            .create_session()
            .expect("create_session should succeed");
        let session_id = handle.session_id().to_string();
        handle.append_header(
            2,
            session_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/compaction-guard".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        handle.flush().await;

        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(handle)));
        let compacted = vec![Message::user_text(format!(
            "{COMPACTION_SUMMARY_WRAPPER_PREFIX}should not be stored"
        ))];

        append_compaction_record(&session_handle, &compacted, 1).await;
        session_handle.read().await.as_ref().unwrap().flush().await;

        let contents =
            std::fs::read_to_string(dir.join("sessions").join(&session_id).join("session.jsonl"))
                .expect("session.jsonl should exist");
        let compaction_count = contents
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str::<rokr_session::SessionRecord>(line).ok())
            .filter(|record| matches!(record, rokr_session::SessionRecord::Compaction { .. }))
            .count();
        assert_eq!(
            compaction_count, 0,
            "expected NO Compaction record when raw_turn_count < 2"
        );
    }

    /// RULING 3 done-when #3 (compaction guard): `/rollback` to a target at
    /// or before the last compaction's `replaced_through` is REFUSED with the
    /// exact message and makes NO mutation -- no filesystem restore, no
    /// `Rollback` record appended, transcript/usage/turn_index untouched.
    /// Builds a session whose log holds a `Compaction { replaced_through: 1 }`
    /// then attempts `/rollback` at target 1 (== boundary) and target 0 (<
    /// boundary).
    #[tokio::test]
    async fn rollback_command_refuses_target_at_or_before_last_compaction_without_mutating() {
        let dir = unique_temp_dir("rollback-compaction-guard");
        let store = rokr_session::SessionStore::open(&dir);
        let handle = store
            .create_session()
            .expect("create_session should succeed");
        let session_id = handle.session_id().to_string();
        handle.append_header(
            2,
            session_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/guard".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        // Three turns (indices 0,1,2), then a compaction summarizing through
        // turn 1 (retaining tail turn 2).
        for i in 0..3usize {
            handle.append_turn(
                vec![Message::user_text(format!("turn {i}"))],
                UsageRecord {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                format!("2026-07-20T00:00:0{i}Z"),
            );
        }
        handle.append_compaction("summary through turn 1".to_string(), 1);
        handle.flush().await;

        let log_before =
            std::fs::read_to_string(dir.join("sessions").join(&session_id).join("session.jsonl"))
                .expect("session.jsonl should exist");

        let transcript = tokio::sync::Mutex::new(vec![Message::user_text("live transcript")]);
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(handle)));
        let last_known_usage = std::sync::Mutex::new(None);
        let turn_index = std::sync::Mutex::new(3usize);

        const EXPECTED: &str =
            "cannot roll back past the last compaction — earlier turns were summarized";

        for target in ["1", "0"] {
            let reply = handle_rollback_command(
                &dir,
                &store,
                &transcript,
                &session_handle,
                &last_known_usage,
                &turn_index,
                target,
            )
            .await;
            assert_eq!(
                reply, EXPECTED,
                "expected the exact refusal message for target {target:?}"
            );
        }

        // No mutation: transcript, turn_index, and the on-disk log are all
        // unchanged (no Rollback record appended).
        assert_eq!(
            transcript.lock().await.as_slice(),
            &[Message::user_text("live transcript")],
            "a refused rollback must not mutate the transcript"
        );
        assert_eq!(
            *turn_index.lock().unwrap(),
            3,
            "turn_index must be untouched"
        );
        session_handle.read().await.as_ref().unwrap().flush().await;
        let log_after =
            std::fs::read_to_string(dir.join("sessions").join(&session_id).join("session.jsonl"))
                .expect("session.jsonl should still exist");
        assert_eq!(
            log_before, log_after,
            "a refused rollback must append no Rollback record (log byte-identical)"
        );
    }

    /// F-012 (re-review, phase-5-session-management): the compaction-boundary
    /// guard must also refuse when the last `Compaction` record is still
    /// sitting UNFLUSHED in the writer channel -- e.g. a `/compact` or an
    /// auto-compaction from the just-completed turn whose `append_compaction`
    /// has been enqueued (fire-and-forget) but not yet drained to disk when
    /// `/rollback` runs in the same session. Unlike
    /// `rollback_command_refuses_target_at_or_before_last_compaction_without_mutating`,
    /// which flushes the compaction to disk BEFORE invoking the handler (so the
    /// guard's disk read trivially sees it), this test flushes ONLY the turns
    /// and leaves the final `append_compaction` enqueued-but-unflushed. Unless
    /// the handler flushes the active writer BEFORE reading
    /// `last_compaction_replaced_through`, the guard's disk read misses the
    /// queued record and the rollback wrongly proceeds into summarized
    /// territory -- so this test fails against the pre-fix ordering.
    #[tokio::test]
    async fn rollback_command_refuses_when_compaction_enqueued_but_not_yet_flushed() {
        let dir = unique_temp_dir("rollback-compaction-unflushed");
        let store = rokr_session::SessionStore::open(&dir);
        let handle = store
            .create_session()
            .expect("create_session should succeed");
        let session_id = handle.session_id().to_string();
        handle.append_header(
            2,
            session_id.clone(),
            "2026-07-20T00:00:00Z".to_string(),
            "/projects/guard".to_string(),
            "build".to_string(),
            "anthropic".to_string(),
            "claude-test".to_string(),
        );
        // Three turns (indices 0,1,2), then a compaction summarizing through
        // turn 1 (retaining tail turn 2).
        for i in 0..3usize {
            handle.append_turn(
                vec![Message::user_text(format!("turn {i}"))],
                UsageRecord {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                format!("2026-07-20T00:00:0{i}Z"),
            );
        }
        // Flush ONLY the turns to disk; then enqueue the Compaction record but
        // deliberately do NOT flush it -- it stays fire-and-forget in the
        // writer channel, exactly the state a just-run /compact or an
        // auto-compaction from the turn that just completed leaves behind when
        // /rollback is invoked in the same session before the writer drains.
        handle.flush().await;
        handle.append_compaction("summary through turn 1".to_string(), 1);

        let transcript = tokio::sync::Mutex::new(vec![Message::user_text("live transcript")]);
        let session_handle = tokio::sync::RwLock::new(Some(Arc::new(handle)));
        let last_known_usage = std::sync::Mutex::new(None);
        let turn_index = std::sync::Mutex::new(3usize);

        const EXPECTED: &str =
            "cannot roll back past the last compaction — earlier turns were summarized";

        for target in ["1", "0"] {
            let reply = handle_rollback_command(
                &dir,
                &store,
                &transcript,
                &session_handle,
                &last_known_usage,
                &turn_index,
                target,
            )
            .await;
            assert_eq!(
                reply, EXPECTED,
                "expected the exact refusal message for target {target:?} even though the \
                 Compaction record was still unflushed when the guard ran"
            );
        }

        // No mutation: transcript and turn_index untouched. Once the writer is
        // fully drained, the log must carry the Compaction record (the guard's
        // own flush made it durable) but NO Rollback record -- a refused
        // rollback appends nothing.
        assert_eq!(
            transcript.lock().await.as_slice(),
            &[Message::user_text("live transcript")],
            "a refused rollback must not mutate the transcript"
        );
        assert_eq!(
            *turn_index.lock().unwrap(),
            3,
            "turn_index must be untouched"
        );
        session_handle.read().await.as_ref().unwrap().flush().await;
        let log_after =
            std::fs::read_to_string(dir.join("sessions").join(&session_id).join("session.jsonl"))
                .expect("session.jsonl should exist");
        assert!(
            log_after.contains(r#""type":"Compaction""#),
            "the Compaction record must be durable after the guard flushes it"
        );
        assert!(
            !log_after.contains(r#""type":"Rollback""#),
            "a refused rollback must append no Rollback record"
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
                messages: vec![Message::user_text(format!("turn {i}"))],
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
                messages: vec![Message::user_text(format!("target turn {i}"))],
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
    async fn jump_flushes_origin_session_before_swapping_so_a_later_rejump_sees_the_pending_append()
    {
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
            vec![Message::user_text(pending_text)],
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
