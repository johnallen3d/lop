use std::{io, process::ExitCode};

use clap::Parser;
use lop::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut output = io::BufWriter::new(io::stdout().lock());
    match lop::run(&cli, &mut output) {
        Ok(summary) => ExitCode::from(summary.exit_code()),
        Err(error) => {
            eprintln!("lop: {error}");
            ExitCode::FAILURE
        }
    }
}
