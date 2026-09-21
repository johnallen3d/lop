pub mod cli;
pub mod config;
pub mod discovery;
pub mod inspection;
pub mod integration;
pub mod lock;
pub mod output;
pub mod paths;
pub mod state;

use std::{io::Write, time::Duration};

use cli::{Cli, Command};
use config::Config;
use discovery::{DiscoveryIssueKind, discover};
use inspection::{RepositoryInspectionOutcome, WorktreeClassification, inspect_repository};
use lock::RunLock;
use output::{CommandName, Event, ReasonCode, RepositoryOutcome, Summary, WorktreeOutcome};
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
    let mut summary = Summary::empty(command, apply);
    summary.repositories_scanned = discovery.repositories.len() as u64;
    summary.operational_failures = discovery.issues.len() as u64;

    for repository in &discovery.repositories {
        let inspection = inspect_repository(
            &repository.path,
            Duration::from_secs(config.fetch_timeout_seconds),
        );
        let (repository_outcome, repository_reason) = repository_result(inspection.outcome);
        if inspection.outcome != RepositoryInspectionOutcome::Inspected {
            summary.operational_failures += 1;
        }
        if matches!(
            inspection.outcome,
            RepositoryInspectionOutcome::FetchFailed | RepositoryInspectionOutcome::FetchTimedOut
        ) {
            summary.fetch_failures += 1;
        }
        if matches!(
            inspection.outcome,
            RepositoryInspectionOutcome::MalformedPorcelain
                | RepositoryInspectionOutcome::InspectionFailed
        ) {
            summary.malformed_states += 1;
        }

        write_event(
            output,
            &Event::Repository {
                schema_version: output::OUTPUT_SCHEMA_VERSION,
                path: repository.path.clone(),
                git_common_directory: Some(repository.git_common_directory.clone()),
                outcome: repository_outcome,
                reason_code: repository_reason,
                message: inspection.message.as_deref(),
            },
        )?;

        for worktree in inspection.worktrees {
            let (worktree_outcome, worktree_reason) = worktree_result(worktree.classification);
            summary.worktrees_inspected += 1;
            match worktree_outcome {
                WorktreeOutcome::Candidate => summary.candidates += 1,
                WorktreeOutcome::Refused => summary.refusals += 1,
                WorktreeOutcome::Malformed => summary.malformed_states += 1,
                WorktreeOutcome::Retained | WorktreeOutcome::Skipped | WorktreeOutcome::Removed => {
                }
            }
            write_event(
                output,
                &Event::Worktree {
                    schema_version: output::OUTPUT_SCHEMA_VERSION,
                    repository: repository.path.clone(),
                    path: worktree.path,
                    outcome: worktree_outcome,
                    reason_code: worktree_reason,
                },
            )?;
        }
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

    state.save(&paths.state_file)?;
    write_event(output, &Event::summary(&summary))?;
    output.flush()?;
    Ok(summary)
}

const fn repository_result(
    outcome: RepositoryInspectionOutcome,
) -> (RepositoryOutcome, ReasonCode) {
    match outcome {
        RepositoryInspectionOutcome::Inspected => (RepositoryOutcome::Inspected, ReasonCode::Ok),
        RepositoryInspectionOutcome::FetchFailed => {
            (RepositoryOutcome::FetchFailed, ReasonCode::FetchFailed)
        }
        RepositoryInspectionOutcome::FetchTimedOut => {
            (RepositoryOutcome::FetchTimedOut, ReasonCode::FetchTimedOut)
        }
        RepositoryInspectionOutcome::MalformedPorcelain => {
            (RepositoryOutcome::Malformed, ReasonCode::MalformedPorcelain)
        }
        RepositoryInspectionOutcome::IntegrationFailed => (
            RepositoryOutcome::IntegrationFailed,
            ReasonCode::WorktrunkFailed,
        ),
        RepositoryInspectionOutcome::InspectionFailed => {
            (RepositoryOutcome::Malformed, ReasonCode::InvalidGitMetadata)
        }
    }
}

const fn worktree_result(classification: WorktreeClassification) -> (WorktreeOutcome, ReasonCode) {
    match classification {
        WorktreeClassification::MainWorktree => {
            (WorktreeOutcome::Retained, ReasonCode::MainWorktree)
        }
        WorktreeClassification::UpstreamExists => {
            (WorktreeOutcome::Retained, ReasonCode::UpstreamExists)
        }
        WorktreeClassification::NoUpstream => (WorktreeOutcome::Skipped, ReasonCode::NoUpstream),
        WorktreeClassification::DetachedWorktree => {
            (WorktreeOutcome::Skipped, ReasonCode::DetachedWorktree)
        }
        WorktreeClassification::DanglingSymbolicHead => {
            (WorktreeOutcome::Malformed, ReasonCode::DanglingSymbolicHead)
        }
        WorktreeClassification::PrunableWorktree => {
            (WorktreeOutcome::Malformed, ReasonCode::PrunableWorktree)
        }
        WorktreeClassification::IntegratedSameCommit => {
            (WorktreeOutcome::Candidate, ReasonCode::IntegratedSameCommit)
        }
        WorktreeClassification::IntegratedAncestor => {
            (WorktreeOutcome::Candidate, ReasonCode::IntegratedAncestor)
        }
        WorktreeClassification::IntegratedNoAddedChanges => (
            WorktreeOutcome::Candidate,
            ReasonCode::IntegratedNoAddedChanges,
        ),
        WorktreeClassification::IntegratedTreesMatch => {
            (WorktreeOutcome::Candidate, ReasonCode::IntegratedTreesMatch)
        }
        WorktreeClassification::IntegratedMergeAddsNothing => (
            WorktreeOutcome::Candidate,
            ReasonCode::IntegratedMergeAddsNothing,
        ),
        WorktreeClassification::IntegratedPatchIdMatch => (
            WorktreeOutcome::Candidate,
            ReasonCode::IntegratedPatchIdMatch,
        ),
        WorktreeClassification::NotIntegrated => {
            (WorktreeOutcome::Retained, ReasonCode::NotIntegrated)
        }
        WorktreeClassification::IntegrationIndeterminate => (
            WorktreeOutcome::Retained,
            ReasonCode::IntegrationIndeterminate,
        ),
        WorktreeClassification::DefaultBranchUnresolved => (
            WorktreeOutcome::Retained,
            ReasonCode::DefaultBranchUnresolved,
        ),
        WorktreeClassification::WorktrunkFailed => {
            (WorktreeOutcome::Retained, ReasonCode::WorktrunkFailed)
        }
        WorktreeClassification::FetchFailed => (WorktreeOutcome::Skipped, ReasonCode::FetchFailed),
        WorktreeClassification::FetchTimedOut => {
            (WorktreeOutcome::Skipped, ReasonCode::FetchTimedOut)
        }
        WorktreeClassification::Malformed => {
            (WorktreeOutcome::Malformed, ReasonCode::InvalidGitMetadata)
        }
    }
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
