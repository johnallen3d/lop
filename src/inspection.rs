use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RepositoryInspectionOutcome {
    Inspected,
    FetchFailed,
    FetchTimedOut,
    MalformedPorcelain,
    InspectionFailed,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum WorktreeClassification {
    MainWorktree,
    UpstreamExists,
    NoUpstream,
    DetachedWorktree,
    DanglingSymbolicHead,
    PrunableWorktree,
    UpstreamMissing,
    FetchFailed,
    FetchTimedOut,
    Malformed,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WorktreeInspection {
    pub path: PathBuf,
    pub classification: WorktreeClassification,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RepositoryInspection {
    pub outcome: RepositoryInspectionOutcome,
    pub message: Option<String>,
    pub worktrees: Vec<WorktreeInspection>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PorcelainWorktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    detached: bool,
    prunable: bool,
    bare: bool,
}

/// Fetches and classifies all worktrees belonging to a repository.
///
/// A failed fetch prevents all branch/upstream classification. Worktree inventory is
/// still emitted, but every entry records the fetch failure instead of a removal
/// decision.
#[must_use]
pub fn inspect_repository(repository: &Path, fetch_timeout: Duration) -> RepositoryInspection {
    inspect_repository_with_git(repository, fetch_timeout, Path::new("git"))
}

fn inspect_repository_with_git(
    repository: &Path,
    fetch_timeout: Duration,
    git_program: &Path,
) -> RepositoryInspection {
    let fetch = run_git(
        git_program,
        repository,
        &["fetch", "--all", "--prune", "--no-recurse-submodules"],
        fetch_timeout,
        true,
    );

    let list = run_git(
        git_program,
        repository,
        &["worktree", "list", "--porcelain", "-z"],
        fetch_timeout,
        false,
    );
    let output = match list {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return inspection_failed(format!(
                "git worktree list failed: {}",
                stderr_message(&output.stderr)
            ));
        }
        Err(RunError::TimedOut) => {
            return inspection_failed("git worktree list timed out".to_owned());
        }
        Err(RunError::Spawn(error)) => {
            return inspection_failed(format!("failed to run git worktree list: {error}"));
        }
    };

    let worktrees = match parse_porcelain(&output.stdout) {
        Ok(worktrees) => worktrees,
        Err(message) => return malformed_porcelain(message),
    };

    match fetch {
        Err(RunError::TimedOut) => RepositoryInspection {
            outcome: RepositoryInspectionOutcome::FetchTimedOut,
            message: Some(format!(
                "git fetch exceeded timeout of {} seconds",
                fetch_timeout.as_secs()
            )),
            worktrees: classify_without_fetch(worktrees, WorktreeClassification::FetchTimedOut),
        },
        Err(RunError::Spawn(error)) => RepositoryInspection {
            outcome: RepositoryInspectionOutcome::FetchFailed,
            message: Some(format!("failed to run git fetch: {error}")),
            worktrees: classify_without_fetch(worktrees, WorktreeClassification::FetchFailed),
        },
        Ok(output) if !output.status.success() => RepositoryInspection {
            outcome: RepositoryInspectionOutcome::FetchFailed,
            message: Some(format!(
                "git fetch failed: {}",
                stderr_message(&output.stderr)
            )),
            worktrees: classify_without_fetch(worktrees, WorktreeClassification::FetchFailed),
        },
        Ok(_) => classify_after_fetch(repository, git_program, fetch_timeout, worktrees),
    }
}

fn classify_without_fetch(
    worktrees: Vec<PorcelainWorktree>,
    classification: WorktreeClassification,
) -> Vec<WorktreeInspection> {
    worktrees
        .into_iter()
        .map(|worktree| WorktreeInspection {
            path: worktree.path,
            classification,
        })
        .collect()
}

fn classify_after_fetch(
    repository: &Path,
    git_program: &Path,
    timeout: Duration,
    worktrees: Vec<PorcelainWorktree>,
) -> RepositoryInspection {
    let mut inspected = Vec::with_capacity(worktrees.len());
    let mut issue = None;

    for (index, worktree) in worktrees.into_iter().enumerate() {
        let classification = if index == 0 || worktree.bare {
            WorktreeClassification::MainWorktree
        } else if worktree.prunable {
            WorktreeClassification::PrunableWorktree
        } else if worktree.detached {
            WorktreeClassification::DetachedWorktree
        } else if let Some(branch) = worktree.branch.as_deref() {
            match ref_exists(git_program, repository, branch, timeout) {
                Ok(false) if is_zero_oid(&worktree.head) => {
                    WorktreeClassification::DanglingSymbolicHead
                }
                Ok(false) => WorktreeClassification::DanglingSymbolicHead,
                Err(message) => {
                    issue.get_or_insert(message);
                    WorktreeClassification::Malformed
                }
                Ok(true) => match branch_upstream(git_program, repository, branch, timeout) {
                    Ok(None) => WorktreeClassification::NoUpstream,
                    Ok(Some(upstream)) => {
                        match ref_exists(git_program, repository, &upstream, timeout) {
                            Ok(true) => WorktreeClassification::UpstreamExists,
                            Ok(false) => WorktreeClassification::UpstreamMissing,
                            Err(message) => {
                                issue.get_or_insert(message);
                                WorktreeClassification::Malformed
                            }
                        }
                    }
                    Err(message) => {
                        issue.get_or_insert(message);
                        WorktreeClassification::Malformed
                    }
                },
            }
        } else {
            issue.get_or_insert_with(|| {
                format!(
                    "worktree {} is neither detached nor associated with a branch",
                    worktree.path.display()
                )
            });
            WorktreeClassification::Malformed
        };

        inspected.push(WorktreeInspection {
            path: worktree.path,
            classification,
        });
    }

    if let Some(message) = issue {
        RepositoryInspection {
            outcome: RepositoryInspectionOutcome::InspectionFailed,
            message: Some(message),
            worktrees: inspected,
        }
    } else {
        RepositoryInspection {
            outcome: RepositoryInspectionOutcome::Inspected,
            message: None,
            worktrees: inspected,
        }
    }
}

fn ref_exists(
    git_program: &Path,
    repository: &Path,
    reference: &str,
    timeout: Duration,
) -> Result<bool, String> {
    let output = run_git(
        git_program,
        repository,
        &["show-ref", "--verify", "--quiet", "--", reference],
        timeout,
        false,
    )
    .map_err(|error| command_error("git show-ref", error))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git show-ref failed for {reference}: {}",
            stderr_message(&output.stderr)
        )),
    }
}

