use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "Usage: rokr [--version] [--agent <plan|build>]";

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

    match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("rokr {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            let agent = match parse_agent_tier(&args) {
                Ok(agent) => agent,
                Err(()) => {
                    eprintln!("{USAGE}");
                    return ExitCode::FAILURE;
                }
            };

            if let Err(err) = rokr_config::load_or_init_default() {
                eprintln!("failed to initialize config: {err}");
                return ExitCode::FAILURE;
            }

            let mut system_prompt = match rokr_config::read_agent_prompt(
                &rokr_config::default_config_dir(),
                agent.prompt_name(),
            ) {
                Ok(prompt) => prompt,
                Err(err) => {
                    eprintln!("failed to read agent prompt: {err}");
                    return ExitCode::FAILURE;
                }
            };

            // One-time, unconditional, side-effect-free read of project-level
            // context (AGENTS.md, falling back to CLAUDE.md) from the
            // current working directory, folded into the system prompt
            // alongside the active agent tier's prompt. Not a tool, not
            // permission-gated — this is how the system prompt is built, not
            // a model-invoked action. A cwd that can't be resolved is
            // treated the same as no project context being present.
            if let Ok(cwd) = std::env::current_dir() {
                if let Some(project_context) = rokr_config::load_project_context(&cwd) {
                    system_prompt.push_str("\n\n");
                    system_prompt.push_str(&project_context);
                }
            }

            // Generated once per session (ticket 18: repo-map-generation),
            // never per turn — cwd is the natural root, mirroring the
            // project-context load above. Not a tool, not permission-gated:
            // orientation infrastructure the agent never chooses to invoke,
            // so it's computed here alongside the system prompt rather than
            // wired through the tool/permission machinery. A cwd that can't
            // be resolved yields no repo map rather than failing startup.
            let repo_map: Option<String> = std::env::current_dir()
                .ok()
                .map(|cwd| rokr_tools::repo_map::generate(&cwd));

            // Constructed once at startup (so a missing/invalid env var
            // doesn't crash the TUI — it's reported the first time the
            // user submits a prompt instead) and wired through
            // `rokr_core::run_tool_loop`. `rokr-tui` stays decoupled from
            // `rokr-core`/`rokr-provider`, so this closure is where the
            // message model and provider abstraction meet the TUI.
            let provider = rokr_provider::OpenAiProvider::from_env()
                .map(Arc::new)
                .map_err(|err| err.to_string());

            // In-memory only (no persistence, per the PRD): accumulates
            // every turn across submits for the lifetime of the process, so
            // each new prompt is sent with the full prior conversation
            // history rather than in isolation. Stays system-prompt-free —
            // pure conversation history; `rokr_core::run_tool_loop` prepends
            // the system segment itself (via `context::assemble()`) on
            // every outgoing send, so it never needs to live here.
            let transcript: Vec<rokr_core::Message> = Vec::new();
            let transcript: Arc<tokio::sync::Mutex<Vec<rokr_core::Message>>> =
                Arc::new(tokio::sync::Mutex::new(transcript));

            let submit = move |input: String, permission: rokr_tui::PermissionHandle| {
                let provider = provider.clone();
                let transcript = transcript.clone();
                let system_prompt = system_prompt.clone();
                let repo_map = repo_map.clone();
                async move {
                    let provider = provider?;

                    // All seven tools are constructed unconditionally
                    // (they're cheap zero-sized unit structs); which ones
                    // actually land in `tools` depends on the agent tier.
                    // read/glob/grep/ls auto-approve (ADR 0005: none are
                    // `PreviewableTool`s); bash, write, and edit are gated
                    // and round-trip through the permission callback below,
                    // and only exist in the tool set for the `Build` tier.
                    let read = rokr_tools::read::ReadTool;
                    let glob = rokr_tools::glob::GlobTool;
                    let grep = rokr_tools::grep::GrepTool;
                    let ls = rokr_tools::ls::LsTool;
                    let bash = rokr_tools::bash::BashTool;
                    let write = rokr_tools::write::WriteTool;
                    let edit = rokr_tools::edit::EditTool;
                    let tools: Vec<&dyn rokr_core::ExecutableTool> = match agent {
                        AgentTier::Plan => vec![&read, &glob, &grep, &ls],
                        AgentTier::Build => vec![&read, &glob, &grep, &ls, &bash, &write, &edit],
                    };

                    // Bridges rokr-core's `PermissionRequest` (tool name +
                    // `PermissionPayload`) to rokr-tui's primitive
                    // `PermissionRequest` (tool name + a display string),
                    // round-tripping through the TUI's render loop via
                    // `permission`. This is the seam rokr-tui's `run` doc
                    // comment calls out: rokr-tui stays decoupled from
                    // rokr-core's specific types, so main.rs bridges them.
                    let request_permission = move |request: rokr_core::PermissionRequest| {
                        let permission = permission.clone();
                        async move {
                            let detail = match request.payload {
                                rokr_core::PermissionPayload::Command(command) => {
                                    rokr_tui::PermissionDetail::Text(command)
                                }
                                rokr_core::PermissionPayload::Diff { old, new } => {
                                    rokr_tui::PermissionDetail::Diff { old, new }
                                }
                            };
                            permission
                                .request(rokr_tui::PermissionRequest {
                                    tool_name: request.tool_name,
                                    detail,
                                })
                                .await
                        }
                    };

                    let mut transcript = transcript.lock().await;
                    accumulate_user_turn(&mut transcript, input);

                    rokr_core::run_tool_loop(
                        provider.as_ref(),
                        &system_prompt,
                        repo_map.as_deref(),
                        &mut transcript,
                        &tools,
                        request_permission,
                    )
                    .await
                    .map(|message| message.text())
                    .map_err(|err| err.to_string())
                }
            };

            match rokr_tui::run(submit).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rokr_core::{Message, Role};

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
}
