use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub exclude_paths: Vec<PathBuf>,
    pub scan_depth: u32,
    pub fetch_timeout_seconds: u64,
    pub check_processes: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    roots: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default = "default_scan_depth")]
    scan_depth: u32,
    #[serde(default = "default_fetch_timeout_seconds")]
    fetch_timeout_seconds: u64,
    #[serde(default = "default_check_processes")]
    check_processes: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalExclusions {
    exclude_paths: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(
        "configuration not found at {0}; copy config.example.toml there and set at least one root"
    )]
    Missing(PathBuf),
    #[error("failed to read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error(
        "invalid configuration: roots must contain at least one explicitly configured directory"
    )]
    NoRoots,
    #[error("invalid exclusion {path:?}: {message}")]
    InvalidExclusion { path: String, message: String },
    #[error("invalid configuration: root {root:?} is empty")]
    EmptyRoot { root: String },
    #[error("invalid configuration: root {root:?} must be absolute or start with '~/'")]
    RelativeRoot { root: String },
    #[error("invalid configuration: cannot expand {root:?} because HOME is not set")]
    MissingHome { root: String },
    #[error(
        "invalid configuration: '~user' expansion is unsupported in root {root:?}; use an absolute path"
    )]
    UnsupportedTilde { root: String },
    #[error("invalid configuration: scan_depth must be greater than zero")]
    InvalidScanDepth,
    #[error("invalid configuration: fetch_timeout_seconds must be greater than zero")]
    InvalidFetchTimeout,
}

