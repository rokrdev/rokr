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

            let submit = move |input: String| {
                let provider = provider.clone();
                async move {
                    let provider = provider?;

                    // Fixed read-only tool set (ADR 0005: no
                    // preview/permission gate needed since none of these
                    // are `PreviewableTool`s). Agent-tier selection lands
                    // in a later ticket; for now every prompt gets the same
                    // four tools.
                    let read = rokr_tools::read::ReadTool;
                    let glob = rokr_tools::glob::GlobTool;
                    let grep = rokr_tools::grep::GrepTool;
                    let ls = rokr_tools::ls::LsTool;
                    let tools: [&dyn rokr_core::ExecutableTool; 4] = [&read, &glob, &grep, &ls];

                    rokr_core::run_tool_loop(provider.as_ref(), input, &tools)
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
