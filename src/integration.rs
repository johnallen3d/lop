use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use thiserror::Error;

const REQUIRED_SCHEMA: u32 = 2;
const MINIMUM_WORKTRUNK_VERSION: Version = Version {
    major: 0,
    minor: 66,
    patch: 0,
};

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IntegrationReason {
    SameCommit,
    Ancestor,
    NoAddedChanges,
    TreesMatch,
    MergeAddsNothing,
    PatchIdMatch,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IntegrationProof {
    Integrated(IntegrationReason),
    NotIntegrated,
    Indeterminate,
    DefaultBranchUnresolved,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IntegrationCandidate {
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("Worktrunk is unavailable: {0}")]
    Unavailable(std::io::Error),
    #[error("Worktrunk command timed out")]
    TimedOut,
    #[error("Worktrunk version command failed: {0}")]
    VersionCommand(String),
    #[error("incompatible Worktrunk version {found}; version {minimum} or newer is required")]
    IncompatibleVersion { found: String, minimum: String },
    #[error("malformed Worktrunk version output: {0:?}")]
    MalformedVersion(String),
    #[error("Worktrunk list command failed: {0}")]
    ListCommand(String),
    #[error("malformed Worktrunk JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("incompatible Worktrunk JSON schema {0}; schema 2 is required")]
    IncompatibleSchema(u32),
    #[error("malformed Worktrunk result: {0}")]
    MalformedResult(String),
}

/// Uses Worktrunk's structured list result to prove whether candidates add no
/// committed changes to the repository's default branch.
///
/// The adapter requests schema 2 explicitly so user configuration and changes
/// to Worktrunk's default schema cannot silently alter Lop's interpretation.
///
/// # Errors
///
/// Returns an error when Worktrunk is missing, incompatible, fails, times out,
/// or emits malformed or ambiguous structured output.
pub fn prove_integrations(
    repository: &Path,
    candidates: &[IntegrationCandidate],
    timeout: Duration,
) -> Result<Vec<IntegrationProof>, AdapterError> {
    prove_integrations_with_program(repository, candidates, timeout, Path::new("wt"))
}

fn prove_integrations_with_program(
    repository: &Path,
    candidates: &[IntegrationCandidate],
    timeout: Duration,
    worktrunk_program: &Path,
) -> Result<Vec<IntegrationProof>, AdapterError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    validate_version(worktrunk_program, timeout)?;
    let output = run(
        worktrunk_program,
        &[
            "-C",
            &repository.to_string_lossy(),
            "--config-set",
            "list.json-schema=2",
            "list",
            "--format=json",
        ],
        timeout,
    )?;
    if !output.status.success() {
        return Err(AdapterError::ListCommand(stderr_message(&output.stderr)));
    }

    parse_proofs(&output.stdout, candidates)
}

fn validate_version(program: &Path, timeout: Duration) -> Result<(), AdapterError> {
    let output = run(program, &["--version"], timeout)?;
    if !output.status.success() {
        return Err(AdapterError::VersionCommand(stderr_message(&output.stderr)));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| AdapterError::MalformedVersion(error.to_string()))?;
    let version_text = text
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| AdapterError::MalformedVersion(text.trim().to_owned()))?;
    let version = Version::parse(version_text)
        .ok_or_else(|| AdapterError::MalformedVersion(text.trim().to_owned()))?;
    if version < MINIMUM_WORKTRUNK_VERSION {
        return Err(AdapterError::IncompatibleVersion {
            found: version.to_string(),
            minimum: MINIMUM_WORKTRUNK_VERSION.to_string(),
        });
    }
    Ok(())
}

fn parse_proofs(
    bytes: &[u8],
    candidates: &[IntegrationCandidate],
) -> Result<Vec<IntegrationProof>, AdapterError> {
    let envelope: ListEnvelope =
        serde_json::from_slice(bytes).map_err(AdapterError::MalformedJson)?;
    if envelope.schema != REQUIRED_SCHEMA {
        return Err(AdapterError::IncompatibleSchema(envelope.schema));
    }

    let Some(default_branch) = envelope.repo.default_branch.filter(|name| !name.is_empty()) else {
        return Ok(vec![
            IntegrationProof::DefaultBranchUnresolved;
            candidates.len()
        ]);
    };

    candidates
        .iter()
        .map(|candidate| proof_for_candidate(&envelope.items, &default_branch, candidate))
        .collect()
}