impl Config {
    /// Loads configuration and optional sibling `exclusions.toml`.
    ///
    /// Local exclusions are added to those in the main configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or any setting is invalid.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::Missing(path.to_path_buf()));
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let mut raw: RawConfig =
            toml::from_str(&contents).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let exclusions_path = path.with_file_name("exclusions.toml");
        match fs::read_to_string(&exclusions_path) {
            Ok(contents) => {
                let local: LocalExclusions =
                    toml::from_str(&contents).map_err(|source| ConfigError::Parse {
                        path: exclusions_path.clone(),
                        source,
                    })?;
                raw.exclude_paths.extend(local.exclude_paths);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ConfigError::Read {
                    path: exclusions_path,
                    source,
                });
            }
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::validate(raw, home.as_deref())
    }

    fn validate(raw: RawConfig, home: Option<&Path>) -> Result<Self, ConfigError> {
        if raw.roots.is_empty() {
            return Err(ConfigError::NoRoots);
        }
        if raw.scan_depth == 0 {
            return Err(ConfigError::InvalidScanDepth);
        }
        if raw.fetch_timeout_seconds == 0 {
            return Err(ConfigError::InvalidFetchTimeout);
        }

        let mut roots = Vec::with_capacity(raw.roots.len());
        for root in raw.roots {
            roots.push(expand_root(&root, home)?);
        }

        let exclude_paths = raw
            .exclude_paths
            .iter()
            .map(|path| {
                expand_root(path, home).map_err(|error| ConfigError::InvalidExclusion {
                    path: path.clone(),
                    message: error.to_string(),
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            roots,
            exclude_paths,
            scan_depth: raw.scan_depth,
            fetch_timeout_seconds: raw.fetch_timeout_seconds,
            check_processes: raw.check_processes,
        })
    }
}

fn expand_root(root: &str, home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if root.is_empty() {
        return Err(ConfigError::EmptyRoot {
            root: root.to_owned(),
        });
    }
    if root == "~" || root.starts_with("~/") {
        let home = home.ok_or_else(|| ConfigError::MissingHome {
            root: root.to_owned(),
        })?;
        let suffix = root.strip_prefix("~/").unwrap_or("");
        return Ok(home.join(suffix));
    }
    if root.starts_with('~') {
        return Err(ConfigError::UnsupportedTilde {
            root: root.to_owned(),
        });
    }
    let path = PathBuf::from(root);
    if !path.is_absolute() {
        return Err(ConfigError::RelativeRoot {
            root: root.to_owned(),
        });
    }
    Ok(path)
}

const fn default_scan_depth() -> u32 {
    3
}

const fn default_fetch_timeout_seconds() -> u64 {
    60
}

const fn default_check_processes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::tempdir;

    use super::{Config, ConfigError, RawConfig};

    fn raw(roots: &[&str]) -> RawConfig {
        RawConfig {
            roots: roots.iter().map(ToString::to_string).collect(),
            exclude_paths: Vec::new(),
            scan_depth: 3,
            fetch_timeout_seconds: 60,
            check_processes: true,
        }
    }

    #[test]
    fn expands_home_and_accepts_absolute_roots() {
        let home = PathBuf::from("/home/me");
        let config = Config::validate(raw(&["~/dev", "/opt/src"]), Some(&home)).unwrap();
        assert_eq!(
            config.roots,
            [Path::new("/home/me/dev"), Path::new("/opt/src")]
        );
    }

    #[test]
    fn expands_exclusions_and_rejects_invalid_paths() {
        let mut config = raw(&["/src"]);
        config.exclude_paths = vec!["~/dev/private".to_owned(), "/src/ignored".to_owned()];
        let validated = Config::validate(config, Some(Path::new("/home/me"))).unwrap();
        assert_eq!(
            validated.exclude_paths,
            [Path::new("/home/me/dev/private"), Path::new("/src/ignored")]
        );

        for path in ["", "relative", "~other/src", "~/missing-home"] {
            let mut config = raw(&["/src"]);
            config.exclude_paths = vec![path.to_owned()];
            assert!(matches!(
                Config::validate(config, None),
                Err(ConfigError::InvalidExclusion { .. })
            ));
        }
    }

    #[test]
    fn requires_a_root() {
        assert!(matches!(
            Config::validate(raw(&[]), None),
            Err(ConfigError::NoRoots)
        ));
    }

    #[test]
    fn rejects_relative_and_user_tilde_roots() {
        assert!(matches!(
            Config::validate(raw(&["src"]), None),
            Err(ConfigError::RelativeRoot { .. })
        ));
        assert!(matches!(
            Config::validate(raw(&["~other/src"]), None),
            Err(ConfigError::UnsupportedTilde { .. })
        ));
    }

    #[test]
    fn preserves_duplicate_roots_for_discovery_to_deduplicate() {
        let config = Config::validate(raw(&["/src", "/src"]), None).unwrap();
        assert_eq!(config.roots, [Path::new("/src"), Path::new("/src")]);
    }

    #[test]
    fn validates_numeric_ranges() {
        let mut config = raw(&["/src"]);
        config.scan_depth = 0;
        assert!(matches!(
            Config::validate(config, None),
            Err(ConfigError::InvalidScanDepth)
        ));

        let mut config = raw(&["/src"]);
        config.fetch_timeout_seconds = 0;
        assert!(matches!(
            Config::validate(config, None),
            Err(ConfigError::InvalidFetchTimeout)
        ));
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<RawConfig>("roots = ['/src']\nsurprise = true").is_err());
    }

    #[test]
    fn local_exclusions_extend_main_configuration_without_replacing_it() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let contents = "roots = ['/src']\nexclude_paths = ['/src/main-exclusion']\n";
        fs::write(&path, contents).unwrap();
        assert_eq!(Config::load(&path).unwrap().exclude_paths.len(), 1);
        fs::write(
            directory.path().join("exclusions.toml"),
            "exclude_paths = ['/src/local-exclusion']\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().exclude_paths,
            [
                Path::new("/src/main-exclusion"),
                Path::new("/src/local-exclusion")
            ]
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn invalid_local_exclusions_fail_closed() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "roots = ['/src']\n").unwrap();
        let exclusions = directory.path().join("exclusions.toml");
        for contents in [
            "exclude_paths = ['/src']\nsurprise = true\n",
            "exclude_paths = ['relative']\n",
            "not TOML",
        ] {
            fs::write(&exclusions, contents).unwrap();
            assert!(Config::load(&path).is_err());
        }
        fs::remove_file(&exclusions).unwrap();
        fs::create_dir(&exclusions).unwrap();
        assert!(matches!(Config::load(&path), Err(ConfigError::Read { .. })));
    }

    #[test]
    fn defaults_optional_fields() {
        let raw: RawConfig = toml::from_str("roots = ['/src']").unwrap();
        assert!(raw.exclude_paths.is_empty());
        assert_eq!(raw.scan_depth, 3);
        assert_eq!(raw.fetch_timeout_seconds, 60);
        assert!(raw.check_processes);
    }
}
