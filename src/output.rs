use std::path::PathBuf;

use serde::Serialize;

pub const OUTPUT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandName {
    Scan,
    Prune,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryOutcome {
    Inspected,
    FetchFailed,
    FetchTimedOut,
    IntegrationFailed,
    Malformed,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeOutcome {
    Retained,
    Candidate,
    Skipped,
    Removed,
    Refused,
    Malformed,
}

/// Machine-stable explanations. Human wording may change; these values do not.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    Ok,
    InaccessibleRoot,
    InaccessibleDirectory,
    UnsafeCanonicalization,
    FetchFailed,
    FetchTimedOut,
    InvalidGitMetadata,
    MalformedPorcelain,
    MainWorktree,
    UpstreamExists,
    NoUpstream,
    DetachedWorktree,
    DanglingSymbolicHead,
    PrunableWorktree,
    LockedWorktree,
    UpstreamMissing,
    IntegratedSameCommit,
    IntegratedAncestor,
    IntegratedNoAddedChanges,
    IntegratedTreesMatch,
    IntegratedMergeAddsNothing,
    IntegratedPatchIdMatch,
    NotIntegrated,
    IntegrationIndeterminate,
    DefaultBranchUnresolved,
    WorktrunkFailed,
    DirtyWorktree,
    CheckedOutMultipleTimes,
    ProcessUsingWorktree,
    HerdrFocusedPane,
    HerdrActiveAgent,
    HerdrCoordinationPending,
    HerdrStaleWorkspacePending,
    HerdrStaleWorkspaceRetired,
    HerdrStaleWorkspaceFailed,
    HerdrInspectionFailed,
    SafetyInspectionFailed,
    WorktreeMissing,
    WorktreeChanged,
    RemovalNotConfirmed,
    RemovalFailed,
    HerdrCleanupFailed,
    HerdrRetirementFailed,
    BranchDeleted,
    BranchDeletionNotAttempted,
    BranchRetainedUnmerged,
    BranchRetainedRaced,
    BranchRetainedCheckedOut,
    BranchDeletionFailed,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Summary {
    pub schema_version: u32,
    pub command: CommandName,
    pub apply: bool,
    pub repositories_scanned: u64,
    pub fetch_failures: u64,
    pub worktrees_inspected: u64,
    pub candidates: u64,
    pub removals: u64,
    pub refusals: u64,
    pub malformed_states: u64,
    pub operational_failures: u64,
}

impl Summary {
    #[must_use]
    pub fn empty(command: CommandName, apply: bool) -> Self {
        Self {
            schema_version: OUTPUT_SCHEMA_VERSION,
            command,
            apply,
            repositories_scanned: 0,
            fetch_failures: 0,
            worktrees_inspected: 0,
            candidates: 0,
            removals: 0,
            refusals: 0,
            malformed_states: 0,
            operational_failures: 0,
        }
    }

    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        if self.operational_failures == 0 { 0 } else { 1 }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Event<'a> {
    RunStarted {
        schema_version: u32,
        tool_version: &'a str,
        command: CommandName,
        apply: bool,
    },
    Repository {
        schema_version: u32,
        path: PathBuf,
        git_common_directory: Option<PathBuf>,
        outcome: RepositoryOutcome,
        reason_code: ReasonCode,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'a str>,
    },
    DiscoveryFailure {
        schema_version: u32,
        path: PathBuf,
        reason_code: ReasonCode,
        message: &'a str,
    },
    Worktree {
        schema_version: u32,
        repository: PathBuf,
        path: PathBuf,
        outcome: WorktreeOutcome,
        reason_code: ReasonCode,
        /// True when the outcome or reason differs from the last completed run.
        classification_changed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<&'a str>,
    },
    Summary {
        #[serde(flatten)]
        summary: &'a Summary,
    },
}

impl<'a> Event<'a> {
    #[must_use]
    pub const fn summary(summary: &'a Summary) -> Self {
        Self::Summary { summary }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{
        CommandName, Event, OUTPUT_SCHEMA_VERSION, ReasonCode, RepositoryOutcome, Summary,
        WorktreeOutcome,
    };

    #[test]
    fn summary_schema_is_stable() {
        let summary = Summary::empty(CommandName::Prune, false);
        let value = serde_json::to_value(Event::summary(&summary)).unwrap();
        assert_eq!(
            value,
            json!({
                "record": "summary",
                "schema_version": OUTPUT_SCHEMA_VERSION,
                "command": "prune",
                "apply": false,
                "repositories_scanned": 0,
                "fetch_failures": 0,
                "worktrees_inspected": 0,
                "candidates": 0,
                "removals": 0,
                "refusals": 0,
                "malformed_states": 0,
                "operational_failures": 0
            })
        );
    }

    #[test]
    fn repository_and_worktree_records_have_stable_reasons() {
        let repository = Event::Repository {
            schema_version: OUTPUT_SCHEMA_VERSION,
            path: PathBuf::from("/src/project"),
            git_common_directory: Some(PathBuf::from("/src/project/.git")),
            outcome: RepositoryOutcome::FetchFailed,
            reason_code: ReasonCode::FetchFailed,
            message: Some("network unavailable"),
        };
        let worktree = Event::Worktree {
            schema_version: OUTPUT_SCHEMA_VERSION,
            repository: PathBuf::from("/src/project"),
            path: PathBuf::from("/src/project-feature"),
            outcome: WorktreeOutcome::Refused,
            reason_code: ReasonCode::DirtyWorktree,
            classification_changed: true,
            message: None,
        };

        assert_eq!(
            serde_json::to_value(repository).unwrap()["reason_code"],
            "fetch_failed"
        );
        let worktree = serde_json::to_value(worktree).unwrap();
        assert_eq!(worktree["reason_code"], "dirty_worktree");
        assert_eq!(worktree["classification_changed"], true);
    }

    #[test]
    fn safe_refusals_do_not_fail_the_run() {
        let mut summary = Summary::empty(CommandName::Scan, false);
        summary.refusals = 2;
        assert_eq!(summary.exit_code(), 0);
        summary.operational_failures = 1;
        assert_eq!(summary.exit_code(), 1);
    }
}
