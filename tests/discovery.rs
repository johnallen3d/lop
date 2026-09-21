use std::{fs, path::Path, process::Command};

use lop::discovery::{DiscoveryIssueKind, discover};
use tempfile::tempdir;

fn init_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn finds_repositories_at_depth_boundaries_and_skips_ignored_locations() {
    let fixture = tempdir().unwrap();
    let root = fixture.path();
    init_repository(&root.join("direct"));
    init_repository(&root.join("group/nested"));
    init_repository(&root.join("one/two/too-deep"));
    init_repository(&root.join("node_modules/dependency"));
    init_repository(&root.join("target/generated"));
    fs::create_dir_all(root.join("linked-worktree")).unwrap();
    fs::write(root.join("linked-worktree/.git"), "gitdir: /elsewhere").unwrap();

    let result = discover(&[root.to_path_buf()], 2);
    let canonical_root = fs::canonicalize(root).unwrap();
    let paths: Vec<_> = result
        .repositories
        .iter()
        .map(|repository| {
            repository
                .path
                .strip_prefix(&canonical_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();

    assert_eq!(paths, [Path::new("direct"), Path::new("group/nested")]);
    assert!(result.issues.is_empty(), "{:?}", result.issues);
}

#[test]
fn deduplicates_duplicate_and_overlapping_roots_after_canonicalization() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("root");
    init_repository(&root.join("project"));
    init_repository(&root.join("group/nested"));

    let result = discover(
        &[
            root.clone(),
            root.clone(),
            root.join("group"),
            root.join("group/.."),
        ],
        3,
    );

    assert_eq!(result.repositories.len(), 2);
    assert!(result.issues.is_empty(), "{:?}", result.issues);
}

#[cfg(unix)]
#[test]
fn canonicalizes_symlinked_roots() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().unwrap();
    let root = fixture.path().join("actual");
    let alias = fixture.path().join("alias");
    init_repository(&root.join("project"));
    symlink(&root, &alias).unwrap();

    let result = discover(&[alias], 1);

    assert_eq!(result.repositories.len(), 1);
    assert_eq!(
        result.repositories[0].path,
        fs::canonicalize(root.join("project")).unwrap()
    );
    assert!(result.issues.is_empty(), "{:?}", result.issues);
}

#[cfg(unix)]
#[test]
fn deduplicates_distinct_working_paths_with_the_same_common_directory() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().unwrap();
    let root = fixture.path();
    let first = root.join("first");
    let second = root.join("second");
    init_repository(&first);
    fs::create_dir(&second).unwrap();
    symlink(first.join(".git"), second.join(".git")).unwrap();

    let result = discover(&[root.to_path_buf()], 1);

    assert_eq!(result.repositories.len(), 1);
    assert_eq!(
        result.repositories[0].path,
        fs::canonicalize(first).unwrap()
    );
    assert!(result.issues.is_empty(), "{:?}", result.issues);
}

#[test]
fn reports_nonexistent_and_nondirectory_roots() {
    let fixture = tempdir().unwrap();
    let missing = fixture.path().join("missing");
    let file = fixture.path().join("file");
    fs::write(&file, "not a directory").unwrap();

    let result = discover(&[missing, file], 2);

    assert!(result.repositories.is_empty());
    assert_eq!(result.issues.len(), 2);
    assert!(
        result
            .issues
            .iter()
            .all(|issue| issue.kind == DiscoveryIssueKind::InaccessibleRoot)
    );
}

#[cfg(unix)]
#[test]
fn reports_unsafe_root_canonicalization() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().unwrap();
    let left = fixture.path().join("left");
    let right = fixture.path().join("right");
    symlink(&right, &left).unwrap();
    symlink(&left, &right).unwrap();

    let result = discover(&[left], 1);

    assert!(result.repositories.is_empty());
    assert_eq!(result.issues.len(), 1);
    assert_eq!(
        result.issues[0].kind,
        DiscoveryIssueKind::UnsafeCanonicalization
    );
}
