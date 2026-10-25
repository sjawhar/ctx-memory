use std::process::ExitCode;

use clap::Parser as _;
use ctx_memory::cli::{self, Cli};
use ctx_memory::output;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if std::env::args_os().any(|argument| argument == "--json") {
                if let Err(emit_error) = output::emit_error("ctx", error.to_string()) {
                    eprintln!("{emit_error:#}");
                }
                return ExitCode::from(2);
            }
            error.exit();
        }
    };
    match cli::run(&cli) {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(error) => {
            if cli.json {
                if let Err(emit_error) =
                    output::emit_error(&cli.command.name(), format!("{error:#}"))
                {
                    eprintln!("{emit_error:#}");
                }
            } else {
                eprintln!("{error:#}");
            }
            ExitCode::from(2)
        }
    }
}
