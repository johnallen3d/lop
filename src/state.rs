use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema_version: u32,
    pub tool_version: String,
    /// State keyed by Git common directory. This is notification/fetch memory,
    /// never an inventory used to discover repositories.
    #[serde(default)]
    pub repositories: BTreeMap<String, RepositoryState>,
}

#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_fetch_at: Option<String>,
    /// Last emitted classification keyed by worktree path, for deduplication only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_classifications: BTreeMap<String, ClassificationState>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassificationState {
    pub classification: String,
    pub reason_code: String,
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("failed to read state at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("state at {path} is malformed: {source}; move it aside and rerun Lop")]
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "state at {path} uses schema version {found}, but this Lop supports version {supported}; upgrade Lop or move the state file aside"
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u64,
        supported: u32,
    },
    #[error("failed to create state directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write state at {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_owned(),
            repositories: BTreeMap::new(),
        }
    }
}

impl State {
    /// Loads state and migrates supported older schemas.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable, malformed, or newer state schemas.
    pub fn load(path: &Path) -> Result<Self, StateError> {
        let contents = match fs::read(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(StateError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let value: Value =
            serde_json::from_slice(&contents).map_err(|source| StateError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        let version = match value.get("schema_version") {
            None => 0,
            Some(version) => serde_json::from_value::<u64>(version.clone()).map_err(|source| {
                StateError::Malformed {
                    path: path.to_path_buf(),
                    source,
                }
            })?,
        };

        let mut state = match version {
            0 => migrate_v0(value, path)?,
            1 => serde_json::from_value(value).map_err(|source| StateError::Malformed {
                path: path.to_path_buf(),
                source,
            })?,
            found => {
                return Err(StateError::UnsupportedVersion {
                    path: path.to_path_buf(),
                    found,
                    supported: STATE_SCHEMA_VERSION,
                });
            }
        };
        state.schema_version = STATE_SCHEMA_VERSION;
        env!("CARGO_PKG_VERSION").clone_into(&mut state.tool_version);
        Ok(state)
    }

    /// Atomically writes state to `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| StateError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let temporary = path.with_extension("json.tmp");
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            serde_json::to_writer_pretty(&mut file, self).map_err(std::io::Error::other)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&temporary);
            return Err(StateError::Write {
                path: path.to_path_buf(),
                source,
            });
        }
        Ok(())
    }
}

fn migrate_v0(value: Value, path: &Path) -> Result<State, StateError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StateV0 {
        #[serde(default)]
        schema_version: Option<u32>,
        #[serde(default)]
        tool_version: Option<String>,
        #[serde(default)]
        repositories: BTreeMap<String, RepositoryState>,
    }

    let old: StateV0 = serde_json::from_value(value).map_err(|source| StateError::Malformed {
        path: path.to_path_buf(),
        source,
    })?;
    let _ = (old.schema_version, old.tool_version);
    Ok(State {
        repositories: old.repositories,
        ..State::default()
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{STATE_SCHEMA_VERSION, State, StateError};

    #[test]
    fn missing_state_is_empty() {
        let directory = tempdir().unwrap();
        let state = State::load(&directory.path().join("missing.json")).unwrap();
        assert_eq!(state, State::default());
    }

    #[test]
    fn saves_and_loads_current_state() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/state.json");
        State::default().save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), State::default());
    }

    #[test]
    fn migrates_unversioned_state() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, r#"{"repositories":{}}"#).unwrap();
        let state = State::load(&path).unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn rejects_future_schema_versions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(
            &path,
            r#"{"schema_version":999,"tool_version":"next","repositories":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            State::load(&path),
            Err(StateError::UnsupportedVersion { found: 999, .. })
        ));
    }

    #[test]
    fn reports_malformed_state() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.json");
        for contents in [
            "not json",
            r#"{"schema_version":"one"}"#,
            r#"{"unknown":true}"#,
        ] {
            fs::write(&path, contents).unwrap();
            assert!(matches!(
                State::load(&path),
                Err(StateError::Malformed { .. })
            ));
        }
    }
}
