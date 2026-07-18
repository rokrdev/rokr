use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "Usage: rokr [--version]";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [] => {
            if let Err(err) = rokr_config::load_or_init_default() {
                eprintln!("failed to initialize config: {err}");
                return ExitCode::FAILURE;
            }

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
            // history rather than in isolation.
            let transcript: Arc<tokio::sync::Mutex<Vec<rokr_core::Message>>> =
                Arc::new(tokio::sync::Mutex::new(Vec::new()));

            let submit = move |input: String, permission: rokr_tui::PermissionHandle| {
                let provider = provider.clone();
                let transcript = transcript.clone();
                async move {
                    let provider = provider?;

                    // Fixed tool set: read/glob/grep/ls auto-approve (ADR
                    // 0005: none are `PreviewableTool`s), bash and write are
                    // gated and round-trip through the permission callback
                    // below. Agent-tier selection lands in a later ticket;
                    // for now every prompt gets the same six tools.
                    let read = rokr_tools::read::ReadTool;
                    let glob = rokr_tools::glob::GlobTool;
                    let grep = rokr_tools::grep::GrepTool;
                    let ls = rokr_tools::ls::LsTool;
                    let bash = rokr_tools::bash::BashTool;
                    let write = rokr_tools::write::WriteTool;
                    let tools: [&dyn rokr_core::ExecutableTool; 6] =
                        [&read, &glob, &grep, &ls, &bash, &write];

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
        [flag] if flag == "--version" => {
            println!("rokr {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
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
