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
            // `rokr_core::single_turn`. `rokr-tui` stays decoupled from
            // `rokr-core`/`rokr-provider`, so this closure is where the
            // message model and provider abstraction meet the TUI.
            let provider = rokr_provider::OpenAiProvider::from_env()
                .map(Arc::new)
                .map_err(|err| err.to_string());

            let submit = move |input: String| {
                let provider = provider.clone();
                async move {
                    let provider = provider?;
                    rokr_core::single_turn(provider.as_ref(), input)
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
