use std::process::ExitCode;

const USAGE: &str = "Usage: rokr [--version]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [] => {
            if let Err(err) = rokr_config::load_or_init_default() {
                eprintln!("failed to initialize config: {err}");
                return ExitCode::FAILURE;
            }
            println!("rokr — pre-alpha");
            ExitCode::SUCCESS
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
