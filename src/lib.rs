pub mod cleanup;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod herdr;
pub mod inspection;
pub mod integration;
pub mod lock;
pub mod output;
pub mod paths;
pub mod schedule;
pub mod state;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
    time::Duration,
};

use cleanup::{
    BranchOutcome, SafetyRefusal, check_candidate, check_candidate_ignoring_processes,
    remove_worktree,
};
use cli::{Cli, Command};
use config::Config;
use discovery::{DiscoveryIssueKind, discover};
use herdr::{
    CandidateStatus, StaleWorkspaceStatus, inspect_candidate, inspect_stale_workspaces,
    retire_candidate,
};
use inspection::{
    RepositoryInspectionOutcome, WorktreeClassification, WorktreeInspection, inspect_repository,
};
use lock::RunLock;
use output::{CommandName, Event, ReasonCode, RepositoryOutcome, Summary, WorktreeOutcome};
use paths::AppPaths;
use state::{ClassificationState, State};
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
    #[error("schedule commands must be run through the scheduling interface")]
    ScheduleCommand,
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
    let mut state = State::load(&paths.state_file)?;

    let (command, apply) = match &cli.command {
        Command::Scan => (CommandName::Scan, false),
        Command::Prune { yes } => (CommandName::Prune, *yes),
        Command::Schedule { .. } => return Err(Error::ScheduleCommand),
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
        update_repository_summary(&mut summary, inspection.outcome);

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

        let repository_key = repository
            .git_common_directory
            .to_string_lossy()
            .into_owned();
        let previous_classifications = state
            .repositories
            .get(&repository_key)
            .map(|repository| &repository.last_classifications);
        let mut current_classifications = BTreeMap::new();
        let inventory_paths: BTreeSet<_> = inspection
            .worktrees
            .iter()
            .map(|worktree| worktree.path.clone())
            .collect();

        for worktree in inspection.worktrees {
            let decision =
                decide_worktree(&repository.path, &worktree, &config, apply, &mut summary);
            write_worktree_decision(
                output,
                &repository.path,
                worktree.path,
                &decision,
                previous_classifications,
                &mut current_classifications,
            )?;
        }

        for (path, decision) in reconcile_stale_herdr_workspaces(
            &repository.git_common_directory,
            &inventory_paths,
            &config,
            apply,
            &mut summary,
        ) {
            write_worktree_decision(
                output,
                &repository.path,
                path,
                &decision,
                previous_classifications,
                &mut current_classifications,
            )?;
        }

        state
            .repositories
            .entry(repository_key)
            .or_default()
            .last_classifications = current_classifications;
    }
    write_discovery_issues(output, &discovery.issues)?;

    state.save(&paths.state_file)?;
    write_event(output, &Event::summary(&summary))?;
    output.flush()?;
    Ok(summary)
}

