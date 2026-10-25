use std::process::ExitCode;

use clap::Parser as _;
use ctx_memory::cli::{self, Cli};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    match cli::run(&cli) {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(2)
        }
    }
}