fn branch_upstream(
    git_program: &Path,
    repository: &Path,
    branch: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let output = run_git(
        git_program,
        repository,
        &["for-each-ref", "--format=%(upstream)", "--count=1", branch],
        timeout,
        false,
    )
    .map_err(|error| command_error("git for-each-ref", error))?;
    if !output.status.success() {
        return Err(format!(
            "git for-each-ref failed for {branch}: {}",
            stderr_message(&output.stderr)
        ));
    }
    let upstream = String::from_utf8(output.stdout)
        .map_err(|error| format!("git returned a non-UTF-8 upstream for {branch}: {error}"))?;
    let upstream = upstream.trim();
    Ok((!upstream.is_empty()).then(|| upstream.to_owned()))
}

fn parse_porcelain(input: &[u8]) -> Result<Vec<PorcelainWorktree>, String> {
    if !input.ends_with(&[0, 0]) {
        return Err("malformed worktree porcelain: record is not NUL-terminated".to_owned());
    }

    let mut parsed = Vec::new();
    let mut fields = Vec::new();

    for field in input.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !fields.is_empty() {
                parsed.push(parse_record(&fields)?);
                fields.clear();
            }
        } else {
            fields.push(field);
        }
    }
    if !fields.is_empty() {
        return Err("malformed worktree porcelain: record is not NUL-terminated".to_owned());
    }
    if parsed.is_empty() {
        return Err("malformed worktree porcelain: no worktrees were returned".to_owned());
    }
    Ok(parsed)
}

fn parse_record(fields: &[&[u8]]) -> Result<PorcelainWorktree, String> {
    let first = fields
        .first()
        .ok_or_else(|| "malformed worktree porcelain: empty record".to_owned())?;
    let path = first.strip_prefix(b"worktree ").ok_or_else(|| {
        "malformed worktree porcelain: record does not start with worktree".to_owned()
    })?;
    if path.is_empty() {
        return Err("malformed worktree porcelain: empty worktree path".to_owned());
    }

    let mut head = None;
    let mut branch = None;
    let mut detached = false;
    let mut prunable = false;
    let mut bare = false;

    for field in &fields[1..] {
        if let Some(value) = field.strip_prefix(b"HEAD ") {
            set_once(&mut head, ascii(value, "HEAD")?, "HEAD")?;
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            set_once(&mut branch, ascii(value, "branch")?, "branch")?;
        } else if *field == b"detached" {
            if detached {
                return Err("malformed worktree porcelain: duplicate detached field".to_owned());
            }
            detached = true;
        } else if *field == b"bare" {
            if bare {
                return Err("malformed worktree porcelain: duplicate bare field".to_owned());
            }
            bare = true;
        } else if *field == b"prunable" || field.starts_with(b"prunable ") {
            if prunable {
                return Err("malformed worktree porcelain: duplicate prunable field".to_owned());
            }
            prunable = true;
        } else if *field == b"locked" || field.starts_with(b"locked ") {
            // Locking is a later safety gate, but it is valid porcelain here.
        } else {
            return Err(format!(
                "malformed worktree porcelain: unknown field {:?}",
                String::from_utf8_lossy(field)
            ));
        }
    }

    let head = head.ok_or_else(|| "malformed worktree porcelain: missing HEAD".to_owned())?;
    if detached && branch.is_some() {
        return Err(
            "malformed worktree porcelain: worktree is both detached and on a branch".to_owned(),
        );
    }

    Ok(PorcelainWorktree {
        path: path_from_bytes(path)?,
        head,
        branch,
        detached,
        prunable,
        bare,
    })
}

