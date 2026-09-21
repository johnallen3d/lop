pub mod cli;
pub mod config;
pub mod discovery;
pub mod lock;
pub mod output;
pub mod paths;
pub mod state;

use std::io::Write;

use cli::{Cli, Command};
use config::Config;
use discovery::{DiscoveryIssueKind, discover};
use lock::RunLock;
use output::{CommandName, Event, ReasonCode, RepositoryOutcome, Summary};
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
    let config = Config::load(&paths.config_file)?;
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

    let discovery = discover(&config.roots, config.scan_depth);
    for repository in &discovery.repositories {
        write_event(
            output,
            &Event::Repository {
                schema_version: output::OUTPUT_SCHEMA_VERSION,
                path: repository.path.clone(),
                git_common_directory: Some(repository.git_common_directory.clone()),
                outcome: RepositoryOutcome::Inspected,
                reason_code: ReasonCode::Ok,
            },
        )?;
    }
    for issue in &discovery.issues {
        write_event(
            output,
            &Event::DiscoveryFailure {
                schema_version: output::OUTPUT_SCHEMA_VERSION,
                path: issue.path.clone(),
                reason_code: reason_code(issue.kind),
                message: &issue.message,
            },
        )?;
    }

    let mut summary = Summary::empty(command, apply);
    summary.repositories_scanned = discovery.repositories.len() as u64;
    summary.operational_failures = discovery.issues.len() as u64;
    state.save(&paths.state_file)?;
    write_event(output, &Event::summary(&summary))?;
    output.flush()?;
    Ok(summary)
}

const fn reason_code(kind: DiscoveryIssueKind) -> ReasonCode {
    match kind {
        DiscoveryIssueKind::InaccessibleRoot => ReasonCode::InaccessibleRoot,
        DiscoveryIssueKind::InaccessibleDirectory => ReasonCode::InaccessibleDirectory,
        DiscoveryIssueKind::UnsafeCanonicalization => ReasonCode::UnsafeCanonicalization,
        DiscoveryIssueKind::InvalidGitMetadata => ReasonCode::InvalidGitMetadata,
    }
}

fn write_event(output: &mut impl Write, event: &Event<'_>) -> std::io::Result<()> {
    serde_json::to_writer(&mut *output, event)?;
    output.write_all(b"\n")
}
