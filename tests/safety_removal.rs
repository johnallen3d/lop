use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    repository: PathBuf,
    remote: PathBuf,
    config_home: PathBuf,
    state_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let sources = directory.path().join("sources");
        let repository = sources.join("repository");
        let remote = sources.join("origin.git");
        let config_home = directory.path().join("config");
        let state_home = directory.path().join("state");

        fs::create_dir_all(&sources).unwrap();
        git(
            directory.path(),
            &["init", "--quiet", "--bare", path(&remote)],
        );
        git(
            directory.path(),
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                path(&repository),
            ],
        );
        git(&repository, &["config", "user.name", "Lop Test"]);
        git(
            &repository,
            &["config", "user.email", "lop@example.invalid"],
        );
        git(&repository, &["config", "commit.gpgsign", "false"]);
        fs::write(repository.join("base.txt"), "base\n").unwrap();
        git(&repository, &["add", "base.txt"]);
        git(&repository, &["commit", "--quiet", "-m", "base"]);
        git(&repository, &["remote", "add", "origin", path(&remote)]);
        git(&repository, &["push", "--quiet", "-u", "origin", "main"]);

        fs::create_dir_all(config_home.join("lop")).unwrap();
        fs::write(
            config_home.join("lop/config.toml"),
            format!(
                "roots = [{:?}]\nscan_depth = 2\nfetch_timeout_seconds = 10\ncheck_processes = false\n",
                sources.display().to_string()
            ),
        )
        .unwrap();

        Self {
            directory,
            repository,
            remote,
            config_home,
            state_home,
        }
    }

    fn integrated_worktree(&self, branch: &str) -> PathBuf {
        let worktree = self.directory.path().join("worktrees").join(branch);
        fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                path(&worktree),
                "main",
            ],
        );
        git(
            &self.repository,
            &["push", "--quiet", "-u", "origin", branch],
        );
        git(
            &self.remote,
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        );
        fs::canonicalize(worktree).unwrap()
    }

    #[cfg(target_os = "macos")]
    fn enable_process_checks(&self) {
        let path = self.config_home.join("lop/config.toml");
        let config = fs::read_to_string(&path).unwrap();
        fs::write(
            path,
            config.replace("check_processes = false", "check_processes = true"),
        )
        .unwrap();
    }

    fn prune(&self) -> Output {
        self.run(&["prune", "--yes"], None)
    }

    fn prune_with_path(&self, path: Option<&std::ffi::OsStr>) -> Output {
        self.run(&["prune", "--yes"], path)
    }

    fn scan_with_path(&self, path: Option<&std::ffi::OsStr>) -> Output {
        self.run(&["scan"], path)
    }

    fn run(&self, arguments: &[&str], path: Option<&std::ffi::OsStr>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lop"));
        command
            .args(arguments)
            .current_dir(self.directory.path())
            .env("HOME", self.directory.path())
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_PANE_ID");
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    }
}

#[test]
fn successful_cleanup_removes_checkout_and_integrated_local_branch() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("finished");

    let output = fixture.prune();

    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!worktree.exists());
    assert!(!ref_exists(&fixture.repository, "refs/heads/finished"));
    let records = records(&output);
    let removed = worktree_record(&records, &worktree);
    assert_eq!(removed["outcome"], "removed");
    assert_eq!(removed["reason_code"], "branch_deleted");
    assert_eq!(records.last().unwrap()["removals"], 1);
}