fn set_once(slot: &mut Option<String>, value: String, name: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!(
            "malformed worktree porcelain: duplicate {name} field"
        ));
    }
    Ok(())
}

fn ascii(value: &[u8], name: &str) -> Result<String, String> {
    if value.is_empty() || !value.is_ascii() {
        return Err(format!(
            "malformed worktree porcelain: invalid {name} value"
        ));
    }
    Ok(String::from_utf8(value.to_vec()).expect("ASCII is valid UTF-8"))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // The non-Unix implementation can reject paths.
fn path_from_bytes(value: &[u8]) -> Result<PathBuf, String> {
    Ok(PathBuf::from(OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(value: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(value.to_vec())
        .map(PathBuf::from)
        .map_err(|error| format!("malformed worktree porcelain: non-UTF-8 path: {error}"))
}

fn is_zero_oid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte == b'0')
}

fn malformed_porcelain(message: String) -> RepositoryInspection {
    RepositoryInspection {
        outcome: RepositoryInspectionOutcome::MalformedPorcelain,
        message: Some(message),
        worktrees: Vec::new(),
    }
}

fn inspection_failed(message: String) -> RepositoryInspection {
    RepositoryInspection {
        outcome: RepositoryInspectionOutcome::InspectionFailed,
        message: Some(message),
        worktrees: Vec::new(),
    }
}

#[derive(Debug)]
struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
enum RunError {
    Spawn(std::io::Error),
    TimedOut,
}

fn run_git(
    git_program: &Path,
    repository: &Path,
    arguments: &[&str],
    timeout: Duration,
    is_fetch: bool,
) -> Result<GitOutput, RunError> {
    let mut command = Command::new(git_program);
    command
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if is_fetch {
        command.env("GIT_TERMINAL_PROMPT", "0");
    }

    let mut child = command.spawn().map_err(RunError::Spawn)?;
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
                // Readers may still be draining descriptors inherited by a Git child.
                // Dropping the handles detaches them rather than extending the timeout.
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(RunError::TimedOut);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| RunError::Spawn(std::io::Error::other("stdout reader panicked")))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| RunError::Spawn(std::io::Error::other("stderr reader panicked")))??;
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_all(mut stream: impl Read) -> Result<Vec<u8>, RunError> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output).map_err(RunError::Spawn)?;
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

