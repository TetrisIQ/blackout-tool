use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use emergency_kit::cli::{Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = std::io::stdout().lock();
    match run(&cli, &mut stdout) {
        Ok(outcome) => {
            let _ = stdout.flush();
            ExitCode::from(outcome.exit_code())
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
