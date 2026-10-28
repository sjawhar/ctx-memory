use std::process::ExitCode;

use ctx_memory::cli;
use ctx_memory::output::{colour_wanted, Sink};

fn main() -> ExitCode {
    // The streams are locked per write, not for the life of the process. The server and the
    // fetch print from threads of their own — the follow loop, the lease renewal, a write
    // whose rollback failed — and a stream this thread held locked stopped each of them, for
    // good, at the first line it printed.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let colour = colour_wanted(&out);
    let mut sink = Sink::new(&mut out, &mut err).coloured(colour);
    ExitCode::from(cli::run_reported(std::env::args_os(), &mut sink, cli::run))
}
