use std::{
    env,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub lock_file: PathBuf,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PathError {
    #[error("HOME is not set; cannot resolve Lop's configuration and state paths")]
    MissingHome,
    #[error("{variable} must be an absolute path, but was {value:?}")]
    RelativeXdgPath {
        variable: &'static str,
        value: PathBuf,
    },
}

impl AppPaths {
    /// Resolves configuration, state, and lock paths from XDG environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable home exists or an XDG path is relative.
    pub fn from_env() -> Result<Self, PathError> {
        Self::resolve(|name| env::var_os(name).map(PathBuf::from))
    }

    fn resolve(mut get: impl FnMut(&str) -> Option<PathBuf>) -> Result<Self, PathError> {
        let home = get("HOME");
        let config_fallback = home.as_ref().map(|path| path.join(".config"));
        let state_fallback = home.as_ref().map(|path| path.join(".local/state"));
        let config_home = xdg_home(&mut get, "XDG_CONFIG_HOME", config_fallback.as_deref())?;
        let state_home = xdg_home(&mut get, "XDG_STATE_HOME", state_fallback.as_deref())?;
        let state_dir = state_home.join("lop");

        Ok(Self {
            config_file: config_home.join("lop/config.toml"),
            state_file: state_dir.join("state.json"),
            lock_file: state_dir.join("run.lock"),
        })
    }
}

fn xdg_home(
    get: &mut impl FnMut(&str) -> Option<PathBuf>,
    variable: &'static str,
    fallback: Option<&Path>,
) -> Result<PathBuf, PathError> {
    let Some(value) = get(variable) else {
        return fallback
            .map(Path::to_path_buf)
            .ok_or(PathError::MissingHome);
    };
    if value.as_os_str().is_empty() {
        return fallback
            .map(Path::to_path_buf)
            .ok_or(PathError::MissingHome);
    }
    if !value.is_absolute() {
        return Err(PathError::RelativeXdgPath { variable, value });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use super::{AppPaths, PathError};

    fn resolve(values: &[(&str, &str)]) -> Result<AppPaths, PathError> {
        let values: HashMap<_, _> = values.iter().copied().collect();
        AppPaths::resolve(|key| values.get(key).map(PathBuf::from))
    }

    #[test]
    fn uses_home_fallbacks() {
        let paths = resolve(&[("HOME", "/Users/test")]).unwrap();
        assert_eq!(
            paths.config_file,
            PathBuf::from("/Users/test/.config/lop/config.toml")
        );
        assert_eq!(
            paths.state_file,
            PathBuf::from("/Users/test/.local/state/lop/state.json")
        );
        assert_eq!(
            paths.lock_file,
            PathBuf::from("/Users/test/.local/state/lop/run.lock")
        );
    }

    #[test]
    fn honors_xdg_overrides_without_requiring_home() {
        let paths = resolve(&[
            ("XDG_CONFIG_HOME", "/tmp/config"),
            ("XDG_STATE_HOME", "/tmp/state"),
        ])
        .unwrap();
        assert_eq!(
            paths.config_file,
            PathBuf::from("/tmp/config/lop/config.toml")
        );
        assert_eq!(paths.state_file, PathBuf::from("/tmp/state/lop/state.json"));
    }

    #[test]
    fn rejects_relative_xdg_paths() {
        let error =
            resolve(&[("HOME", "/Users/test"), ("XDG_CONFIG_HOME", "relative")]).unwrap_err();
        assert!(matches!(error, PathError::RelativeXdgPath { .. }));
    }
}