fn proof_for_candidate(
    items: &[ListItem],
    default_branch: &str,
    candidate: &IntegrationCandidate,
) -> Result<IntegrationProof, AdapterError> {
    let branch = candidate
        .branch
        .strip_prefix("refs/heads/")
        .unwrap_or(&candidate.branch);
    if branch == default_branch {
        return Err(AdapterError::MalformedResult(format!(
            "candidate {} is Worktrunk's default branch",
            candidate.branch
        )));
    }

    let mut matches = items.iter().filter(|item| {
        item.branch.as_deref() == Some(branch)
            && item
                .worktree
                .as_ref()
                .is_some_and(|worktree| worktree.path == candidate.path)
    });
    let Some(item) = matches.next() else {
        return Err(AdapterError::MalformedResult(format!(
            "no Worktrunk row matched branch {} at {}",
            candidate.branch,
            candidate.path.display()
        )));
    };
    if matches.next().is_some() {
        return Err(AdapterError::MalformedResult(format!(
            "multiple Worktrunk rows matched branch {} at {}",
            candidate.branch,
            candidate.path.display()
        )));
    }

    let Some(head) = item.head.as_ref() else {
        return Err(AdapterError::MalformedResult(format!(
            "Worktrunk returned no HEAD for {}",
            candidate.branch
        )));
    };
    if head.sha != candidate.head {
        return Err(AdapterError::MalformedResult(format!(
            "Worktrunk HEAD for {} changed from {} to {}",
            candidate.branch, candidate.head, head.sha
        )));
    }

    match &item.default_branch {
        Presence::Missing => Err(AdapterError::MalformedResult(format!(
            "Worktrunk omitted the default-branch relation for {}",
            candidate.branch
        ))),
        Presence::Null => Ok(IntegrationProof::Indeterminate),
        Presence::Value(relation) => match relation.integration {
            Presence::Missing => Ok(IntegrationProof::NotIntegrated),
            Presence::Null => Ok(IntegrationProof::Indeterminate),
            Presence::Value(integration) => {
                Ok(IntegrationProof::Integrated(integration.reason.into()))
            }
        },
    }
}

#[derive(Debug, Deserialize)]
struct ListEnvelope {
    schema: u32,
    repo: ListRepository,
    #[allow(dead_code)]
    collected: Collected,
    items: Vec<ListItem>,
}

