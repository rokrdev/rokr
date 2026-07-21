//! Ticket 54 (headless-print-mode-text-output): headless (`-p`/`--print`)
//! mode selection. `select_mode` decides whether this invocation should
//! launch the TUI (the `-p`/`--print` flag absent) or run headless against a
//! single prompt (the flag present) -- and, when the flag's value is the
//! literal `-`, resolves that prompt by reading stdin instead of using `-`
//! itself as the prompt text.
//!
//! Ticket 55 (headless-output-formats-and-permission-mode): this module now
//! also owns the full headless orchestration ([`run`]) -- output-format
//! dispatch (`text`/`json`/`stream-json`), permission-mode dispatch
//! (`deny`/`accept-edits`/`bypass`, see [`build_permission_requester`]), and
//! the exit-code contract (0 success, 1 agent/runtime error, 2 CLI misuse).
//! Ticket 54 deliberately skipped session persistence, hooks, MCP, and all
//! of the above; this ticket wires every one of those in for real, mirroring
//! the TUI startup path in `crates/rokr/src/main.rs`.

/// Whether this invocation should launch the TUI or run headless against a
/// single prompt. See [`select_mode`].
pub enum Mode {
    /// No `-p`/`--print` flag: launch the TUI, unchanged from before this
    /// ticket.
    Tui,
    /// `-p`/`--print <prompt>` was given; run headless against this prompt
    /// text with no terminal UI.
    Headless(String),
}

/// Resolves the parsed `--print` flag value into a [`Mode`]. `print` is
/// `Cli::print` from `crate::cli` (`None` when the flag is absent). A value
/// of the literal `-` reads the prompt from `stdin` instead of using `-`
/// itself as the prompt text -- `stdin` is injected (rather than always
/// reading the real `std::io::stdin()`) so this is testable without a real
/// terminal or piped process.
pub fn select_mode(print: Option<&str>, mut stdin: impl std::io::Read) -> Mode {
    match print {
        None => Mode::Tui,
        Some("-") => {
            let mut buf = String::new();
            let _ = stdin.read_to_string(&mut buf);
            Mode::Headless(buf.trim_end().to_string())
        }
        Some(prompt) => Mode::Headless(prompt.to_string()),
    }
}

