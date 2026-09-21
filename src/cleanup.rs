use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use thiserror::Error;

use crate::{
    inspection::{InventoryWorktree, WorktreeInspection, worktree_inventory},
    output::ReasonCode,
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SafetyRefusal {
    pub reason_code: ReasonCode,
    pub message: String,
    pub operational_failure: bool,
}

pub trait ProcessInspector {
    /// Reports whether a live process has its current directory at or below `worktree`.
    ///
    /// # Errors
    ///
    /// Returns an error when process information cannot be obtained completely and safely.
    fn worktree_in_use(&self, worktree: &Path, timeout: Duration) -> Result<bool, String>;
}

#[derive(Debug, Copy, Clone, Default)]
pub struct PlatformProcessInspector;

impl ProcessInspector for PlatformProcessInspector {
    fn worktree_in_use(&self, worktree: &Path, timeout: Duration) -> Result<bool, String> {
        platform_worktree_in_use(worktree, timeout)
    }
}

/// Rechecks mutable worktree safety facts immediately before a possible removal.
///
/// # Errors
///
/// Returns a fail-closed refusal when any safety fact is negative or cannot be inspected.
pub fn check_candidate(
    repository: &Path,
    candidate: &WorktreeInspection,
    check_processes: bool,
    timeout: Duration,
) -> Result<(), SafetyRefusal> {
    check_candidate_with_inspector(
        repository,
        candidate,
        check_processes,
        timeout,
        &PlatformProcessInspector,
    )
}

fn check_candidate_with_inspector(
    repository: &Path,
    candidate: &WorktreeInspection,
    check_processes: bool,
    timeout: Duration,
    process_inspector: &impl ProcessInspector,
) -> Result<(), SafetyRefusal> {
    let inventory = worktree_inventory(repository, timeout)
        .map_err(|message| inspection_failure("worktree inventory", &message))?;
    let current = exact_inventory_entry(&inventory, &candidate.path)?;

    if inventory
        .first()
        .is_some_and(|main| main.path == current.path)
    {
        return Err(refusal(
            ReasonCode::MainWorktree,
            "the main worktree is never removable",
        ));
    }
    if current.locked {
        return Err(refusal(
            ReasonCode::LockedWorktree,
            "the worktree is locked",
        ));
    }
    if current.detached || current.branch.is_none() {
        return Err(refusal(
            ReasonCode::DetachedWorktree,
            "the worktree no longer has an attached branch",
        ));
    }
    if current.prunable || current.bare {
        return Err(refusal(
            ReasonCode::InvalidGitMetadata,
            "the worktree metadata is prunable or bare",
        ));
    }
    if current.head != candidate.head || current.branch != candidate.branch {
        return Err(refusal(
            ReasonCode::WorktreeChanged,
            "the worktree branch or HEAD changed after classification",
        ));
    }

    let same_branch = inventory
        .iter()
        .filter(|worktree| worktree.branch == current.branch)
        .count();
    if same_branch != 1 {
        return Err(refusal(
            ReasonCode::CheckedOutMultipleTimes,
            "the branch is checked out in more than one worktree",
        ));
    }

    let canonical = fs::canonicalize(&candidate.path).map_err(|error| {
        refusal(
            ReasonCode::WorktreeMissing,
            format!("cannot resolve worktree path: {error}"),
        )
    })?;
    if canonical != candidate.path {
        return Err(refusal(
            ReasonCode::WorktreeChanged,
            "the worktree path changed after classification",
        ));
    }

    let status = run_process(
        Path::new("git"),
        &[
            "-C",
            &candidate.path.to_string_lossy(),
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
        timeout,
    )
    .map_err(|error| inspection_failure("git status", &error.to_string()))?;
    if !status.status.success() {
        return Err(inspection_failure(
            "git status",
            &stderr_message(&status.stderr),
        ));
    }
    if !status.stdout.is_empty() {
        return Err(refusal(
            ReasonCode::DirtyWorktree,
            "the worktree has staged, modified, or untracked files",
        ));
    }

    if check_processes {
        match process_inspector.worktree_in_use(&canonical, timeout) {
            Ok(true) => {
                return Err(refusal(
                    ReasonCode::ProcessUsingWorktree,
                    "a live process has its current directory in the worktree",
                ));
            }
            Ok(false) => {}
            Err(message) => {
                return Err(inspection_failure("process inspection", &message));
            }
        }
    }

    Ok(())
}

fn exact_inventory_entry<'a>(
    inventory: &'a [InventoryWorktree],
    path: &Path,
) -> Result<&'a InventoryWorktree, SafetyRefusal> {
    let mut matches = inventory.iter().filter(|worktree| worktree.path == path);
    let Some(worktree) = matches.next() else {
        return Err(refusal(
            ReasonCode::WorktreeMissing,
            "the worktree disappeared after classification",
        ));
    };
    if matches.next().is_some() {
        return Err(inspection_failure(
            "worktree inventory",
            "multiple records matched the candidate path",
        ));
    }
    Ok(worktree)
}

fn refusal(reason_code: ReasonCode, message: impl Into<String>) -> SafetyRefusal {
    SafetyRefusal {
        reason_code,
        message: message.into(),
        operational_failure: false,
    }
}

