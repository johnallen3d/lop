use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Coordination {
    workspaces: Vec<String>,
    panes: Vec<String>,
    processes: BTreeSet<u32>,
}

impl Coordination {
    #[must_use]
    pub fn requires_retirement(&self) -> bool {
        !self.workspaces.is_empty()
    }

    #[must_use]
    pub const fn attributed_processes(&self) -> &BTreeSet<u32> {
        &self.processes
    }

    #[must_use]
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CandidateStatus {
    Unavailable,
    Clear(Coordination),
    FocusedPane { pane_id: String },
    ActiveAgent { pane_id: String },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StaleWorkspace {
    pub checkout_path: PathBuf,
    pub status: CandidateStatus,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum StaleWorkspaceStatus {
    Unavailable,
    Inspected(Vec<StaleWorkspace>),
}

#[derive(Debug, Error)]
pub enum InspectionError {
    #[error("malformed Herdr snapshot JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("malformed Herdr snapshot: {0}")]
    MalformedResult(String),
    #[error("incomplete Herdr activity data: {0}")]
    IncompleteActivity(String),
    #[error("Herdr pane process inspection failed: {0}")]
    ProcessInspection(String),
}

#[derive(Debug, Error)]
#[error("Herdr workspace retirement failed: {message}")]
pub struct RetirementError {
    message: String,
}

/// Checks whether a Herdr-managed pane or agent makes `candidate` unsafe to remove.
///
/// A missing client or unavailable socket is treated as an optional integration that
/// is not active. Once Herdr returns a snapshot, malformed candidate activity fails
/// closed.
///
/// # Errors
///
/// Returns an error when an available Herdr client emits malformed or incomplete data.
pub fn inspect_candidate(
    candidate: &Path,
    timeout: Duration,
) -> Result<CandidateStatus, InspectionError> {
    inspect_candidate_with_program(candidate, timeout, Path::new("herdr"))
}

/// Closes Herdr workspaces mapped exclusively to a candidate before Git removal.
///
/// # Errors
///
/// Returns an error if any associated workspace cannot be closed or Herdr does not
/// confirm the command with structured output.
pub fn retire_candidate(
    coordination: &Coordination,
    timeout: Duration,
) -> Result<(), RetirementError> {
    retire_candidate_with_program(coordination, timeout, Path::new("herdr"))
}

/// Finds safe-to-retire Herdr workspaces whose linked Git checkout is already gone.
///
/// # Errors
///
/// Returns an error when available Herdr state is malformed, incomplete, or mixes
/// a stale checkout with unrelated pane activity.
pub fn inspect_stale_workspaces(
    git_common_directory: &Path,
    inventory_paths: &BTreeSet<PathBuf>,
    timeout: Duration,
) -> Result<StaleWorkspaceStatus, InspectionError> {
    inspect_stale_workspaces_with_program(
        git_common_directory,
        inventory_paths,
        timeout,
        Path::new("herdr"),
    )
}

fn inspect_candidate_with_program(
    candidate: &Path,
    timeout: Duration,
    program: &Path,
) -> Result<CandidateStatus, InspectionError> {
    let output = match run(program, &["api", "snapshot"], timeout) {
        CommandResult::Unavailable => return Ok(CandidateStatus::Unavailable),
        CommandResult::Output(output) if !output.status.success() => {
            return Ok(CandidateStatus::Unavailable);
        }
        CommandResult::Output(output) => output,
    };

    let envelope: SnapshotEnvelope =
        serde_json::from_slice(&output.stdout).map_err(InspectionError::MalformedJson)?;
    if envelope.result.kind != "session_snapshot" {
        return Err(InspectionError::MalformedResult(format!(
            "expected session_snapshot result, received {:?}",
            envelope.result.kind
        )));
    }
    let status = evaluate_snapshot(&envelope.result.snapshot, candidate)?;
    let CandidateStatus::Clear(mut coordination) = status else {
        return Ok(status);
    };
    coordination.processes = inspect_pane_processes(&coordination.panes, timeout, program)?;
    Ok(CandidateStatus::Clear(coordination))
}

fn inspect_stale_workspaces_with_program(
    git_common_directory: &Path,
    inventory_paths: &BTreeSet<PathBuf>,
    timeout: Duration,
    program: &Path,
) -> Result<StaleWorkspaceStatus, InspectionError> {
    let output = match run(program, &["api", "snapshot"], timeout) {
        CommandResult::Unavailable => return Ok(StaleWorkspaceStatus::Unavailable),
        CommandResult::Output(output) if !output.status.success() => {
            return Ok(StaleWorkspaceStatus::Unavailable);
        }
        CommandResult::Output(output) => output,
    };
    let envelope: SnapshotEnvelope =
        serde_json::from_slice(&output.stdout).map_err(InspectionError::MalformedJson)?;
    if envelope.result.kind != "session_snapshot" {
        return Err(InspectionError::MalformedResult(format!(
            "expected session_snapshot result, received {:?}",
            envelope.result.kind
        )));
    }
    validate_snapshot(&envelope.result.snapshot)?;

    let mut stale = Vec::new();
    for workspace in &envelope.result.snapshot.workspaces {
        let Some(worktree) = workspace.worktree.as_ref() else {
            continue;
        };
        if worktree.is_linked_worktree != Some(true)
            || !worktree
                .repo_key
                .as_deref()
                .is_some_and(|repo_key| paths_equal(repo_key, git_common_directory))
            || inventory_paths.contains(&worktree.checkout_path)
            || worktree.checkout_path.exists()
        {
            continue;
        }
        let status = evaluate_stale_workspace(
            &envelope.result.snapshot,
            workspace,
            worktree,
            git_common_directory,
        )?;
        let status = match status {
            CandidateStatus::Clear(mut coordination) => {
                coordination.processes =
                    inspect_pane_processes(&coordination.panes, timeout, program)?;
                CandidateStatus::Clear(coordination)
            }
            status => status,
        };
        stale.push(StaleWorkspace {
            checkout_path: worktree.checkout_path.clone(),
            status,
        });
    }
    Ok(StaleWorkspaceStatus::Inspected(stale))
}

fn evaluate_snapshot(
    snapshot: &Snapshot,
    candidate: &Path,
) -> Result<CandidateStatus, InspectionError> {
    validate_snapshot(snapshot)?;

    let candidate_workspaces = candidate_workspaces(snapshot, candidate);
    validate_candidate_workspaces(snapshot, candidate, &candidate_workspaces)?;
    let candidate_panes: Vec<_> = snapshot
        .panes
        .iter()
        .filter(|pane| {
            pane_matches(pane, candidate)
                || candidate_workspaces.contains(pane.workspace_id.as_str())
        })
        .collect();

    for pane in &candidate_panes {
        if pane.focused
            || snapshot
                .focused_pane_id
                .as_deref()
                .is_some_and(|focused| focused == pane.id)
        {
            return Ok(CandidateStatus::FocusedPane {
                pane_id: pane.id.clone(),
            });
        }
        if pane.agent.is_some()
            && let Some(veto) = activity_veto(pane.status, &pane.id, "pane")?
        {
            return Ok(veto);
        }
    }

    for agent in snapshot.agents.iter().filter(|agent| {
        path_option_is_within(agent.cwd.as_deref(), candidate)
            || path_option_is_within(agent.foreground_cwd.as_deref(), candidate)
            || candidate_panes.iter().any(|pane| pane.id == agent.id)
            || candidate_workspaces.contains(agent.workspace_id.as_str())
    }) {
        if let Some(veto) = activity_veto(agent.status, &agent.id, "agent")? {
            return Ok(veto);
        }
    }

    Ok(CandidateStatus::Clear(Coordination {
        workspaces: cleanup_workspace_ids(snapshot, candidate, candidate_workspaces),
        panes: candidate_panes.iter().map(|pane| pane.id.clone()).collect(),
        processes: BTreeSet::new(),
    }))
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), InspectionError> {
    if snapshot.version.trim().is_empty() || snapshot.protocol == 0 {
        Err(InspectionError::MalformedResult(
            "snapshot omitted a valid version or protocol".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn evaluate_stale_workspace(
    snapshot: &Snapshot,
    workspace: &Workspace,
    worktree: &WorkspaceWorktree,
    git_common_directory: &Path,
) -> Result<CandidateStatus, InspectionError> {
    let panes: Vec<_> = snapshot
        .panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace.workspace_id)
        .collect();
    if panes.is_empty() {
        return Err(InspectionError::IncompleteActivity(format!(
            "stale workspace {} has no pane activity data",
            workspace.workspace_id
        )));
    }
    for pane in &panes {
        if pane.focused
            || snapshot
                .focused_pane_id
                .as_deref()
                .is_some_and(|focused| focused == pane.id)
        {
            return Ok(CandidateStatus::FocusedPane {
                pane_id: pane.id.clone(),
            });
        }
        if !stale_pane_belongs_to_checkout(pane, &worktree.checkout_path, git_common_directory) {
            return Err(InspectionError::IncompleteActivity(format!(
                "stale workspace {} contains unrelated pane {}",
                workspace.workspace_id, pane.id
            )));
        }
        if pane.agent.is_some()
            && let Some(veto) = activity_veto(pane.status, &pane.id, "pane")?
        {
            return Ok(veto);
        }
    }
    for agent in snapshot
        .agents
        .iter()
        .filter(|agent| agent.workspace_id == workspace.workspace_id)
    {
        if let Some(veto) = activity_veto(agent.status, &agent.id, "agent")? {
            return Ok(veto);
        }
    }
    Ok(CandidateStatus::Clear(Coordination {
        workspaces: vec![workspace.workspace_id.clone()],
        panes: panes.iter().map(|pane| pane.id.clone()).collect(),
        processes: BTreeSet::new(),
    }))
}

fn stale_pane_belongs_to_checkout(
    pane: &Pane,
    checkout_path: &Path,
    git_common_directory: &Path,
) -> bool {
    let paths = [pane.cwd.as_deref(), pane.foreground_cwd.as_deref()];
    paths.into_iter().flatten().all(|path| {
        path_is_within(path, checkout_path)
            || path_is_stale_worktrunk_trash(path, checkout_path, git_common_directory)
    }) && (pane.cwd.is_some() || pane.foreground_cwd.is_some())
}

fn path_is_stale_worktrunk_trash(
    path: &Path,
    checkout_path: &Path,
    git_common_directory: &Path,
) -> bool {
    let trash = git_common_directory.join("wt/trash");
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let trash = fs::canonicalize(&trash).unwrap_or(trash);
    let Some(name) = resolved.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(checkout_name) = checkout_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    resolved.starts_with(trash) && name.starts_with(&format!("{checkout_name}-"))
}

fn candidate_workspaces<'a>(snapshot: &'a Snapshot, candidate: &Path) -> BTreeSet<&'a str> {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| {
            workspace
                .worktree
                .as_ref()
                .is_some_and(|worktree| path_is_within(&worktree.checkout_path, candidate))
        })
        .map(|workspace| workspace.workspace_id.as_str())
        .collect()
}

fn activity_veto(
    status: AgentStatus,
    pane_id: &str,
    source: &str,
) -> Result<Option<CandidateStatus>, InspectionError> {
    match status {
        AgentStatus::Working | AgentStatus::Blocked => Ok(Some(CandidateStatus::ActiveAgent {
            pane_id: pane_id.to_owned(),
        })),
        AgentStatus::Unknown => Err(InspectionError::IncompleteActivity(format!(
            "{source} in pane {pane_id} has unknown state"
        ))),
        AgentStatus::Idle | AgentStatus::Done => Ok(None),
    }
}

fn validate_candidate_workspaces(
    snapshot: &Snapshot,
    candidate: &Path,
    candidate_workspaces: &BTreeSet<&str>,
) -> Result<(), InspectionError> {
    for (workspace_id, panes) in panes_by_workspace(&snapshot.panes) {
        let metadata_match = candidate_workspaces.contains(workspace_id);
        let has_candidate_pane = panes.iter().any(|pane| pane_matches(pane, candidate));
        if !metadata_match && !has_candidate_pane {
            continue;
        }
        if panes.is_empty() {
            return Err(InspectionError::IncompleteActivity(format!(
                "workspace {workspace_id} has no pane activity data"
            )));
        }
        if let Some(pane) = panes
            .iter()
            .find(|pane| pane.cwd.is_none() && pane.foreground_cwd.is_none())
        {
            return Err(InspectionError::IncompleteActivity(format!(
                "pane {} has no current-directory data",
                pane.id
            )));
        }
        if let Some(pane) = panes.iter().find(|pane| !pane_matches(pane, candidate)) {
            return Err(InspectionError::IncompleteActivity(format!(
                "workspace {workspace_id} contains unrelated pane {}",
                pane.id
            )));
        }
    }

    for workspace_id in candidate_workspaces {
        if !snapshot
            .panes
            .iter()
            .any(|pane| pane.workspace_id == *workspace_id)
        {
            return Err(InspectionError::IncompleteActivity(format!(
                "workspace {workspace_id} has no pane activity data"
            )));
        }
    }
    Ok(())
}

fn cleanup_workspace_ids<'a>(
    snapshot: &'a Snapshot,
    candidate: &Path,
    mut workspace_ids: BTreeSet<&'a str>,
) -> Vec<String> {
    for (workspace_id, panes) in panes_by_workspace(&snapshot.panes) {
        let has_candidate_pane = panes.iter().any(|pane| pane_matches(pane, candidate));
        let all_panes_belong_to_candidate = panes.iter().all(|pane| {
            (pane.cwd.is_some() || pane.foreground_cwd.is_some()) && pane_matches(pane, candidate)
        });
        if has_candidate_pane && all_panes_belong_to_candidate {
            workspace_ids.insert(workspace_id);
        }
    }
    workspace_ids.into_iter().map(str::to_owned).collect()
}

fn inspect_pane_processes(
    pane_ids: &[String],
    timeout: Duration,
    program: &Path,
) -> Result<BTreeSet<u32>, InspectionError> {
    let mut process_ids = BTreeSet::new();
    for pane_id in pane_ids {
        let output = match run(
            program,
            &["pane", "process-info", "--pane", pane_id],
            timeout,
        ) {
            CommandResult::Unavailable => {
                return Err(InspectionError::ProcessInspection(format!(
                    "pane {pane_id}: client or socket became unavailable"
                )));
            }
            CommandResult::Output(output) if !output.status.success() => {
                return Err(InspectionError::ProcessInspection(format!(
                    "pane {pane_id}: {}",
                    stderr_message(&output.stderr)
                )));
            }
            CommandResult::Output(output) => output,
        };
        let envelope: ProcessInfoEnvelope =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                InspectionError::ProcessInspection(format!(
                    "pane {pane_id}: malformed JSON: {error}"
                ))
            })?;
        if envelope.result.kind != "pane_process_info"
            || envelope.result.process_info.pane_id != *pane_id
        {
            return Err(InspectionError::ProcessInspection(format!(
                "pane {pane_id}: malformed process-info result"
            )));
        }
        let info = envelope.result.process_info;
        let shell_pid = info.shell_pid.ok_or_else(|| {
            InspectionError::ProcessInspection(format!("pane {pane_id}: shell PID is missing"))
        })?;
        process_ids.insert(shell_pid);
        process_ids.extend(
            info.foreground_processes
                .into_iter()
                .map(|process| process.pid),
        );
    }
    Ok(process_ids)
}