#[test]
fn failed_repository_is_untouched_while_healthy_repository_is_pruned() {
    let fixture = Fixture::new();
    let healthy_worktree = fixture.integrated_worktree("healthy-finished");
    let sources = fixture.repository.parent().unwrap();
    let broken_repository = sources.join("broken");
    git(
        sources,
        &[
            "init",
            "--quiet",
            "--initial-branch=main",
            path(&broken_repository),
        ],
    );
    git(&broken_repository, &["config", "user.name", "Lop Test"]);
    git(
        &broken_repository,
        &["config", "user.email", "lop@example.invalid"],
    );
    git(&broken_repository, &["config", "commit.gpgsign", "false"]);
    fs::write(broken_repository.join("base.txt"), "base\n").unwrap();
    git(&broken_repository, &["add", "base.txt"]);
    git(&broken_repository, &["commit", "--quiet", "-m", "base"]);
    git(
        &broken_repository,
        &[
            "remote",
            "add",
            "origin",
            path(&sources.join("missing.git")),
        ],
    );
    let failed_worktree = fixture.directory.path().join("worktrees/failed-repository");
    git(
        &broken_repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "candidate",
            path(&failed_worktree),
            "main",
        ],
    );
    let failed_worktree = fs::canonicalize(failed_worktree).unwrap();

    let output = fixture.prune();

    assert!(!output.status.success());
    assert!(!healthy_worktree.exists());
    assert!(failed_worktree.exists());
    assert!(ref_exists(&broken_repository, "refs/heads/candidate"));
    let records = records(&output);
    let failed_record = worktree_record(&records, &failed_worktree);
    assert_eq!(failed_record["outcome"], "skipped");
    assert_eq!(failed_record["reason_code"], "fetch_failed");
    assert_eq!(records.last().unwrap()["repositories_scanned"], 2);
    assert_eq!(records.last().unwrap()["removals"], 1);
}

#[test]
fn dirty_staged_and_untracked_files_survive_cleanup_attempts() {
    let fixture = Fixture::new();
    let modified = fixture.integrated_worktree("modified");
    let staged = fixture.integrated_worktree("staged");
    let untracked = fixture.integrated_worktree("untracked");
    fs::write(modified.join("base.txt"), "modified\n").unwrap();
    fs::write(staged.join("staged.txt"), "staged\n").unwrap();
    git(&staged, &["add", "staged.txt"]);
    fs::write(untracked.join("untracked.txt"), "untracked\n").unwrap();

    let output = fixture.prune();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = records(&output);
    for worktree in [&modified, &staged, &untracked] {
        assert!(worktree.exists());
        assert_eq!(worktree_record(&records, worktree)["outcome"], "refused");
        assert_eq!(
            worktree_record(&records, worktree)["reason_code"],
            "dirty_worktree"
        );
    }
    assert_eq!(
        fs::read_to_string(modified.join("base.txt")).unwrap(),
        "modified\n"
    );
    assert_eq!(
        fs::read_to_string(staged.join("staged.txt")).unwrap(),
        "staged\n"
    );
    assert_eq!(
        fs::read_to_string(untracked.join("untracked.txt")).unwrap(),
        "untracked\n"
    );
    assert!(ref_exists(&fixture.repository, "refs/heads/modified"));
    assert!(ref_exists(&fixture.repository, "refs/heads/staged"));
    assert!(ref_exists(&fixture.repository, "refs/heads/untracked"));
}

#[cfg(target_os = "macos")]
#[test]
fn live_process_current_directory_refuses_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("in-use");
    fixture.enable_process_checks();
    let mut process = Command::new("sleep")
        .arg("30")
        .current_dir(&worktree)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let output = fixture.prune();
    let _ = process.kill();
    let _ = process.wait();

    assert!(output.status.success());
    assert!(worktree.exists());
    assert!(ref_exists(&fixture.repository, "refs/heads/in-use"));
    let records = records(&output);
    let record = worktree_record(&records, &worktree);
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["reason_code"], "process_using_worktree");
}

#[test]
fn locked_worktree_is_refused_without_invoking_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("locked");
    git(&fixture.repository, &["worktree", "lock", path(&worktree)]);

    let output = fixture.prune();

    assert!(output.status.success());
    assert!(worktree.exists());
    assert!(ref_exists(&fixture.repository, "refs/heads/locked"));
    let records = records(&output);
    let record = worktree_record(&records, &worktree);
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["reason_code"], "locked_worktree");
}