fn write_discovery_issues(
    output: &mut impl Write,
    issues: &[discovery::DiscoveryIssue],
) -> std::io::Result<()> {
    for issue in issues {
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
    Ok(())
}

struct WorktreeDecision {
    outcome: WorktreeOutcome,
    reason_code: ReasonCode,
    message: Option<String>,
}

fn write_worktree_decision(
    output: &mut impl Write,
    repository: &std::path::Path,
    path: PathBuf,
    decision: &WorktreeDecision,
    previous: Option<&BTreeMap<String, ClassificationState>>,
    current: &mut BTreeMap<String, ClassificationState>,
) -> std::io::Result<()> {
    let classification_changed = record_classification(previous, current, &path, decision);
    write_event(
        output,
        &Event::Worktree {
            schema_version: output::OUTPUT_SCHEMA_VERSION,
            repository: repository.to_path_buf(),
            path,
            outcome: decision.outcome,
            reason_code: decision.reason_code,
            classification_changed,
            message: decision.message.as_deref(),
        },
    )
}

fn reconcile_stale_herdr_workspaces(
    git_common_directory: &std::path::Path,
    inventory_paths: &BTreeSet<PathBuf>,
    config: &Config,
    apply: bool,
    summary: &mut Summary,
) -> Vec<(PathBuf, WorktreeDecision)> {
    let timeout = Duration::from_secs(config.fetch_timeout_seconds);
    let stale = match inspect_stale_workspaces(git_common_directory, inventory_paths, timeout) {
        Ok(StaleWorkspaceStatus::Unavailable) => return Vec::new(),
        Ok(StaleWorkspaceStatus::Inspected(stale)) => stale,
        Err(_) => {
            summary.operational_failures += 1;
            return Vec::new();
        }
    };

    summary.worktrees_inspected += stale.len() as u64;
    stale
        .into_iter()
        .map(|workspace| {
            let path = workspace.checkout_path;
            let decision = match workspace.status {
                CandidateStatus::Clear(_coordination) if !apply => WorktreeDecision {
                    outcome: WorktreeOutcome::Candidate,
                    reason_code: ReasonCode::HerdrStaleWorkspacePending,
                    message: Some("stale idle Herdr workspace is pending retirement".to_owned()),
                },
                CandidateStatus::Clear(coordination) => retire_stale_workspace(
                    git_common_directory,
                    inventory_paths,
                    &path,
                    &coordination,
                    timeout,
                    summary,
                ),
                CandidateStatus::FocusedPane { pane_id } => WorktreeDecision {
                    outcome: WorktreeOutcome::Refused,
                    reason_code: ReasonCode::HerdrFocusedPane,
                    message: Some(format!(
                        "stale Herdr workspace contains focused pane {pane_id}"
                    )),
                },
                CandidateStatus::ActiveAgent { pane_id } => WorktreeDecision {
                    outcome: WorktreeOutcome::Refused,
                    reason_code: ReasonCode::HerdrActiveAgent,
                    message: Some(format!(
                        "stale Herdr workspace contains active pane {pane_id}"
                    )),
                },
                CandidateStatus::Unavailable => unreachable!("nested stale status unavailable"),
            };
            update_worktree_summary(&decision, summary);
            (path, decision)
        })
        .collect()
}

fn retire_stale_workspace(
    git_common_directory: &std::path::Path,
    inventory_paths: &BTreeSet<PathBuf>,
    checkout_path: &std::path::Path,
    coordination: &herdr::Coordination,
    timeout: Duration,
    summary: &mut Summary,
) -> WorktreeDecision {
    if let Err(error) = retire_candidate(coordination, timeout) {
        summary.operational_failures += 1;
        return WorktreeDecision {
            outcome: WorktreeOutcome::Refused,
            reason_code: ReasonCode::HerdrStaleWorkspaceFailed,
            message: Some(error.to_string()),
        };
    }

    match inspect_stale_workspaces(git_common_directory, inventory_paths, timeout) {
        Ok(StaleWorkspaceStatus::Inspected(stale))
            if !stale
                .iter()
                .any(|workspace| workspace.checkout_path == checkout_path) =>
        {
            WorktreeDecision {
                outcome: WorktreeOutcome::Removed,
                reason_code: ReasonCode::HerdrStaleWorkspaceRetired,
                message: Some("retired stale idle Herdr workspace".to_owned()),
            }
        }
        Ok(StaleWorkspaceStatus::Inspected(_)) => {
            summary.operational_failures += 1;
            WorktreeDecision {
                outcome: WorktreeOutcome::Refused,
                reason_code: ReasonCode::HerdrStaleWorkspaceFailed,
                message: Some("stale Herdr workspace remained after close".to_owned()),
            }
        }
        Ok(StaleWorkspaceStatus::Unavailable) | Err(_) => {
            summary.operational_failures += 1;
            WorktreeDecision {
                outcome: WorktreeOutcome::Refused,
                reason_code: ReasonCode::HerdrStaleWorkspaceFailed,
                message: Some("could not verify stale Herdr workspace retirement".to_owned()),
            }
        }
    }
}

fn update_worktree_summary(decision: &WorktreeDecision, summary: &mut Summary) {
    match decision.outcome {
        WorktreeOutcome::Candidate => summary.candidates += 1,
        WorktreeOutcome::Refused => summary.refusals += 1,
        WorktreeOutcome::Malformed => summary.malformed_states += 1,
        WorktreeOutcome::Removed => summary.removals += 1,
        WorktreeOutcome::Retained | WorktreeOutcome::Skipped => {}
    }
}

fn decide_worktree(
    repository: &std::path::Path,
    worktree: &WorktreeInspection,
    config: &Config,
    apply: bool,
    summary: &mut Summary,
) -> WorktreeDecision {
    let (outcome, reason_code) = worktree_result(worktree.classification);
    let mut decision = WorktreeDecision {
        outcome,
        reason_code,
        message: None,
    };
    summary.worktrees_inspected += 1;

    if decision.outcome == WorktreeOutcome::Candidate {
        let timeout = Duration::from_secs(config.fetch_timeout_seconds);
        if prepare_candidate(
            repository,
            worktree,
            config,
            apply,
            timeout,
            &mut decision,
            summary,
        ) {
            apply_removal(repository, worktree, timeout, &mut decision, summary);
        }
    }

    update_worktree_summary(&decision, summary);
    decision
}

fn prepare_candidate(
    repository: &std::path::Path,
    worktree: &WorktreeInspection,
    config: &Config,
    apply: bool,
    timeout: Duration,
    decision: &mut WorktreeDecision,
    summary: &mut Summary,
) -> bool {
    if let Err(refusal) = check_candidate(repository, worktree, false, timeout) {
        apply_safety_refusal(refusal, decision, summary);
        return false;
    }

    match inspect_candidate(&worktree.path, timeout) {
        Ok(CandidateStatus::Unavailable) => {
            match check_candidate(repository, worktree, config.check_processes, timeout) {
                Ok(()) => apply,
                Err(refusal) => {
                    apply_safety_refusal(refusal, decision, summary);
                    false
                }
            }
        }
        Ok(CandidateStatus::Clear(coordination)) => {
            if let Err(refusal) = check_candidate_ignoring_processes(
                repository,
                worktree,
                config.check_processes,
                timeout,
                coordination.attributed_processes(),
            ) {
                apply_safety_refusal(refusal, decision, summary);
                return false;
            }
            if !apply {
                if coordination.requires_retirement() {
                    decision.reason_code = ReasonCode::HerdrCoordinationPending;
                    decision.message = Some(format!(
                        "{} idle Herdr workspace(s) must be retired before removal",
                        coordination.workspace_count()
                    ));
                }
                return false;
            }
            if coordination.requires_retirement()
                && let Err(error) = retire_candidate(&coordination, timeout)
            {
                decision.outcome = WorktreeOutcome::Refused;
                decision.reason_code = ReasonCode::HerdrRetirementFailed;
                decision.message = Some(error.to_string());
                summary.operational_failures += 1;
                return false;
            }
            recheck_after_coordination(repository, worktree, config, timeout, decision, summary)
        }
        Ok(status) => {
            apply_herdr_veto(status, decision);
            false
        }
        Err(error) => {
            decision.outcome = WorktreeOutcome::Refused;
            decision.reason_code = ReasonCode::HerdrInspectionFailed;
            decision.message = Some(error.to_string());
            summary.operational_failures += 1;
            false
        }
    }
}

fn apply_removal(
    repository: &std::path::Path,
    worktree: &WorktreeInspection,
    timeout: Duration,
    decision: &mut WorktreeDecision,
    summary: &mut Summary,
) {
    match remove_worktree(repository, &worktree.path, timeout) {
        Ok(result) => {
            decision.outcome = WorktreeOutcome::Removed;
            decision.reason_code = branch_outcome_reason(result.branch_outcome);
            if result.branch_outcome == BranchOutcome::RetainedFailed {
                summary.operational_failures += 1;
            }
            decision.message = result
                .branch_checked_out_at
                .map(|path| format!("branch is also checked out at {}", path.display()));
        }
        Err(error) => {
            decision.outcome = WorktreeOutcome::Refused;
            decision.reason_code = ReasonCode::RemovalFailed;
            decision.message = Some(error.to_string());
            summary.operational_failures += 1;
        }
    }
}

fn recheck_after_coordination(
    repository: &std::path::Path,
    worktree: &WorktreeInspection,
    config: &Config,
    timeout: Duration,
    decision: &mut WorktreeDecision,
    summary: &mut Summary,
) -> bool {
    match inspect_candidate(&worktree.path, timeout) {
        Ok(CandidateStatus::Clear(coordination)) if !coordination.requires_retirement() => {
            match check_candidate(repository, worktree, config.check_processes, timeout) {
                Ok(()) => true,
                Err(refusal) => {
                    apply_safety_refusal(refusal, decision, summary);
                    false
                }
            }
        }
        Ok(CandidateStatus::Clear(coordination)) => {
            decision.outcome = WorktreeOutcome::Refused;
            decision.reason_code = ReasonCode::HerdrRetirementFailed;
            decision.message = Some(format!(
                "{} candidate Herdr workspace(s) remain after retirement",
                coordination.workspace_count()
            ));
            summary.operational_failures += 1;
            false
        }
        Ok(CandidateStatus::Unavailable) => {
            decision.outcome = WorktreeOutcome::Refused;
            decision.reason_code = ReasonCode::HerdrInspectionFailed;
            decision.message =
                Some("Herdr became unavailable while rechecking retired workspaces".to_owned());
            summary.operational_failures += 1;
            false
        }
        Ok(status) => {
            apply_herdr_veto(status, decision);
            false
        }
        Err(error) => {
            decision.outcome = WorktreeOutcome::Refused;
            decision.reason_code = ReasonCode::HerdrInspectionFailed;
            decision.message = Some(error.to_string());
            summary.operational_failures += 1;
            false
        }
    }
}

fn apply_herdr_veto(status: CandidateStatus, decision: &mut WorktreeDecision) {
    decision.outcome = WorktreeOutcome::Refused;
    match status {
        CandidateStatus::FocusedPane { pane_id } => {
            decision.reason_code = ReasonCode::HerdrFocusedPane;
            decision.message = Some(format!("Herdr pane {pane_id} is focused in the worktree"));
        }
        CandidateStatus::ActiveAgent { pane_id } => {
            decision.reason_code = ReasonCode::HerdrActiveAgent;
            decision.message = Some(format!("Herdr pane {pane_id} contains an active agent"));
        }
        CandidateStatus::Unavailable | CandidateStatus::Clear(_) => {
            unreachable!("non-veto status passed to apply_herdr_veto")
        }
    }
}

fn apply_safety_refusal(
    refusal: SafetyRefusal,
    decision: &mut WorktreeDecision,
    summary: &mut Summary,
) {
    decision.outcome = WorktreeOutcome::Refused;
    decision.reason_code = refusal.reason_code;
    decision.message = Some(refusal.message);
    if refusal.operational_failure {
        summary.operational_failures += 1;
    }
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
        WorktreeClassification::LockedWorktree => {
            (WorktreeOutcome::Refused, ReasonCode::LockedWorktree)
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

const fn branch_outcome_reason(outcome: BranchOutcome) -> ReasonCode {
    match outcome {
        BranchOutcome::Deleted => ReasonCode::BranchDeleted,
        BranchOutcome::NotAttempted | BranchOutcome::Deferred => {
            ReasonCode::BranchDeletionNotAttempted
        }
        BranchOutcome::RetainedUnmerged => ReasonCode::BranchRetainedUnmerged,
        BranchOutcome::RetainedCheckedOut => ReasonCode::BranchRetainedCheckedOut,
        BranchOutcome::RetainedRaced => ReasonCode::BranchRetainedRaced,
        BranchOutcome::RetainedFailed => ReasonCode::BranchDeletionFailed,
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

fn update_repository_summary(summary: &mut Summary, outcome: RepositoryInspectionOutcome) {
    if outcome != RepositoryInspectionOutcome::Inspected {
        summary.operational_failures += 1;
    }
    if matches!(
        outcome,
        RepositoryInspectionOutcome::FetchFailed | RepositoryInspectionOutcome::FetchTimedOut
    ) {
        summary.fetch_failures += 1;
    }
    if matches!(
        outcome,
        RepositoryInspectionOutcome::MalformedPorcelain
            | RepositoryInspectionOutcome::InspectionFailed
    ) {
        summary.malformed_states += 1;
    }
}

fn record_classification(
    previous: Option<&BTreeMap<String, ClassificationState>>,
    current: &mut BTreeMap<String, ClassificationState>,
    path: &std::path::Path,
    decision: &WorktreeDecision,
) -> bool {
    let path = path.to_string_lossy().into_owned();
    let classification = ClassificationState {
        classification: serialized_name(decision.outcome),
        reason_code: serialized_name(decision.reason_code),
    };
    let changed =
        previous.and_then(|classifications| classifications.get(&path)) != Some(&classification);
    current.insert(path, classification);
    changed
}

fn serialized_name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("unit enums serialize without failure")
        .as_str()
        .expect("output enums serialize as strings")
        .to_owned()
}

fn write_event(output: &mut impl Write, event: &Event<'_>) -> std::io::Result<()> {
    serde_json::to_writer(&mut *output, event)?;
    output.write_all(b"\n")
}