#[derive(Debug, Deserialize)]
struct ListRepository {
    #[serde(default)]
    default_branch: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Collected {
    ci: bool,
    summary: bool,
}

#[derive(Debug, Deserialize)]
struct ListItem {
    branch: Option<String>,
    head: Option<Head>,
    #[serde(default)]
    worktree: Option<ListWorktree>,
    #[serde(default)]
    default_branch: Presence<DefaultBranchRelation>,
}

#[derive(Debug, Deserialize)]
struct Head {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct ListWorktree {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct DefaultBranchRelation {
    #[serde(default)]
    integration: Presence<Integration>,
}

#[derive(Debug, Copy, Clone, Deserialize)]
struct Integration {
    reason: JsonIntegrationReason,
}

#[derive(Debug, Copy, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JsonIntegrationReason {
    SameCommit,
    Ancestor,
    NoAddedChanges,
    TreesMatch,
    MergeAddsNothing,
    PatchIdMatch,
}

impl From<JsonIntegrationReason> for IntegrationReason {
    fn from(value: JsonIntegrationReason) -> Self {
        match value {
            JsonIntegrationReason::SameCommit => Self::SameCommit,
            JsonIntegrationReason::Ancestor => Self::Ancestor,
            JsonIntegrationReason::NoAddedChanges => Self::NoAddedChanges,
            JsonIntegrationReason::TreesMatch => Self::TreesMatch,
            JsonIntegrationReason::MergeAddsNothing => Self::MergeAddsNothing,
            JsonIntegrationReason::PatchIdMatch => Self::PatchIdMatch,
        }
    }
}

#[derive(Debug, Copy, Clone, Default)]
enum Presence<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<'de, T> Deserialize<'de> for Presence<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

#[derive(Debug, Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(value: &str) -> Option<Self> {
        let core = value.split_once('-').map_or(value, |(core, _)| core);
        let mut parts = core.split('.');
        let version = Self {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run(
    program: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> Result<ProcessOutput, AdapterError> {
    let mut child = Command::new(program)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(AdapterError::Unavailable)?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let started = Instant::now();

    let status = loop {
        match child.try_wait().map_err(AdapterError::Unavailable)? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(AdapterError::TimedOut);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };

    let stdout = stdout_reader.join().map_err(|_| {
        AdapterError::Unavailable(std::io::Error::other("stdout reader panicked"))
    })??;
    let stderr = stderr_reader.join().map_err(|_| {
        AdapterError::Unavailable(std::io::Error::other("stderr reader panicked"))
    })??;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_all(mut stream: impl Read) -> Result<Vec<u8>, AdapterError> {
    let mut output = Vec::new();
    stream
        .read_to_end(&mut output)
        .map_err(AdapterError::Unavailable)?;
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

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::{
        AdapterError, IntegrationCandidate, IntegrationProof, IntegrationReason, parse_proofs,
        prove_integrations_with_program,
    };

    fn candidate() -> IntegrationCandidate {
        IntegrationCandidate {
            path: Path::new("/tmp/project-feature").to_path_buf(),
            branch: "refs/heads/feature".to_owned(),
            head: "0123456789012345678901234567890123456789".to_owned(),
        }
    }

    fn envelope(integration: &Value) -> Value {
        json!({
            "schema": 2,
            "repo": {"default_branch": "main"},
            "collected": {"ci": false, "summary": false},
            "items": [{
                "branch": "feature",
                "head": {"sha": "0123456789012345678901234567890123456789"},
                "worktree": {"path": "/tmp/project-feature"},
                "default_branch": {"integration": integration},
                "display": {}
            }]
        })
    }

    #[test]
    fn accepts_every_positive_integration_reason() {
        for (name, expected) in [
            ("same_commit", IntegrationReason::SameCommit),
            ("ancestor", IntegrationReason::Ancestor),
            ("no_added_changes", IntegrationReason::NoAddedChanges),
            ("trees_match", IntegrationReason::TreesMatch),
            ("merge_adds_nothing", IntegrationReason::MergeAddsNothing),
            ("patch_id_match", IntegrationReason::PatchIdMatch),
        ] {
            let bytes = serde_json::to_vec(&envelope(&json!({"reason": name}))).unwrap();
            assert_eq!(
                parse_proofs(&bytes, &[candidate()]).unwrap(),
                [IntegrationProof::Integrated(expected)]
            );
        }
    }

    #[test]
    fn distinguishes_unique_work_from_indeterminate_results() {
        let mut unique = envelope(&Value::Null);
        unique["items"][0]["default_branch"] = json!({});
        assert_eq!(
            parse_proofs(&serde_json::to_vec(&unique).unwrap(), &[candidate()]).unwrap(),
            [IntegrationProof::NotIntegrated]
        );

        assert_eq!(
            parse_proofs(
                &serde_json::to_vec(&envelope(&Value::Null)).unwrap(),
                &[candidate()]
            )
            .unwrap(),
            [IntegrationProof::Indeterminate]
        );
    }

    #[test]
    fn unresolved_default_branch_is_retained() {
        let mut value = envelope(&json!({"reason": "ancestor"}));
        value["repo"] = json!({});
        assert_eq!(
            parse_proofs(&serde_json::to_vec(&value).unwrap(), &[candidate()]).unwrap(),
            [IntegrationProof::DefaultBranchUnresolved]
        );
    }

    #[test]
    fn rejects_wrong_schema_unknown_reason_and_ambiguous_rows() {
        let mut wrong_schema = envelope(&json!({"reason": "ancestor"}));
        wrong_schema["schema"] = json!(1);
        assert!(matches!(
            parse_proofs(&serde_json::to_vec(&wrong_schema).unwrap(), &[candidate()]),
            Err(AdapterError::IncompatibleSchema(1))
        ));

        let unknown = envelope(&json!({"reason": "probably_integrated"}));
        assert!(matches!(
            parse_proofs(&serde_json::to_vec(&unknown).unwrap(), &[candidate()]),
            Err(AdapterError::MalformedJson(_))
        ));

        let mut duplicate = envelope(&json!({"reason": "ancestor"}));
        let row = duplicate["items"][0].clone();
        duplicate["items"].as_array_mut().unwrap().push(row);
        assert!(matches!(
            parse_proofs(&serde_json::to_vec(&duplicate).unwrap(), &[candidate()]),
            Err(AdapterError::MalformedResult(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_missing_old_and_malformed_worktrunk_executables() {
        let fixture = tempdir().unwrap();
        let missing = fixture.path().join("missing-wt");
        assert!(matches!(
            prove_integrations_with_program(
                fixture.path(),
                &[candidate()],
                Duration::from_secs(1),
                &missing
            ),
            Err(AdapterError::Unavailable(_))
        ));

        for (name, version, expected_old) in [
            ("old", "wt 0.65.0", true),
            ("malformed", "worktrunk development", false),
        ] {
            let program = fixture.path().join(name);
            fs::write(&program, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
            let mut permissions = fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&program, permissions).unwrap();
            let result = prove_integrations_with_program(
                fixture.path(),
                &[candidate()],
                Duration::from_secs(1),
                &program,
            );
            assert_eq!(
                matches!(result, Err(AdapterError::IncompatibleVersion { .. })),
                expected_old
            );
            assert_eq!(
                matches!(result, Err(AdapterError::MalformedVersion(_))),
                !expected_old
            );
        }

        let malformed_json = fixture.path().join("malformed-json");
        fs::write(
            &malformed_json,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'wt 0.74.0'; else echo '{'; fi\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&malformed_json).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&malformed_json, permissions).unwrap();
        assert!(matches!(
            prove_integrations_with_program(
                fixture.path(),
                &[candidate()],
                Duration::from_secs(1),
                &malformed_json
            ),
            Err(AdapterError::MalformedJson(_))
        ));
    }
}