fn retire_candidate_with_program(
    coordination: &Coordination,
    timeout: Duration,
    program: &Path,
) -> Result<(), RetirementError> {
    let mut failures = Vec::new();
    for workspace_id in &coordination.workspaces {
        match run(program, &["workspace", "close", workspace_id], timeout) {
            CommandResult::Unavailable => failures.push(format!(
                "workspace {workspace_id}: client or socket became unavailable"
            )),
            CommandResult::Output(output) if !output.status.success() => failures.push(format!(
                "workspace {workspace_id}: {}",
                stderr_message(&output.stderr)
            )),
            CommandResult::Output(output) => {
                let valid = serde_json::from_slice::<AcknowledgementEnvelope>(&output.stdout)
                    .is_ok_and(|response| {
                        response.result.kind == "ok"
                            || (response.result.kind == "workspace_closed"
                                && response.result.workspace_id.as_deref() == Some(workspace_id))
                    });
                if !valid {
                    failures.push(format!(
                        "workspace {workspace_id}: malformed command response"
                    ));
                }
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(RetirementError {
            message: failures.join("; "),
        })
    }
}

fn panes_by_workspace(panes: &[Pane]) -> BTreeMap<&str, Vec<&Pane>> {
    let mut grouped = BTreeMap::<_, Vec<_>>::new();
    for pane in panes {
        grouped
            .entry(pane.workspace_id.as_str())
            .or_default()
            .push(pane);
    }
    grouped
}

fn pane_matches(pane: &Pane, candidate: &Path) -> bool {
    path_option_is_within(pane.cwd.as_deref(), candidate)
        || path_option_is_within(pane.foreground_cwd.as_deref(), candidate)
}

fn path_option_is_within(path: Option<&Path>, candidate: &Path) -> bool {
    path.is_some_and(|path| path_is_within(path, candidate))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn path_is_within(path: &Path, candidate: &Path) -> bool {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    resolved == candidate || resolved.starts_with(candidate)
}

#[derive(Debug, Deserialize)]
struct SnapshotEnvelope {
    result: SnapshotResult,
}

#[derive(Debug, Deserialize)]
struct SnapshotResult {
    #[serde(rename = "type")]
    kind: String,
    snapshot: Snapshot,
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    version: String,
    protocol: u32,
    focused_pane_id: Option<String>,
    panes: Vec<Pane>,
    agents: Vec<Agent>,
    workspaces: Vec<Workspace>,
}

#[derive(Debug, Deserialize)]
struct Pane {
    #[serde(rename = "pane_id")]
    id: String,
    workspace_id: String,
    focused: bool,
    cwd: Option<PathBuf>,
    foreground_cwd: Option<PathBuf>,
    agent: Option<String>,
    #[serde(rename = "agent_status")]
    status: AgentStatus,
}

#[derive(Debug, Deserialize)]
struct Agent {
    #[serde(rename = "pane_id")]
    id: String,
    workspace_id: String,
    cwd: Option<PathBuf>,
    foreground_cwd: Option<PathBuf>,
    #[serde(rename = "agent_status")]
    status: AgentStatus,
}

#[derive(Debug, Deserialize)]
struct Workspace {
    workspace_id: String,
    worktree: Option<WorkspaceWorktree>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceWorktree {
    checkout_path: PathBuf,
    is_linked_worktree: Option<bool>,
    repo_key: Option<PathBuf>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoEnvelope {
    result: ProcessInfoResult,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoResult {
    #[serde(rename = "type")]
    kind: String,
    process_info: PaneProcessInfo,
}

#[derive(Debug, Deserialize)]
struct PaneProcessInfo {
    pane_id: String,
    shell_pid: Option<u32>,
    #[serde(default)]
    foreground_processes: Vec<PaneProcess>,
}

#[derive(Debug, Deserialize)]
struct PaneProcess {
    pid: u32,
}

#[derive(Debug, Deserialize)]
struct AcknowledgementEnvelope {
    result: Acknowledgement,
}

#[derive(Debug, Deserialize)]
struct Acknowledgement {
    #[serde(rename = "type")]
    kind: String,
    workspace_id: Option<String>,
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum CommandResult {
    Unavailable,
    Output(ProcessOutput),
}

fn run(program: &Path, arguments: &[&str], timeout: Duration) -> CommandResult {
    let Ok(mut child) = Command::new(program)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return CommandResult::Unavailable;
    };
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let started = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return CommandResult::Unavailable;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => return CommandResult::Unavailable,
        }
    };
    let Ok(Ok(stdout)) = stdout_reader.join() else {
        return CommandResult::Unavailable;
    };
    let Ok(Ok(stderr)) = stderr_reader.join() else {
        return CommandResult::Unavailable;
    };
    CommandResult::Output(ProcessOutput {
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
    use std::{collections::BTreeSet, fs, path::Path, time::Duration};

    use serde_json::{Value, json};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    use super::{
        CandidateStatus, InspectionError, StaleWorkspaceStatus, inspect_candidate_with_program,
        inspect_stale_workspaces_with_program, retire_candidate_with_program,
    };

    fn snapshot(candidate: &Path, focused: bool, agent: Option<(&str, &str)>) -> Value {
        let agent_name = agent.map(|(name, _)| name);
        let agent_status = agent.map_or("unknown", |(_, status)| status);
        let agents = agent.map_or_else(Vec::new, |(_name, status)| {
            vec![json!({
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "cwd": candidate,
                "foreground_cwd": candidate,
                "agent_status": status
            })]
        });
        json!({
            "id": "test",
            "result": {
                "type": "session_snapshot",
                "snapshot": {
                    "version": "0.9.0",
                    "protocol": 22,
                    "focused_pane_id": if focused { Some("w1:p1") } else { None },
                    "panes": [{
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "focused": focused,
                        "cwd": candidate,
                        "foreground_cwd": candidate,
                        "agent": agent_name,
                        "agent_status": agent_status
                    }],
                    "agents": agents,
                    "workspaces": [{
                        "workspace_id": "w1",
                        "worktree": {"checkout_path": candidate}
                    }]
                }
            }
        })
    }

    #[cfg(unix)]
    fn executable(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn snapshot_program(directory: &Path, name: &str, snapshot: &Value) -> std::path::PathBuf {
        let snapshot = serde_json::to_string(snapshot).unwrap();
        let process_info = json!({
            "id": "test",
            "result": {
                "type": "pane_process_info",
                "process_info": {
                    "pane_id": "w1:p1",
                    "shell_pid": 41,
                    "foreground_processes": [{"pid": 42}]
                }
            }
        });
        executable(
            directory,
            name,
            &format!(
                "if [ \"$1\" = api ]; then cat <<'JSON'\n{snapshot}\nJSON\nelif [ \"$1 $2\" = 'pane process-info' ]; then cat <<'JSON'\n{process_info}\nJSON\nelse printf '%s' '{{\"id\":\"test\",\"result\":{{\"type\":\"ok\"}}}}'; fi"
            ),
        )
    }

    #[test]
    fn missing_client_is_optional() {
        let fixture = tempdir().unwrap();
        assert_eq!(
            inspect_candidate_with_program(
                fixture.path(),
                Duration::from_millis(100),
                &fixture.path().join("missing-herdr")
            )
            .unwrap(),
            CandidateStatus::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_socket_is_optional() {
        let fixture = tempdir().unwrap();
        let program = executable(fixture.path(), "herdr", "echo unavailable >&2; exit 1");
        assert_eq!(
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &program)
                .unwrap(),
            CandidateStatus::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn focused_pane_and_active_agent_veto_removal() {
        let fixture = tempdir().unwrap();
        for (name, value, expected) in [
            ("focused", snapshot(fixture.path(), true, None), "focused"),
            (
                "working",
                snapshot(fixture.path(), false, Some(("pi", "working"))),
                "active",
            ),
        ] {
            let json = serde_json::to_string(&value).unwrap();
            let program = executable(fixture.path(), name, &format!("cat <<'JSON'\n{json}\nJSON"));
            let result =
                inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &program)
                    .unwrap();
            assert_eq!(
                matches!(result, CandidateStatus::FocusedPane { .. }),
                expected == "focused"
            );
            assert_eq!(
                matches!(result, CandidateStatus::ActiveAgent { .. }),
                expected == "active"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn idle_workspace_is_clear_and_selected_for_cleanup() {
        let fixture = tempdir().unwrap();
        let value = snapshot(fixture.path(), false, Some(("pi", "idle")));
        let program = snapshot_program(fixture.path(), "idle", &value);
        let result =
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &program)
                .unwrap();
        let CandidateStatus::Clear(coordination) = result else {
            panic!("expected clear status");
        };
        assert_eq!(coordination.workspaces, ["w1"]);
        assert_eq!(coordination.processes, BTreeSet::from([41, 42]));
    }

    #[cfg(unix)]
    #[test]
    fn mixed_path_workspace_fails_closed() {
        let fixture = tempdir().unwrap();
        let mut value = snapshot(fixture.path(), false, Some(("pi", "idle")));
        value["result"]["snapshot"]["panes"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "pane_id": "w1:p2",
                "workspace_id": "w1",
                "focused": false,
                "cwd": "/tmp/unrelated",
                "foreground_cwd": "/tmp/unrelated",
                "agent": null,
                "agent_status": "unknown"
            }));
        let program = snapshot_program(fixture.path(), "mixed", &value);

        let error =
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &program)
                .unwrap_err();

        assert!(matches!(error, InspectionError::IncompleteActivity(_)));
        assert!(error.to_string().contains("unrelated pane"));
    }

    #[cfg(unix)]
    #[test]
    fn stale_worktrunk_trash_workspace_is_detected() {
        let fixture = tempdir().unwrap();
        let git_common = fixture.path().join("repository/.git");
        let checkout = fixture.path().join("repository/.worktrees/finished");
        let trash = git_common.join("wt/trash/finished-1234");
        fs::create_dir_all(&trash).unwrap();
        let value = json!({
            "id": "test",
            "result": {
                "type": "session_snapshot",
                "snapshot": {
                    "version": "0.9.0",
                    "protocol": 22,
                    "focused_pane_id": null,
                    "panes": [{
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "focused": false,
                        "cwd": trash,
                        "foreground_cwd": trash,
                        "agent": null,
                        "agent_status": "unknown"
                    }],
                    "agents": [],
                    "workspaces": [{
                        "workspace_id": "w1",
                        "worktree": {
                            "checkout_path": checkout,
                            "is_linked_worktree": true,
                            "repo_key": git_common
                        }
                    }]
                }
            }
        });
        let program = snapshot_program(fixture.path(), "stale", &value);

        let status = inspect_stale_workspaces_with_program(
            &git_common,
            &BTreeSet::new(),
            Duration::from_secs(1),
            &program,
        )
        .unwrap();

        let StaleWorkspaceStatus::Inspected(stale) = status else {
            panic!("expected available Herdr status");
        };
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].checkout_path, checkout);
        assert!(matches!(stale[0].status, CandidateStatus::Clear(_)));
    }

    #[cfg(unix)]
    #[test]
    fn malformed_and_incomplete_activity_fail_closed() {
        let fixture = tempdir().unwrap();
        let malformed = executable(fixture.path(), "malformed", "printf '{'");
        assert!(matches!(
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &malformed),
            Err(InspectionError::MalformedJson(_))
        ));

        let value = snapshot(fixture.path(), false, Some(("pi", "unknown")));
        let json = serde_json::to_string(&value).unwrap();
        let incomplete = executable(
            fixture.path(),
            "incomplete",
            &format!("cat <<'JSON'\n{json}\nJSON"),
        );
        assert!(matches!(
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &incomplete),
            Err(InspectionError::IncompleteActivity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn candidate_retirement_reports_success_and_failure() {
        let fixture = tempdir().unwrap();
        let value = snapshot(fixture.path(), false, Some(("pi", "idle")));
        let success = snapshot_program(fixture.path(), "success", &value);
        let CandidateStatus::Clear(coordination) =
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &success)
                .unwrap()
        else {
            panic!("expected clear status");
        };
        retire_candidate_with_program(&coordination, Duration::from_secs(1), &success).unwrap();

        let failure = executable(fixture.path(), "failure", "echo close-failed >&2; exit 1");
        let error = retire_candidate_with_program(&coordination, Duration::from_secs(1), &failure)
            .unwrap_err();
        assert!(error.to_string().contains("close-failed"));
    }
}