#[cfg(unix)]
#[test]
fn herdr_retirement_failure_preserves_worktree_and_branch() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-cleanup-failure");
    let bin = fixture.directory.path().join("bin");
    let snapshot_path = fixture.directory.path().join("herdr-snapshot.json");
    let close_log = fixture.directory.path().join("herdr-close.log");
    fs::create_dir(&bin).unwrap();
    fs::write(
        &snapshot_path,
        serde_json::to_vec(&json!({
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
                        "cwd": worktree,
                        "foreground_cwd": worktree,
                        "agent": "pi",
                        "agent_status": "idle"
                    }],
                    "agents": [{
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "cwd": worktree,
                        "foreground_cwd": worktree,
                        "agent_status": "idle"
                    }],
                    "workspaces": [{
                        "workspace_id": "w1",
                        "worktree": {"checkout_path": worktree}
                    }]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let herdr = bin.join("herdr");
    fs::write(
        &herdr,
        format!(
            "#!/bin/sh\nif [ \"$1 $2 $3\" = 'session list --json' ]; then printf '%s' '{{\"sessions\":[{{\"name\":\"test\",\"running\":true,\"socket_path\":\"/tmp/test.sock\"}}]}}'; elif [ \"$1 $2 $3 $4\" = '--session test api snapshot' ]; then cat '{}'; elif [ \"$1 $2 $3 $4\" = '--session test pane process-info' ]; then printf '%s' '{{\"id\":\"test\",\"result\":{{\"type\":\"pane_process_info\",\"process_info\":{{\"pane_id\":\"w1:p1\",\"shell_pid\":41,\"foreground_processes\":[]}}}}}}'; else printf '%s\\n' \"$*\" > '{}'; echo retirement-failed >&2; exit 1; fi\n",
            snapshot_path.display(),
            close_log.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&herdr, permissions).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();

    let output = fixture.prune_with_path(Some(&path));

    assert!(!output.status.success());
    assert!(worktree.exists());
    assert!(ref_exists(
        &fixture.repository,
        "refs/heads/herdr-cleanup-failure"
    ));
    let records = records(&output);
    let record = worktree_record(&records, &worktree);
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["reason_code"], "herdr_retirement_failed");
    assert!(
        record["message"]
            .as_str()
            .unwrap()
            .contains("retirement-failed")
    );
    assert_eq!(records.last().unwrap()["removals"], 0);
    assert_eq!(records.last().unwrap()["operational_failures"], 1);
    assert_eq!(
        fs::read_to_string(close_log).unwrap().trim(),
        "--session test workspace close w1"
    );
}

#[cfg(unix)]
#[test]
fn lop_previews_and_retires_stale_herdr_workspace() {
    let fixture = Fixture::new();
    let checkout = fixture.integrated_worktree("herdr-stale");
    git(
        &fixture.repository,
        &["worktree", "remove", path(&checkout)],
    );
    git(&fixture.repository, &["branch", "-d", "herdr-stale"]);
    assert!(!checkout.exists());
    let initial = stale_herdr_snapshot(&fixture.repository, &checkout);
    let after = empty_herdr_snapshot();
    let (path, close_marker) = install_stateful_herdr(&fixture, &initial, &after, 41, "true");

    let preview = fixture.scan_with_path(Some(&path));

    assert!(preview.status.success());
    assert!(!close_marker.exists());
    let preview_record = worktree_record(&records(&preview), &checkout).clone();
    assert_eq!(preview_record["outcome"], "candidate");
    assert_eq!(
        preview_record["reason_code"],
        "herdr_stale_workspace_pending"
    );

    let apply = fixture.prune_with_path(Some(&path));

    assert!(
        apply.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(close_marker.exists());
    let apply_record = worktree_record(&records(&apply), &checkout).clone();
    assert_eq!(apply_record["outcome"], "removed");
    assert_eq!(apply_record["reason_code"], "herdr_stale_workspace_retired");
}

#[cfg(unix)]
#[test]
fn scheduled_scan_discovers_herdr_without_inherited_session_context() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-preview");
    let initial = herdr_snapshot(&worktree, false, "idle");
    let after = empty_herdr_snapshot();
    let (path, close_marker) = install_stateful_herdr(&fixture, &initial, &after, 41, "true");

    let output = fixture.scan_with_path(Some(&path));

    assert!(output.status.success());
    assert!(worktree.exists());
    assert!(!close_marker.exists());
    let record = worktree_record(&records(&output), &worktree).clone();
    assert_eq!(record["outcome"], "candidate");
    assert_eq!(record["reason_code"], "herdr_coordination_pending");
}

#[cfg(unix)]
#[test]
fn idle_herdr_workspace_is_retired_before_successful_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-idle-success");
    let initial = herdr_snapshot(&worktree, false, "done");
    let after = empty_herdr_snapshot();
    let (path, close_marker) = install_stateful_herdr(&fixture, &initial, &after, 41, "true");

    let output = fixture.prune_with_path(Some(&path));

    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(close_marker.exists());
    assert!(!worktree.exists());
    assert!(!ref_exists(
        &fixture.repository,
        "refs/heads/herdr-idle-success"
    ));
    assert_eq!(
        worktree_record(&records(&output), &worktree)["outcome"],
        "removed"
    );
}

