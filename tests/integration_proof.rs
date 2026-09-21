use std::{fs, path::Path, process::Command, time::Duration};

use lop::integration::{
    IntegrationCandidate, IntegrationProof, IntegrationReason, prove_integrations,
};
use tempfile::TempDir;

struct RepositoryFixture {
    directory: TempDir,
    repository: std::path::PathBuf,
    feature_worktree: std::path::PathBuf,
}

impl RepositoryFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        let feature_worktree = directory.path().join("feature-worktree");
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
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                path(&feature_worktree),
            ],
        );
        Self {
            directory,
            repository,
            feature_worktree,
        }
    }

    fn write_feature(&self, file: &str, contents: &str, message: &str) {
        fs::write(self.feature_worktree.join(file), contents).unwrap();
        git(&self.feature_worktree, &["add", file]);
        git(
            &self.feature_worktree,
            &["commit", "--quiet", "-m", message],
        );
    }

    fn write_main(&self, file: &str, contents: &str, message: &str) {
        fs::write(self.repository.join(file), contents).unwrap();
        git(&self.repository, &["add", file]);
        git(&self.repository, &["commit", "--quiet", "-m", message]);
    }

    fn proof(&self) -> IntegrationProof {
        let head = git_stdout(&self.repository, &["rev-parse", "feature"]);
        prove_integrations(
            &self.repository,
            &[IntegrationCandidate {
                path: fs::canonicalize(&self.feature_worktree).unwrap(),
                branch: "refs/heads/feature".to_owned(),
                head,
            }],
            Duration::from_secs(10),
        )
        .unwrap()[0]
    }
}

#[test]
fn proves_ordinary_merge_and_fast_forward_integration() {
    let ordinary = RepositoryFixture::new();
    ordinary.write_feature("feature.txt", "ordinary\n", "feature");
    git(
        &ordinary.repository,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge feature",
            "feature",
        ],
    );
    assert_eq!(
        ordinary.proof(),
        IntegrationProof::Integrated(IntegrationReason::Ancestor)
    );

    let fast_forward = RepositoryFixture::new();
    fast_forward.write_feature("feature.txt", "fast forward\n", "feature");
    git(
        &fast_forward.repository,
        &["merge", "--quiet", "--ff-only", "feature"],
    );
    assert_eq!(
        fast_forward.proof(),
        IntegrationProof::Integrated(IntegrationReason::SameCommit)
    );
}

#[test]
fn proves_rebased_and_squash_integrated_changes() {
    let rebased = RepositoryFixture::new();
    rebased.write_feature("feature.txt", "rebased change\n", "feature");
    rebased.write_main("unrelated.txt", "main moved\n", "move main");
    let feature_commit = git_stdout(&rebased.repository, &["rev-parse", "feature"]);
    git(
        &rebased.repository,
        &["cherry-pick", "--quiet", &feature_commit],
    );
    assert!(matches!(rebased.proof(), IntegrationProof::Integrated(_)));

    let squash = RepositoryFixture::new();
    squash.write_feature("feature.txt", "first\n", "first feature commit");
    squash.write_feature("feature.txt", "first\nsecond\n", "second feature commit");
    git(
        &squash.repository,
        &["merge", "--quiet", "--squash", "feature"],
    );
    git(
        &squash.repository,
        &["commit", "--quiet", "-m", "squash feature"],
    );
    assert!(matches!(squash.proof(), IntegrationProof::Integrated(_)));
}

#[test]
fn proves_squash_after_unrelated_and_same_file_changes() {
    let unrelated = RepositoryFixture::new();
    unrelated.write_feature("feature.txt", "squashed\n", "feature");
    git(
        &unrelated.repository,
        &["merge", "--quiet", "--squash", "feature"],
    );
    git(
        &unrelated.repository,
        &["commit", "--quiet", "-m", "squash feature"],
    );
    unrelated.write_main("later.txt", "later\n", "unrelated later work");
    assert!(matches!(unrelated.proof(), IntegrationProof::Integrated(_)));

    let edited = RepositoryFixture::new();
    edited.write_feature("feature.txt", "squashed\n", "feature");
    git(
        &edited.repository,
        &["merge", "--quiet", "--squash", "feature"],
    );
    git(
        &edited.repository,
        &["commit", "--quiet", "-m", "squash feature"],
    );
    edited.write_main(
        "feature.txt",
        "squashed\nlater edit\n",
        "edit squashed file later",
    );
    assert_eq!(
        edited.proof(),
        IntegrationProof::Integrated(IntegrationReason::PatchIdMatch)
    );
}

#[test]
fn uses_a_strictly_ahead_default_branch_upstream_tip() {
    let fixture = RepositoryFixture::new();
    fixture.write_feature("feature.txt", "remote integration\n", "feature");
    let local_main = git_stdout(&fixture.repository, &["rev-parse", "main"]);
    git(
        &fixture.repository,
        &["merge", "--quiet", "--squash", "feature"],
    );
    git(
        &fixture.repository,
        &["commit", "--quiet", "-m", "squash feature"],
    );

    let remote = fixture.directory.path().join("origin.git");
    git(
        fixture.directory.path(),
        &["init", "--quiet", "--bare", path(&remote)],
    );
    git(
        &fixture.repository,
        &["remote", "add", "origin", path(&remote)],
    );
    git(
        &fixture.repository,
        &["push", "--quiet", "-u", "origin", "main"],
    );
    git(
        &fixture.repository,
        &["reset", "--quiet", "--hard", &local_main],
    );

    assert!(matches!(fixture.proof(), IntegrationProof::Integrated(_)));
}

#[test]
fn retains_patch_mismatches_and_unique_commits() {
    let mismatch = RepositoryFixture::new();
    mismatch.write_feature("base.txt", "feature version\n", "feature edit");
    mismatch.write_main("base.txt", "different main version\n", "different edit");
    assert_eq!(mismatch.proof(), IntegrationProof::NotIntegrated);

    let unique = RepositoryFixture::new();
    unique.write_feature("unique.txt", "not integrated\n", "unique work");
    assert_eq!(unique.proof(), IntegrationProof::NotIntegrated);
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

fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}