fn inspection_failure(context: &str, message: &str) -> SafetyRefusal {
    SafetyRefusal {
        reason_code: ReasonCode::SafetyInspectionFailed,
        message: format!("{context} failed: {message}"),
        operational_failure: true,
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchOutcome {
    Deleted,
    Deferred,
    NotAttempted,
    RetainedUnmerged,
    RetainedCheckedOut,
    RetainedRaced,
    RetainedFailed,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RemovalResult {
    pub branch_outcome: BranchOutcome,
    pub branch_checked_out_at: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum RemovalError {
    #[error("Worktrunk is unavailable: {0}")]
    Unavailable(std::io::Error),
    #[error("Worktrunk removal timed out")]
    TimedOut,
    #[error("Worktrunk removal failed: {0}")]
    Command(String),
    #[error("malformed Worktrunk removal JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("malformed Worktrunk removal result: {0}")]
    MalformedResult(String),
}

/// Removes one worktree through Worktrunk without any force or reaping flags.
///
/// # Errors
///
/// Returns an error when Worktrunk fails, times out, or returns an ambiguous result.
pub fn remove_worktree(
    repository: &Path,
    worktree: &Path,
    timeout: Duration,
) -> Result<RemovalResult, RemovalError> {
    remove_worktree_with_program(repository, worktree, timeout, Path::new("wt"))
}

fn remove_worktree_with_program(
    repository: &Path,
    worktree: &Path,
    timeout: Duration,
    program: &Path,
) -> Result<RemovalResult, RemovalError> {
    let output = run_process(
        program,
        &[
            "-C",
            &repository.to_string_lossy(),
            "remove",
            "--foreground",
            "--format=json",
            "--yes",
            &worktree.to_string_lossy(),
        ],
        timeout,
    )
    .map_err(|error| match error {
        RunError::Spawn(error) => RemovalError::Unavailable(error),
        RunError::TimedOut => RemovalError::TimedOut,
    })?;
    if !output.status.success() {
        return Err(RemovalError::Command(stderr_message(&output.stderr)));
    }

    parse_removal(&output.stdout, worktree)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRemoval {
    kind: String,
    branch: Option<String>,
    path: PathBuf,
    branch_outcome: BranchOutcome,
    branch_checked_out_at: Option<PathBuf>,
}

fn parse_removal(bytes: &[u8], expected_path: &Path) -> Result<RemovalResult, RemovalError> {
    let rows: Vec<JsonRemoval> =
        serde_json::from_slice(bytes).map_err(RemovalError::MalformedJson)?;
    if rows.len() != 1 {
        return Err(RemovalError::MalformedResult(format!(
            "expected one result, received {}",
            rows.len()
        )));
    }
    let row = rows.into_iter().next().expect("length checked");
    if row.kind != "worktree" {
        return Err(RemovalError::MalformedResult(format!(
            "expected worktree result, received {:?}",
            row.kind
        )));
    }
    if row.path != expected_path {
        return Err(RemovalError::MalformedResult(format!(
            "result path {} did not match {}",
            row.path.display(),
            expected_path.display()
        )));
    }
    if row.branch.is_none() {
        return Err(RemovalError::MalformedResult(
            "result omitted the attached branch".to_owned(),
        ));
    }
    if row.branch_outcome == BranchOutcome::Deferred {
        return Err(RemovalError::MalformedResult(
            "foreground removal unexpectedly deferred branch deletion".to_owned(),
        ));
    }
    Ok(RemovalResult {
        branch_outcome: row.branch_outcome,
        branch_checked_out_at: row.branch_checked_out_at,
    })
}

#[cfg(target_os = "macos")]
fn platform_worktree_in_use(worktree: &Path, timeout: Duration) -> Result<bool, String> {
    let output = run_process(
        Path::new("/usr/sbin/lsof"),
        &["-n", "-w", "-a", "-d", "cwd", "-F0pn"],
        timeout,
    )
    .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "lsof exited unsuccessfully: {}",
            stderr_message(&output.stderr)
        ));
    }
    parse_lsof_cwds(&output.stdout, worktree)
}

#[cfg(not(target_os = "macos"))]
fn platform_worktree_in_use(_worktree: &Path, _timeout: Duration) -> Result<bool, String> {
    Err("process inspection is not implemented on this platform".to_owned())
}

fn parse_lsof_cwds(bytes: &[u8], worktree: &Path) -> Result<bool, String> {
    for field in bytes.split(|byte| *byte == 0 || *byte == b'\n') {
        let Some(path) = field.strip_prefix(b"n") else {
            continue;
        };
        if path.is_empty() {
            return Err("lsof returned an empty current-directory path".to_owned());
        }
        let path = path_from_bytes(path);
        if path == worktree || path.starts_with(worktree) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Error)]
enum RunError {
    #[error("failed to start command: {0}")]
    Spawn(std::io::Error),
    #[error("command timed out")]
    TimedOut,
}

fn run_process(
    program: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> Result<ProcessOutput, RunError> {
    let mut child = Command::new(program)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let started = Instant::now();

    let status = loop {
        match child.try_wait().map_err(RunError::Spawn)? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| RunError::Spawn(std::io::Error::other("stdout reader panicked")))?
        .map_err(RunError::Spawn)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| RunError::Spawn(std::io::Error::other("stderr reader panicked")))?
        .map_err(RunError::Spawn)?;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_all(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output)?;
    Ok(output)
}

fn stderr_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr);
    let message = message.trim();
    if message.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, time::Duration};

    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    use super::{
        BranchOutcome, ProcessInspector, check_candidate_with_inspector, parse_lsof_cwds,
        parse_removal, remove_worktree_with_program,
    };
    use crate::{
        inspection::{WorktreeClassification, WorktreeInspection},
        output::ReasonCode,
    };

    #[test]
    fn lsof_cwd_detection_only_matches_the_worktree_or_descendants() {
        let bytes = b"p12\0fcwd\0n/tmp/project\0\np13\0fcwd\0n/tmp/project-worktree/subdir\0\n";
        assert!(parse_lsof_cwds(bytes, Path::new("/tmp/project-worktree")).unwrap());
        assert!(!parse_lsof_cwds(bytes, Path::new("/tmp/other")).unwrap());
    }

    #[test]
    fn parses_all_foreground_branch_outcomes_and_rejects_deferred() {
        let path = Path::new("/tmp/feature");
        for (name, expected) in [
            ("deleted", BranchOutcome::Deleted),
            ("not_attempted", BranchOutcome::NotAttempted),
            ("retained_unmerged", BranchOutcome::RetainedUnmerged),
            ("retained_checked_out", BranchOutcome::RetainedCheckedOut),
            ("retained_raced", BranchOutcome::RetainedRaced),
            ("retained_failed", BranchOutcome::RetainedFailed),
        ] {
            let value = json!([{
                "kind": "worktree",
                "branch": "feature",
                "path": path,
                "branch_outcome": name,
                "branch_checked_out_at": null
            }]);
            assert_eq!(
                parse_removal(&serde_json::to_vec(&value).unwrap(), path)
                    .unwrap()
                    .branch_outcome,
                expected
            );
        }
        let deferred = json!([{
            "kind": "worktree", "branch": "feature", "path": path,
            "branch_outcome": "deferred", "branch_checked_out_at": null
        }]);
        assert!(parse_removal(&serde_json::to_vec(&deferred).unwrap(), path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn removal_invocation_is_foreground_confirmed_and_force_free() {
        let fixture = tempdir().unwrap();
        let log = fixture.path().join("arguments");
        let program = fixture.path().join("wt");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '[{{\"kind\":\"worktree\",\"branch\":\"feature\",\"path\":\"/tmp/feature\",\"branch_outcome\":\"retained_raced\",\"branch_checked_out_at\":null}}]'\n",
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();

        let result = remove_worktree_with_program(
            Path::new("/tmp/repository"),
            Path::new("/tmp/feature"),
            Duration::from_secs(2),
            &program,
        )
        .unwrap();
        assert_eq!(result.branch_outcome, BranchOutcome::RetainedRaced);
        let arguments = fs::read_to_string(log).unwrap();
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            [
                "-C",
                "/tmp/repository",
                "remove",
                "--foreground",
                "--format=json",
                "--yes",
                "/tmp/feature"
            ]
        );
        assert!(!arguments.contains("--force"));
        assert!(!arguments.contains("--reap"));
    }

    struct FailingInspector;

    impl ProcessInspector for FailingInspector {
        fn worktree_in_use(&self, _worktree: &Path, _timeout: Duration) -> Result<bool, String> {
            Err("unavailable".to_owned())
        }
    }

    #[test]
    fn unavailable_process_inspection_fails_closed() {
        let fixture = tempdir().unwrap();
        let repository = fixture.path().join("repository");
        let worktree = fixture.path().join("feature");
        git(
            fixture.path(),
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                path(&repository),
            ],
        );
        git(&repository, &["config", "user.name", "Test"]);
        git(
            &repository,
            &["config", "user.email", "test@example.invalid"],
        );
        git(&repository, &["config", "commit.gpgsign", "false"]);
        fs::write(repository.join("base"), "base\n").unwrap();
        git(&repository, &["add", "base"]);
        git(&repository, &["commit", "--quiet", "-m", "base"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                path(&worktree),
            ],
        );
        let head = git_stdout(&worktree, &["rev-parse", "HEAD"]);
        let candidate = WorktreeInspection {
            path: fs::canonicalize(&worktree).unwrap(),
            head,
            branch: Some("refs/heads/feature".to_owned()),
            classification: WorktreeClassification::IntegratedSameCommit,
        };

        let refusal = check_candidate_with_inspector(
            &repository,
            &candidate,
            true,
            Duration::from_secs(2),
            &FailingInspector,
        )
        .unwrap_err();
        assert_eq!(refusal.reason_code, ReasonCode::SafetyInspectionFailed);
        assert!(refusal.operational_failure);
        assert!(worktree.exists());
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn path(path: &Path) -> &str {
        path.to_str().unwrap()
    }
}
