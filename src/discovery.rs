use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Repository {
    pub path: PathBuf,
    pub git_common_directory: PathBuf,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DiscoveryIssueKind {
    InaccessibleRoot,
    InaccessibleDirectory,
    UnsafeCanonicalization,
    InvalidGitMetadata,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DiscoveryIssue {
    pub path: PathBuf,
    pub kind: DiscoveryIssueKind,
    pub message: String,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Discovery {
    pub repositories: Vec<Repository>,
    pub issues: Vec<DiscoveryIssue>,
}

/// Finds main Git repositories beneath configured roots.
///
/// Repository paths and Git common directories are canonicalized. Repositories
/// are deduplicated by common directory, while failures are retained so callers
/// can report every operational problem.
#[must_use]
pub fn discover(roots: &[PathBuf], scan_depth: u32) -> Discovery {
    let mut repositories = BTreeMap::<PathBuf, PathBuf>::new();
    let mut issues = Vec::new();
    let mut canonical_roots = HashSet::new();

    for configured_root in roots {
        let root = match fs::canonicalize(configured_root) {
            Ok(root) => root,
            Err(error) => {
                issues.push(canonicalization_issue(configured_root, &error, true));
                continue;
            }
        };

        if !canonical_roots.insert(root.clone()) {
            continue;
        }

        walk_root(&root, scan_depth, &mut repositories, &mut issues);
    }

    let mut repositories: Vec<_> = repositories
        .into_iter()
        .map(|(git_common_directory, path)| Repository {
            path,
            git_common_directory,
        })
        .collect();
    repositories.sort_by(|left, right| left.path.cmp(&right.path));

    Discovery {
        repositories,
        issues,
    }
}

fn walk_root(
    root: &Path,
    scan_depth: u32,
    repositories: &mut BTreeMap<PathBuf, PathBuf>,
    issues: &mut Vec<DiscoveryIssue>,
) {
    let mut pending = VecDeque::from([(root.to_path_buf(), 0)]);
    let mut visited = HashSet::new();

    while let Some((path, depth)) = pending.pop_front() {
        let canonical_path = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) => {
                issues.push(canonicalization_issue(&path, &error, depth == 0));
                continue;
            }
        };

        if !canonical_path.starts_with(root) {
            issues.push(DiscoveryIssue {
                path: canonical_path,
                kind: DiscoveryIssueKind::UnsafeCanonicalization,
                message: format!(
                    "directory reached from {} resolves outside configured root {}",
                    path.display(),
                    root.display()
                ),
            });
            continue;
        }
        if !visited.insert(canonical_path.clone()) {
            continue;
        }

        let entries = match fs::read_dir(&canonical_path) {
            Ok(entries) => entries,
            Err(error) => {
                issues.push(DiscoveryIssue {
                    path: canonical_path,
                    kind: if depth == 0 {
                        DiscoveryIssueKind::InaccessibleRoot
                    } else {
                        DiscoveryIssueKind::InaccessibleDirectory
                    },
                    message: error.to_string(),
                });
                continue;
            }
        };

        let mut entries = match entries.collect::<Result<Vec<_>, _>>() {
            Ok(entries) => entries,
            Err(error) => {
                issues.push(DiscoveryIssue {
                    path: canonical_path,
                    kind: if depth == 0 {
                        DiscoveryIssueKind::InaccessibleRoot
                    } else {
                        DiscoveryIssueKind::InaccessibleDirectory
                    },
                    message: error.to_string(),
                });
                continue;
            }
        };
        entries.sort_by_key(fs::DirEntry::file_name);

        if let Some(git_entry) = entries
            .iter()
            .find(|entry| entry.file_name() == OsStr::new(".git"))
        {
            inspect_git_directory(&canonical_path, &git_entry.path(), repositories, issues);
        }

        if depth >= scan_depth {
            continue;
        }

        for entry in entries {
            let name = entry.file_name();
            if ignored_directory(&name) {
                continue;
            }

            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => {
                    pending.push_back((entry.path(), depth + 1));
                }
                Ok(_) => {}
                Err(error) => issues.push(DiscoveryIssue {
                    path: entry.path(),
                    kind: DiscoveryIssueKind::InaccessibleDirectory,
                    message: error.to_string(),
                }),
            }
        }
    }
}

fn inspect_git_directory(
    repository_path: &Path,
    git_path: &Path,
    repositories: &mut BTreeMap<PathBuf, PathBuf>,
    issues: &mut Vec<DiscoveryIssue>,
) {
    match fs::metadata(git_path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return,
        Err(error) => {
            issues.push(DiscoveryIssue {
                path: repository_path.to_path_buf(),
                kind: DiscoveryIssueKind::InvalidGitMetadata,
                message: format!("cannot inspect {}: {error}", git_path.display()),
            });
            return;
        }
    }

    match git_common_directory(repository_path) {
        Ok(common_directory) => {
            repositories
                .entry(common_directory)
                .and_modify(|existing| {
                    if repository_path < existing.as_path() {
                        repository_path.clone_into(existing);
                    }
                })
                .or_insert_with(|| repository_path.to_path_buf());
        }
        Err(message) => issues.push(DiscoveryIssue {
            path: repository_path.to_path_buf(),
            kind: DiscoveryIssueKind::InvalidGitMetadata,
            message,
        }),
    }
}

fn git_common_directory(repository_path: &Path) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository_path)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git could not resolve the common directory: {}",
            stderr.trim()
        ));
    }

    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("git returned a non-UTF-8 common directory: {error}"))?;
    let common_directory = PathBuf::from(value.trim());
    if !common_directory.is_absolute() {
        return Err(format!(
            "git returned a non-absolute common directory: {}",
            common_directory.display()
        ));
    }

    fs::canonicalize(&common_directory).map_err(|error| {
        format!(
            "cannot canonicalize Git common directory {}: {error}",
            common_directory.display()
        )
    })
}

fn canonicalization_issue(path: &Path, error: &io::Error, is_root: bool) -> DiscoveryIssue {
    let kind = if is_root
        && matches!(
            error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::PermissionDenied
                | io::ErrorKind::NotADirectory
        ) {
        DiscoveryIssueKind::InaccessibleRoot
    } else {
        DiscoveryIssueKind::UnsafeCanonicalization
    };

    DiscoveryIssue {
        path: path.to_path_buf(),
        kind,
        message: error.to_string(),
    }
}

fn ignored_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".worktrees"
                | ".venv"
                | "node_modules"
                | "target"
                | "build"
                | "dist"
                | "out"
                | "coverage"
        )
    )
}
