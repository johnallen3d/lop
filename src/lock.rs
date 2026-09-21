use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use thiserror::Error;

#[derive(Debug)]
pub struct RunLock {
    file: File,
}

#[derive(Debug, Error)]
pub enum LockError {
    #[error("another Lop run is already active (lock: {0})")]
    Contended(PathBuf),
    #[error("failed to create lock directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open process lock {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to acquire process lock {path}: {source}")]
    Acquire {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl RunLock {
    /// Acquires the process-wide Lop lock at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be opened or another run holds it.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| LockError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| LockError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(LockError::Contended(path.to_path_buf()))
            }
            Err(source) => Err(LockError::Acquire {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{LockError, RunLock};

    #[test]
    fn contention_is_reported_and_release_allows_next_run() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("run.lock");
        let first = RunLock::acquire(&path).unwrap();
        assert!(matches!(
            RunLock::acquire(&path),
            Err(LockError::Contended(_))
        ));
        drop(first);
        RunLock::acquire(&path).unwrap();
    }
}