/// The permission surface headless drives `SessionRunner::run_submission`
/// with, dispatching per-request on the `--permission-mode` this run was
/// built with (see [`build_permission_requester`]). `denied` is shared
/// (`Arc<AtomicBool>`) rather than owned per-clone: `SessionRunner`
/// internally clones the permission requester at least twice (once for the
/// parent's own gated calls, once for `subagent::SubagentTool`, see
/// `runner.rs`'s `subagent_permission` clone), and every clone must report
/// into the SAME flag so `crate::headless::run` can observe "did ANY gated
/// call get denied during this run" after `run_submission` returns --
/// `run_tool_loop` does not abort on a denial (it appends an error
/// `ToolResult` and loops again, see that function's own doc comment), so
/// this flag is the only way headless can tell a denial happened at all.
#[derive(Clone)]
pub struct HeadlessPermissionRequester {
    mode: crate::cli::PermissionMode,
    denied: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HeadlessPermissionRequester {
    /// Whether any gated tool call this requester (or one of its clones)
    /// handled was denied. Checked by `crate::headless::run` after
    /// `run_submission` returns to decide `subtype: error_permission`.
    pub fn denied(&self) -> bool {
        self.denied.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl crate::runner::PermissionRequester for HeadlessPermissionRequester {
    fn request(
        &self,
        request: rokr_tui::PermissionRequest,
    ) -> impl std::future::Future<Output = bool> + Send {
        let mode = self.mode;
        let denied = self.denied.clone();
        async move {
            // Assumption (not stated explicitly in the ticket, see this
            // ticket's report): `AcceptEdits` grants only a write/edit
            // (`Diff`) call -- there's no human present in headless to
            // approve a `bash` command or an MCP tool call interactively,
            // so both stay denied under `AcceptEdits` exactly as under
            // `Deny`. `rokr_tui::PermissionDetail` collapses
            // `PermissionPayload::Command` and `PermissionPayload::ToolCall`
            // into the same `Text` variant (see that type's doc comment),
            // so this can only distinguish "a diff" from "everything else",
            // which is exactly the distinction this policy needs.
            let granted = match mode {
                crate::cli::PermissionMode::Deny => false,
                crate::cli::PermissionMode::Bypass => true,
                crate::cli::PermissionMode::AcceptEdits => {
                    matches!(request.detail, rokr_tui::PermissionDetail::Diff { .. })
                }
            };
            if !granted {
                denied.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            granted
        }
    }
}

/// Builds the headless permission surface for `mode`, enforcing the one
/// rule that can't be expressed in `PermissionMode`'s shape alone: `Bypass`
/// (grant every gated call unconditionally) additionally requires the
/// operator to have passed the explicit, unsafely-named
/// `--dangerously-skip-permissions` flag -- `--permission-mode bypass` by
/// itself is not enough. `Deny` and `AcceptEdits` never require that flag,
/// since neither ever grants everything unconditionally. A pure function
/// (no I/O, no process exit) so it's directly unit-testable without
/// spawning the `rokr` binary.
/// Ticket 58 (eval-case-runner-and-deterministic-assertions): the CLI-misuse
/// (exit 2, e.g. `--permission-mode bypass` without
/// `--dangerously-skip-permissions`) vs. any other bootstrap failure (exit
/// 1) distinction [`run`] used to collapse straight into an `ExitCode`
/// inline. Pulled out as its own error type so [`run_result_object`] (which
/// has no `ExitCode` to return -- callers like `rokr-eval` need the
/// distinction, not a process exit code) can still report it.
pub enum BootstrapError {
    /// A flag/parameter combination that's invalid before any session,
    /// provider, or hook setup even starts (maps to exit code 2 in [`run`]).
    CliMisuse(String),
    /// Any other bootstrap failure -- config load, prompt read, etc. (maps
    /// to `ExitCode::FAILURE` in [`run`]).
    Other(String),
}

/// The result of one headless turn: the [`crate::result_schema::ResultObject`]
/// plus the full message transcript that produced it (needed for
/// `--output-format stream-json`'s event replay in [`run`]; `rokr-eval`
/// callers of [`run_result_object`] only need `result_object`).
pub struct HeadlessRunOutcome {
    pub result_object: crate::result_schema::ResultObject,
    pub transcript: Vec<rokr_core::Message>,
}

/// Drives ONE headless run end to end: session bootstrap (mirroring the TUI
/// startup path in `crates/rokr/src/main.rs`), permission-requester
/// selection by `cli.permission_mode`, real `hooks_config` wiring (from
/// `rokr_config::load_or_init_default()`'s `config.hooks`, not the empty
/// map ticket 54 hardcoded), output-format dispatch, and the exit-code
/// contract (0 success, 1 agent/runtime error, 2 CLI misuse). `main.rs`'s
/// `run_headless` is now a thin adapter that just threads flags in and
/// returns this function's `ExitCode` -- see this ticket's report for why
/// the real orchestration lives here instead ("real orchestration ...
/// belongs in rokr-app ... fully unit-testable without the binary").
///
/// Ticket 58: this is now itself a thin wrapper around
/// [`run_result_object`] -- resolve the three flag defaults + the real cwd,
/// call it, then print per `output_format` and map to an `ExitCode`. The
/// bootstrap/run/result-object-construction logic lives in
/// `run_result_object` so `rokr-eval` can drive one isolated headless turn
/// per eval case (explicit pinned agent tier/permission mode + a fresh temp
/// fixture dir as `cwd`) without going through a `Cli` struct or this
/// process's real `std::env::current_dir()`.
pub async fn run(cli: &crate::cli::Cli, prompt: String) -> std::process::ExitCode {
    let permission_mode = cli
        .permission_mode
        .unwrap_or(crate::cli::PermissionMode::Deny);
    let output_format = cli.output_format.unwrap_or(crate::cli::OutputFormat::Text);
    let agent = cli.agent.unwrap_or(crate::cli::AgentTier::Plan);
    let cwd = std::env::current_dir().ok();

    let outcome = match run_result_object(
        agent,
        permission_mode,
        cli.dangerously_skip_permissions,
        prompt,
        cwd,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(BootstrapError::CliMisuse(err)) => {
            eprintln!("{err}");
            return std::process::ExitCode::from(2);
        }
        Err(BootstrapError::Other(err)) => {
            eprintln!("{err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result_object = outcome.result_object;

    match output_format {
        // Unchanged from ticket 54: only the final assistant text, on
        // stdout on success / stderr on failure, no framing.
        crate::cli::OutputFormat::Text => match result_object.subtype {
            crate::result_schema::Subtype::Success => println!("{}", result_object.result),
            _ => eprintln!("{}", result_object.result),
        },
        // JSON output is always on stdout, success or failure -- a
        // machine-parseable result is exactly what a caller needs from a
        // failed run too (which subtype, which message), so it's never
        // routed to stderr the way plain text framing is.
        crate::cli::OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&result_object)
                    .expect("ResultObject serialization cannot fail")
            );
        }
        // Post-hoc, not live (documented simplification -- see this
        // ticket's report and docs/adr/0013-headless-output-schema.md):
        // there is no live event-streaming hook available without touching
        // `runner.rs`/`rokr-core` (out of this ticket's scope), so this
        // replays the transcript `run_submission` already produced --
        // genuine per-message data, just delivered after the fact rather
        // than incrementally.
        crate::cli::OutputFormat::StreamJson => {
            for message in &outcome.transcript {
                let event_type = match message.role {
                    rokr_core::Role::User => "user",
                    rokr_core::Role::Assistant => "assistant",
                    rokr_core::Role::System => "system",
                };
                let event = serde_json::json!({ "type": event_type, "message": message });
                println!(
                    "{}",
                    serde_json::to_string(&event).expect("event serialization cannot fail")
                );
            }
            println!(
                "{}",
                serde_json::to_string(&result_object)
                    .expect("ResultObject serialization cannot fail")
            );
        }
    }

    result_object.exit_code()
}

/// Ticket 58: the bootstrap-and-run logic [`run`] used to have inlined,
/// taking explicit params instead of `&crate::cli::Cli` +
/// `std::env::current_dir()` so a caller (headless `main.rs`'s [`run`]
/// wrapper, or `rokr-eval`'s per-case driver) can pin exactly which agent
/// tier / permission mode / cwd this ONE turn runs with. Byte-for-byte the
/// same bootstrap/run/result-object construction `run` used to do inline;
/// only the parameter source changed (explicit args here, `&Cli` there).
pub async fn run_result_object(
    agent: crate::cli::AgentTier,
    permission_mode: crate::cli::PermissionMode,
    dangerously_skip_permissions: bool,
    prompt: String,
    cwd: Option<std::path::PathBuf>,
) -> Result<HeadlessRunOutcome, BootstrapError> {
    let started_at = std::time::Instant::now();

    // CLI misuse (exit 2, per the ticket's exit-code contract): caught
    // before any session/provider/hook setup below, mirroring how clap
    // itself exits 2 for a bad flag combination it CAN express structurally
    // -- `--permission-mode bypass` without `--dangerously-skip-permissions`
    // is a combination clap's own `ValueEnum`/`bool` flags can't express,
    // so it's checked here instead.
    let permission = build_permission_requester(permission_mode, dangerously_skip_permissions)
        .map_err(BootstrapError::CliMisuse)?;

    let config = rokr_config::load_or_init_default()
        .map_err(|err| BootstrapError::Other(format!("failed to initialize config: {err}")))?;

    let config_dir = rokr_config::default_config_dir();

    let mut system_prompt = rokr_config::read_agent_prompt(&config_dir, agent.prompt_name())
        .map_err(|err| BootstrapError::Other(format!("failed to read agent prompt: {err}")))?;

    if let Some(cwd) = cwd.as_deref() {
        for segment in rokr_config::load_memory(&config_dir, cwd) {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&format!("# {}\n", segment.label));
            system_prompt.push_str(&segment.content);
        }
    }

    let repo_map: Option<String> = cwd.as_deref().map(rokr_tools::repo_map::generate);

    let token_store = rokr_provider::auth::default_token_store(&config_dir);
    let resolved_auth = rokr_provider::auth::resolve_auth(
        None,
        token_store.as_ref(),
        rokr_provider::anthropic::ENV_API_KEY,
    );
    let built =
        rokr_provider::build_provider(None, resolved_auth, rokr_provider::RetryPolicy::default());
    let (provider, provider_name, model_name): (
        Result<crate::runner::SharedProvider, String>,
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
                Ok(std::sync::Arc::new(tokio::sync::RwLock::new(
                    built.resilient,
                ))),
                provider_name,
                model_name,
            )
        }
        Err(err) => (Err(err), "unknown".to_string(), "unknown".to_string()),
    };

    // Session bootstrap: mirrors the TUI path's `ResumeMode::None` branch
    // exactly (schema v2 header, zero prior turns) -- headless never
    // resumes, it's always a single fresh submission. A store/creation
    // failure degrades gracefully (no persistence, empty session_id) rather
    // than aborting the run, matching the TUI path's own handling.
    let data_dir = crate::runner::default_data_dir();
    let store = rokr_session::SessionStore::open(&data_dir);
    let session_handle = match store.create_session() {
        Ok(handle) => {
            handle.append_header(
                2,
                handle.session_id().to_string(),
                crate::runner::now_timestamp(),
                cwd.as_ref()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                agent.prompt_name().to_string(),
                provider_name.clone(),
                model_name.clone(),
            );
            Some(std::sync::Arc::new(handle))
        }
        Err(err) => {
            eprintln!("failed to create session log: {err}");
            None
        }
    };
    let session_id = session_handle
        .as_ref()
        .map(|handle| handle.session_id().to_string())
        .unwrap_or_default();

    let (status_tx, _status_rx) = std::sync::mpsc::channel::<rokr_tui::SessionStatus>();
    let (mcp_notice_tx, _mcp_notice_rx) = std::sync::mpsc::channel::<String>();

    // Headless is single-shot and never resumes, so this transcript always
    // starts empty -- which is what makes reading it back after
    // `run_submission` returns (below) a faithful, non-fabricated record of
    // exactly this run's exchange (see `stream-json`'s dispatch below).
    let transcript: std::sync::Arc<tokio::sync::Mutex<Vec<rokr_core::Message>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let runner = crate::runner::SessionRunner {
        provider,
        transcript: transcript.clone(),
        system_prompt,
        repo_map: std::sync::Arc::new(std::sync::Mutex::new(repo_map)),
        last_known_usage: std::sync::Arc::new(std::sync::Mutex::new(None)),
        config_dir,
        session_handle: std::sync::Arc::new(tokio::sync::RwLock::new(session_handle)),
        turn_index: std::sync::Arc::new(std::sync::Mutex::new(0)),
        data_dir,
        status_tx,
        mcp_server_handles: std::sync::Arc::new(Vec::new()),
        mcp_notice_tx,
        mcp_http_origins: std::sync::Arc::new(std::collections::HashMap::new()),
        // Real hooks config now (ticket 54 hardcoded an empty map) --
        // `PreToolUse` hooks run exactly as in the TUI, per the ticket's
        // `## Context`.
        hooks_config: std::sync::Arc::new(config.hooks.clone()),
        agent,
        context_window_size: config.context_window_size,
        auto_compact_threshold: config.auto_compact_threshold,
    };

    let run_result = runner.run_submission(prompt, permission.clone()).await;
    let duration_ms = started_at.elapsed().as_millis() as u64;
    let usage = runner.last_known_usage.lock().unwrap().unwrap_or_default();

    // `run_tool_loop` does not abort on a denied gated tool call -- it
    // records an error `ToolResult` and loops again, so `run_submission`
    // can return `Ok` even though a call was denied (see this ticket's
    // report / `HeadlessPermissionRequester`'s doc comment). The shared
    // `denied` flag is the only way to detect that after the fact and
    // override the subtype accordingly.
    let (subtype, result_text) = match run_result {
        Ok(reply) if permission.denied() => (crate::result_schema::Subtype::ErrorPermission, reply),
        Ok(reply) => (crate::result_schema::Subtype::Success, reply),
        Err(err) if permission.denied() => (crate::result_schema::Subtype::ErrorPermission, err),
        // No real max-turns cap exists in `rokr_core::run_tool_loop` today
        // (out of this ticket's scope to add one) -- `ErrorMaxTurns` is the
        // closest fit among the three subtypes the ticket's `## Context`
        // documents for any OTHER `run_submission` failure (e.g. a
        // provider error), see `Subtype`'s own doc comment.
        Err(err) => (crate::result_schema::Subtype::ErrorMaxTurns, err),
    };
    let is_error = subtype != crate::result_schema::Subtype::Success;

    // Snapshotted once here (rather than filtering for `num_turns` and
    // separately re-locking for `HeadlessRunOutcome::transcript` /
    // `run`'s stream-json replay) -- a single faithful, non-fabricated
    // record of exactly this run's exchange.
    let transcript_snapshot = transcript.lock().await.clone();
    let num_turns = transcript_snapshot
        .iter()
        .filter(|message| message.role == rokr_core::Role::Assistant)
        .count() as u32;

    // Ticket 57 (cost-command-and-headless-reporting): resolves this run's
    // own model into a dollar rate (falling back to `calculate_cost`'s own
    // `None` -> `$0.00` handling for an unpriced/unknown model) and applies
    // it to the SAME `usage` value already used for `UsageObject` above --
    // `rokr_core::Usage` is `Copy`, so no re-fetch off `runner.last_known_usage`
    // is needed. Replaces this ticket's predecessor's `cost_usd: 0.0`
    // placeholder.
    let pricing_entry = config
        .model_pricing
        .get(&model_name)
        .map(model_pricing_to_pricing_entry);
    let cost_usd = rokr_core::pricing::calculate_cost(usage, pricing_entry.as_ref());

    let result_object = crate::result_schema::ResultObject {
        subtype,
        session_id,
        result: result_text,
        is_error,
        usage: crate::result_schema::UsageObject::from(usage),
        cost_usd,
        num_turns,
        duration_ms,
    };

    Ok(HeadlessRunOutcome {
        result_object,
        transcript: transcript_snapshot,
    })
}

pub fn build_permission_requester(
    mode: crate::cli::PermissionMode,
    dangerously_skip_permissions: bool,
) -> Result<HeadlessPermissionRequester, String> {
    if matches!(mode, crate::cli::PermissionMode::Bypass) && !dangerously_skip_permissions {
        return Err(
            "--permission-mode bypass requires the explicit --dangerously-skip-permissions flag"
                .to_string(),
        );
    }
    Ok(HeadlessPermissionRequester {
        mode,
        denied: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
}

/// Ticket 57: field-copy bridge from `rokr-config`'s `ModelPricing` (the
/// on-disk-configurable pricing table entry) to `rokr-core::pricing`'s
/// `PricingEntry` (what `calculate_cost` actually takes) -- the two crates
/// have no dependency edge on each other (see `ModelPricing`'s own doc
/// comment in `crates/rokr-config/src/lib.rs`), so there's no `From`/`Into`
/// between them to reuse. Same four fields, same names, same types;
/// duplicated in `crates/rokr/src/main.rs` for the same reason (that file
/// already depends on both crates too).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `select_mode` must return `Mode::Tui` when `--print` is absent
    /// (today's unchanged TUI-launch behavior), and `Mode::Headless(prompt)`
    /// carrying the literal argument text when `--print <prompt>` is
    /// present.
    #[test]
    fn headless_mode_selected_only_when_print_flag_present_otherwise_tui_launches() {
        assert!(matches!(select_mode(None, std::io::empty()), Mode::Tui));

        match select_mode(Some("say hi"), std::io::empty()) {
            Mode::Headless(prompt) => assert_eq!(prompt, "say hi"),
            Mode::Tui => panic!("expected Headless mode when --print is present"),
        }
    }

    /// `--print -` must read the prompt from stdin rather than treating the
    /// literal `-` as the prompt text.
    #[test]
    fn dash_prompt_argument_reads_from_stdin_instead_of_argv() {
        let stdin = std::io::Cursor::new(b"say hi\n".to_vec());

        match select_mode(Some("-"), stdin) {
            Mode::Headless(prompt) => assert_eq!(prompt, "say hi"),
            Mode::Tui => panic!("expected Headless mode when --print - is present"),
        }
    }

    /// Ticket 55 (headless-output-formats-and-permission-mode): `--permission-mode
    /// bypass` alone must NOT be enough to grant every gated tool call --
    /// there's no human in headless to approve anything, so bypassing the
    /// permission gate entirely requires the operator to also pass the
    /// explicit, unsafely-named `--dangerously-skip-permissions` flag.
    /// `Deny` and `AcceptEdits` never require that flag (they never grant
    /// everything unconditionally).
    #[test]
    fn bypass_permission_mode_requires_explicit_dangerously_skip_permissions_flag() {
        assert!(
            build_permission_requester(crate::cli::PermissionMode::Bypass, false).is_err(),
            "bypass mode without --dangerously-skip-permissions must be rejected"
        );
        assert!(
            build_permission_requester(crate::cli::PermissionMode::Bypass, true).is_ok(),
            "bypass mode WITH --dangerously-skip-permissions must be accepted"
        );

        // Neither Deny nor AcceptEdits require the flag either way.
        assert!(build_permission_requester(crate::cli::PermissionMode::Deny, false).is_ok());
        assert!(build_permission_requester(crate::cli::PermissionMode::AcceptEdits, false).is_ok());
    }
}