#[cfg(unix)]
#[test]
fn focus_race_after_workspace_retirement_refuses_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-focus-race");
    let initial = herdr_snapshot(&worktree, false, "idle");
    let after = herdr_snapshot(&worktree, true, "idle");
    let (path, close_marker) = install_stateful_herdr(&fixture, &initial, &after, 40, "true");

    let output = fixture.prune_with_path(Some(&path));

    assert!(output.status.success());
    assert!(close_marker.exists());
    assert!(worktree.exists());
    assert!(ref_exists(
        &fixture.repository,
        "refs/heads/herdr-focus-race"
    ));
    let record = worktree_record(&records(&output), &worktree).clone();
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["reason_code"], "herdr_focused_pane");
}

#[cfg(unix)]
#[test]
fn agent_race_after_workspace_retirement_refuses_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-agent-race");
    let initial = herdr_snapshot(&worktree, false, "idle");
    let after = herdr_snapshot(&worktree, false, "working");
    let (path, close_marker) = install_stateful_herdr(&fixture, &initial, &after, 41, "true");

    let output = fixture.prune_with_path(Some(&path));

    assert!(output.status.success());
    assert!(close_marker.exists());
    assert!(worktree.exists());
    assert!(ref_exists(
        &fixture.repository,
        "refs/heads/herdr-agent-race"
    ));
    let record = worktree_record(&records(&output), &worktree).clone();
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["reason_code"], "herdr_active_agent");
}

#[cfg(target_os = "macos")]
#[test]
fn attributed_idle_herdr_process_is_retired_before_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-idle-process");
    fixture.enable_process_checks();
    let mut idle_process = Command::new("sleep")
        .arg("30")
        .current_dir(&worktree)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let process_id = idle_process.id();
    let initial = herdr_snapshot(&worktree, false, "idle");
    let after = empty_herdr_snapshot();
    let close_action = format!("kill {process_id}; sleep 0.1");
    let (path, close_marker) =
        install_stateful_herdr(&fixture, &initial, &after, process_id, &close_action);

    let output = fixture.prune_with_path(Some(&path));
    let _ = idle_process.wait();

    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(close_marker.exists());
    assert!(!worktree.exists());
    assert!(!ref_exists(
        &fixture.repository,
        "refs/heads/herdr-idle-process"
    ));
    assert_eq!(
        worktree_record(&records(&output), &worktree)["outcome"],
        "removed"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn process_appearing_during_herdr_retirement_refuses_removal() {
    let fixture = Fixture::new();
    let worktree = fixture.integrated_worktree("herdr-process-race");
    fixture.enable_process_checks();
    let initial = herdr_snapshot(&worktree, false, "idle");
    let after = empty_herdr_snapshot();
    let process_file = fixture.directory.path().join("racing-process.pid");
    let close_action = format!(
        "(cd '{}' && exec sleep 30) >/dev/null 2>&1 </dev/null & echo $! > '{}'; sleep 0.1",
        worktree.display(),
        process_file.display()
    );
    let (path, close_marker) =
        install_stateful_herdr(&fixture, &initial, &after, 999_999, &close_action);

    let output = fixture.prune_with_path(Some(&path));
    if let Ok(process_id) = fs::read_to_string(&process_file).map(|value| value.trim().to_owned()) {
        let _ = Command::new("kill").arg(process_id).status();
    }

    assert!(output.status.success());
    assert!(close_marker.exists());
    assert!(worktree.exists());
    assert!(ref_exists(
        &fixture.repository,
        "refs/heads/herdr-process-race"
    ));
    let record = worktree_record(&records(&output), &worktree).clone();
    assert_eq!(record["outcome"], "refused");
    assert_eq!(record["reason_code"], "process_using_worktree");
}

#[cfg(unix)]
fn herdr_snapshot(worktree: &Path, focused: bool, status: &str) -> Value {
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
                    "cwd": worktree,
                    "foreground_cwd": worktree,
                    "agent": "pi",
                    "agent_status": status
                }],
                "agents": [{
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "cwd": worktree,
                    "foreground_cwd": worktree,
                    "agent_status": status
                }],
                "workspaces": [{
                    "workspace_id": "w1",
                    "worktree": {"checkout_path": worktree}
                }]
            }
        }
    })
}

