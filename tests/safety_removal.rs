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
        self.prune_with_path(None)
    }

    fn prune_with_path(&self, path: Option<&std::ffi::OsStr>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lop"));
        command
            .args(["prune", "--yes"])
            .current_dir(self.directory.path())
            .env("HOME", self.directory.path())
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
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
fn herdr_cleanup_failure_preserves_completed_removal_result() {
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
            "#!/bin/sh\nif [ \"$1\" = api ]; then cat '{}'; else printf '%s\\n' \"$*\" > '{}'; echo cleanup-failed >&2; exit 1; fi\n",
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
    assert!(!worktree.exists());
    assert!(!ref_exists(
        &fixture.repository,
        "refs/heads/herdr-cleanup-failure"
    ));
    let records = records(&output);
    let record = worktree_record(&records, &worktree);
    assert_eq!(record["outcome"], "removed");
    assert_eq!(record["reason_code"], "herdr_cleanup_failed");
    assert!(
        record["message"]
            .as_str()
            .unwrap()
            .contains("cleanup-failed")
    );
    assert_eq!(records.last().unwrap()["removals"], 1);
    assert_eq!(records.last().unwrap()["operational_failures"], 1);
    assert_eq!(
        fs::read_to_string(close_log).unwrap().trim(),
        "workspace close w1"
    );
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
