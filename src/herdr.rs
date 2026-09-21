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
    workspace_ids: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CandidateStatus {
    Unavailable,
    Clear(Coordination),
    FocusedPane { pane_id: String },
    ActiveAgent { pane_id: String },
}

#[derive(Debug, Error)]
pub enum InspectionError {
    #[error("malformed Herdr snapshot JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("malformed Herdr snapshot: {0}")]
    MalformedResult(String),
    #[error("incomplete Herdr activity data: {0}")]
    IncompleteActivity(String),
}

#[derive(Debug, Error)]
#[error("Herdr workspace cleanup failed: {message}")]
pub struct CleanupError {
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

/// Closes Herdr workspaces that were mapped exclusively to a removed checkout.
///
/// # Errors
///
/// Returns an error if any associated workspace cannot be closed or Herdr does not
/// confirm the command with structured output.
pub fn cleanup_removed(coordination: &Coordination, timeout: Duration) -> Result<(), CleanupError> {
    cleanup_removed_with_program(coordination, timeout, Path::new("herdr"))
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
    evaluate_snapshot(&envelope.result.snapshot, candidate)
}

fn evaluate_snapshot(
    snapshot: &Snapshot,
    candidate: &Path,
) -> Result<CandidateStatus, InspectionError> {
    if snapshot.version.trim().is_empty() || snapshot.protocol == 0 {
        return Err(InspectionError::MalformedResult(
            "snapshot omitted a valid version or protocol".to_owned(),
        ));
    }

    let candidate_workspaces = candidate_workspaces(snapshot, candidate);
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

    validate_candidate_workspaces(snapshot, &candidate_workspaces)?;
    Ok(CandidateStatus::Clear(Coordination {
        workspace_ids: cleanup_workspace_ids(snapshot, candidate, candidate_workspaces),
    }))
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
    candidate_workspaces: &BTreeSet<&str>,
) -> Result<(), InspectionError> {
    for workspace_id in candidate_workspaces {
        let panes: Vec<_> = snapshot
            .panes
            .iter()
            .filter(|pane| pane.workspace_id == *workspace_id)
            .collect();
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

fn cleanup_removed_with_program(
    coordination: &Coordination,
    timeout: Duration,
    program: &Path,
) -> Result<(), CleanupError> {
    let mut failures = Vec::new();
    for workspace_id in &coordination.workspace_ids {
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
                    .is_ok_and(|response| response.result.kind == "ok");
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
        Err(CleanupError {
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
struct AcknowledgementEnvelope {
    result: Acknowledgement,
}

#[derive(Debug, Deserialize)]
struct Acknowledgement {
    #[serde(rename = "type")]
    kind: String,
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
    use std::{fs, path::Path, time::Duration};

    use serde_json::{Value, json};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    use super::{
        CandidateStatus, InspectionError, cleanup_removed_with_program,
        inspect_candidate_with_program,
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
        let json = serde_json::to_string(&value).unwrap();
        let program = executable(
            fixture.path(),
            "idle",
            &format!("cat <<'JSON'\n{json}\nJSON"),
        );
        let result =
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &program)
                .unwrap();
        let CandidateStatus::Clear(coordination) = result else {
            panic!("expected clear status");
        };
        assert_eq!(coordination.workspace_ids, ["w1"]);
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
    fn post_removal_cleanup_reports_success_and_failure() {
        let fixture = tempdir().unwrap();
        let value = snapshot(fixture.path(), false, Some(("pi", "idle")));
        let json = serde_json::to_string(&value).unwrap();
        let success = executable(
            fixture.path(),
            "success",
            &format!(
                "if [ \"$1\" = api ]; then cat <<'JSON'\n{json}\nJSON\nelse printf '%s' '{{\"id\":\"test\",\"result\":{{\"type\":\"ok\"}}}}'; fi"
            ),
        );
        let CandidateStatus::Clear(coordination) =
            inspect_candidate_with_program(fixture.path(), Duration::from_secs(1), &success)
                .unwrap()
        else {
            panic!("expected clear status");
        };
        cleanup_removed_with_program(&coordination, Duration::from_secs(1), &success).unwrap();

        let failure = executable(fixture.path(), "failure", "echo close-failed >&2; exit 1");
        let error = cleanup_removed_with_program(&coordination, Duration::from_secs(1), &failure)
            .unwrap_err();
        assert!(error.to_string().contains("close-failed"));
    }
}
