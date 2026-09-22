use std::{io, process::ExitCode};

use clap::Parser;
use lop::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut output = io::BufWriter::new(io::stdout().lock());
    if let Command::Schedule { command } = &cli.command {
        let mut warnings = io::BufWriter::new(io::stderr().lock());
        return match lop::schedule::run(*command, &mut output, &mut warnings) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lop: {error}");
                ExitCode::FAILURE
            }
        };
    }

    match lop::run(&cli, &mut output) {
        Ok(summary) => ExitCode::from(summary.exit_code()),
        Err(error) => {
            eprintln!("lop: {error}");
            ExitCode::FAILURE
        }
    }
}
