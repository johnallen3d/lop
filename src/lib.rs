pub mod cli;
pub mod config;
pub mod lock;
pub mod output;
pub mod paths;
pub mod state;

use std::io::Write;

use cli::{Cli, Command};
use config::Config;
use lock::RunLock;
use output::{CommandName, Event, Summary};
use paths::AppPaths;
use state::State;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Paths(#[from] paths::PathError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Lock(#[from] lock::LockError),
    #[error(transparent)]
    State(#[from] state::StateError),
    #[error("failed to write structured output: {0}")]
    Output(#[from] std::io::Error),
}

/// Runs Lop's operational shell. Repository discovery and classification plug into
/// this boundary without changing configuration, state, locking, or output contracts.
///
/// # Errors
///
/// Returns an error for invalid paths/configuration/state, lock contention, or output
/// failures.
pub fn run(cli: &Cli, output: &mut impl Write) -> Result<Summary, Error> {
    let paths = AppPaths::from_env()?;
    run_with_paths(cli, &paths, output)
}

/// Runs Lop using pre-resolved application paths.
///
/// # Errors
///
/// Returns an error for invalid configuration/state, lock contention, or output
/// failures.
pub fn run_with_paths(
    cli: &Cli,
    paths: &AppPaths,
    output: &mut impl Write,
) -> Result<Summary, Error> {
    let _lock = RunLock::acquire(&paths.lock_file)?;
    let _config = Config::load(&paths.config_file)?;
    let state = State::load(&paths.state_file)?;

    let (command, apply) = match &cli.command {
        Command::Scan => (CommandName::Scan, false),
        Command::Prune { yes } => (CommandName::Prune, *yes),
    };

    write_event(
        output,
        &Event::RunStarted {
            schema_version: output::OUTPUT_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION"),
            command,
            apply,
        },
    )?;

    // Later pipeline cards populate this summary through the same stable contract.
    let summary = Summary::empty(command, apply);
    state.save(&paths.state_file)?;
    write_event(output, &Event::summary(&summary))?;
    output.flush()?;
    Ok(summary)
}

fn write_event(output: &mut impl Write, event: &Event<'_>) -> std::io::Result<()> {
    serde_json::to_writer(&mut *output, event)?;
    output.write_all(b"\n")
}