#[cfg(unix)]
fn stale_herdr_snapshot(repository: &Path, checkout: &Path) -> Value {
    json!({
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
                    "cwd": checkout,
                    "foreground_cwd": checkout,
                    "agent": null,
                    "agent_status": "unknown"
                }],
                "agents": [],
                "workspaces": [{
                    "workspace_id": "w1",
                    "worktree": {
                        "checkout_path": checkout,
                        "is_linked_worktree": true,
                        "repo_key": repository.join(".git")
                    }
                }]
            }
        }
    })
}

#[cfg(unix)]
fn empty_herdr_snapshot() -> Value {
    json!({
        "id": "test",
        "result": {
            "type": "session_snapshot",
            "snapshot": {
                "version": "0.9.0",
                "protocol": 22,
                "focused_pane_id": null,
                "panes": [],
                "agents": [],
                "workspaces": []
            }
        }
    })
}

#[cfg(unix)]
fn install_stateful_herdr(
    fixture: &Fixture,
    initial: &Value,
    after: &Value,
    shell_pid: u32,
    close_action: &str,
) -> (std::ffi::OsString, PathBuf) {
    let bin = fixture.directory.path().join(format!("bin-{shell_pid}"));
    let initial_path = fixture
        .directory
        .path()
        .join(format!("herdr-initial-{shell_pid}.json"));
    let after_path = fixture
        .directory
        .path()
        .join(format!("herdr-after-{shell_pid}.json"));
    let close_marker = fixture
        .directory
        .path()
        .join(format!("herdr-close-{shell_pid}"));
    fs::create_dir(&bin).unwrap();
    fs::write(&initial_path, serde_json::to_vec(initial).unwrap()).unwrap();
    fs::write(&after_path, serde_json::to_vec(after).unwrap()).unwrap();
    let herdr = bin.join("herdr");
    fs::write(
        &herdr,
        format!(
            "#!/bin/sh\nif [ \"$1 $2 $3\" = 'session list --json' ]; then printf '%s' '{{\"sessions\":[{{\"name\":\"test\",\"running\":true,\"socket_path\":\"/tmp/test.sock\"}}]}}'; elif [ \"$1 $2 $3 $4\" = '--session test api snapshot' ]; then if [ -e '{close_marker}' ]; then cat '{after_path}'; else cat '{initial_path}'; fi; elif [ \"$1 $2 $3 $4\" = '--session test pane process-info' ]; then printf '%s' '{{\"id\":\"test\",\"result\":{{\"type\":\"pane_process_info\",\"process_info\":{{\"pane_id\":\"w1:p1\",\"shell_pid\":{shell_pid},\"foreground_processes\":[]}}}}}}'; else {close_action}; touch '{close_marker}'; printf '%s' '{{\"id\":\"test\",\"result\":{{\"type\":\"workspace_closed\",\"workspace_id\":\"w1\"}}}}'; fi\n",
            close_marker = close_marker.display(),
            after_path = after_path.display(),
            initial_path = initial_path.display(),
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&herdr, permissions).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    (std::env::join_paths(paths).unwrap(), close_marker)
}

fn records(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn worktree_record<'a>(records: &'a [Value], expected_path: &Path) -> &'a Value {
    records
        .iter()
        .find(|record| record["record"] == "worktree" && record["path"] == path(expected_path))
        .unwrap_or_else(|| panic!("no record for {} in {records:#?}", expected_path.display()))
}

fn ref_exists(repository: &Path, reference: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["show-ref", "--verify", "--quiet", reference])
        .status()
        .unwrap()
        .success()
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}