fn command_error(command: &str, error: RunError) -> String {
    match error {
        RunError::Spawn(error) => format!("failed to run {command}: {error}"),
        RunError::TimedOut => format!("{command} timed out"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::{
        RepositoryInspectionOutcome, WorktreeClassification, inspect_repository,
        inspect_repository_with_git, is_zero_oid, parse_porcelain,
    };

    fn git(repository: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "--quiet", "--initial-branch=main"]);
        git(path, &["config", "user.name", "Lop Test"]);
        git(path, &["config", "user.email", "lop@example.invalid"]);
        fs::write(path.join("README"), "fixture\n").unwrap();
        git(path, &["add", "README"]);
        git(path, &["commit", "--quiet", "-m", "fixture"]);
    }

    fn add_worktree(repository: &Path, root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        git(
            repository,
            &["worktree", "add", "--quiet", path.to_str().unwrap(), name],
        );
        path
    }

    #[test]
    fn parses_nul_porcelain_with_unusual_paths_and_metadata() {
        let input = b"worktree /tmp/main path\0HEAD abc123\0branch refs/heads/main\0locked reason here\0\0worktree /tmp/gone\npath\0HEAD 0000000000000000000000000000000000000000\0branch refs/heads/gone\0prunable gitdir file points to non-existent location\0\0";
        let parsed = parse_porcelain(input).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, Path::new("/tmp/main path"));
        assert_eq!(parsed[1].path, Path::new("/tmp/gone\npath"));
        assert!(parsed[1].prunable);
        assert!(is_zero_oid(&parsed[1].head));
    }

    #[test]
    fn rejects_malformed_porcelain() {
        for input in [
            &b"HEAD abc\0\0"[..],
            &b"worktree /tmp/a\0branch refs/heads/a\0\0"[..],
            &b"worktree /tmp/a\0HEAD abc\0mystery value\0\0"[..],
            &b"worktree /tmp/a\0HEAD abc\0detached\0branch refs/heads/a\0\0"[..],
            &b"worktree /tmp/a\0HEAD abc\0"[..],
        ] {
            assert!(parse_porcelain(input).is_err(), "input: {input:?}");
        }
    }

    #[test]
    fn classifies_all_worktree_states_after_fetching_multiple_remotes() {
        let fixture = tempdir().unwrap();
        let root = fs::canonicalize(fixture.path()).unwrap();
        let repository = root.join("repository");
        let worktrees = root.join("unusual worktree paths");
        let origin = root.join("origin.git");
        let secondary = root.join("secondary.git");
        init_repository(&repository);
        git(
            &root,
            &["init", "--bare", "--quiet", origin.to_str().unwrap()],
        );
        git(
            &root,
            &["init", "--bare", "--quiet", secondary.to_str().unwrap()],
        );
        git(
            &repository,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repository, &["push", "--quiet", "-u", "origin", "main"]);
        git(
            &repository,
            &["remote", "add", "secondary", secondary.to_str().unwrap()],
        );

        for branch in ["keep", "gone", "local", "dangling", "prunable", "second"] {
            git(&repository, &["branch", branch]);
        }
        git(&repository, &["push", "--quiet", "-u", "origin", "keep"]);
        git(&repository, &["push", "--quiet", "-u", "origin", "gone"]);
        git(
            &repository,
            &["push", "--quiet", "-u", "secondary", "second"],
        );

        let keep = add_worktree(&repository, &worktrees, "keep");
        let gone = add_worktree(&repository, &worktrees, "gone");
        let local = add_worktree(&repository, &worktrees, "local");
        let dangling = add_worktree(&repository, &worktrees, "dangling");
        let prunable = add_worktree(&repository, &worktrees, "prunable");
        let second = add_worktree(&repository, &worktrees, "second");
        let detached = worktrees.join("detached checkout");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                detached.to_str().unwrap(),
                "HEAD",
            ],
        );

        git(
            &root,
            &[
                "--git-dir",
                origin.to_str().unwrap(),
                "update-ref",
                "-d",
                "refs/heads/gone",
            ],
        );
        git(&repository, &["update-ref", "-d", "refs/heads/dangling"]);
        fs::remove_dir_all(&prunable).unwrap();

        let result = inspect_repository(&repository, Duration::from_secs(5));
        assert_eq!(result.outcome, RepositoryInspectionOutcome::Inspected);
        let by_path: BTreeMap<_, _> = result
            .worktrees
            .into_iter()
            .map(|item| (item.path, item.classification))
            .collect();
        assert_eq!(
            by_path[&fs::canonicalize(&repository).unwrap()],
            WorktreeClassification::MainWorktree
        );
        assert_eq!(by_path[&keep], WorktreeClassification::UpstreamExists);
        assert_eq!(by_path[&second], WorktreeClassification::UpstreamExists);
        assert_eq!(by_path[&gone], WorktreeClassification::UpstreamMissing);
        assert_eq!(by_path[&local], WorktreeClassification::NoUpstream);
        assert_eq!(by_path[&detached], WorktreeClassification::DetachedWorktree);
        assert_eq!(
            by_path[&dangling],
            WorktreeClassification::DanglingSymbolicHead
        );
        assert_eq!(by_path[&prunable], WorktreeClassification::PrunableWorktree);
    }

    #[test]
    fn fetch_failure_suppresses_all_removal_classification() {
        let fixture = tempdir().unwrap();
        let repository = fixture.path().join("repository");
        init_repository(&repository);
        git(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                fixture.path().join("missing.git").to_str().unwrap(),
            ],
        );

        let result = inspect_repository(&repository, Duration::from_secs(2));
        assert_eq!(result.outcome, RepositoryInspectionOutcome::FetchFailed);
        assert_eq!(result.worktrees.len(), 1);
        assert!(
            result
                .worktrees
                .iter()
                .all(|item| item.classification == WorktreeClassification::FetchFailed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn fetch_timeout_suppresses_all_removal_classification() {
        let fixture = tempdir().unwrap();
        let repository = fixture.path().join("repository");
        init_repository(&repository);
        let fake_git = fixture.path().join("slow-git");
        fs::write(
            &fake_git,
            "#!/bin/sh\ncase \" $* \" in\n  *\" fetch \"*) sleep 2 ;;\n  *) exec git \"$@\" ;;\nesac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_git, permissions).unwrap();

        let result =
            inspect_repository_with_git(&repository, Duration::from_millis(250), &fake_git);
        assert_eq!(result.outcome, RepositoryInspectionOutcome::FetchTimedOut);
        assert_eq!(result.worktrees.len(), 1);
        assert_eq!(
            result.worktrees[0].classification,
            WorktreeClassification::FetchTimedOut
        );
    }
}
