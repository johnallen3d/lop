use std::{
    collections::BTreeSet,
    env, fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{cli::ScheduleCommand, config::Config, discovery::discover};

const LABEL: &str = "org.nixos.lop";
const DEFAULT_INTERVAL_SECONDS: u64 = 1_800;
const MINIMUM_INTERVAL_SECONDS: u64 = 300;
const MAXIMUM_INTERVAL_SECONDS: u64 = 604_800;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error("scheduling is unavailable on {0}; scan and prune remain available")]
    UnsupportedPlatform(&'static str),
    #[error("HOME is not set; cannot resolve scheduling paths")]
    MissingHome,
    #[error("{variable} must be an absolute path, but was {value:?}")]
    RelativeXdgPath {
        variable: &'static str,
        value: PathBuf,
    },
    #[error("failed to read schedule settings at {path}: {source}")]
    ReadSettings {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid schedule settings at {path}: {message}")]
    InvalidSettings { path: PathBuf, message: String },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("required executable {name:?} was not found in PATH; install it before scheduling Lop")]
    MissingExecutable { name: &'static str },
    #[error("cannot schedule Lop because repository discovery failed: {0}")]
    Discovery(String),
    #[error("noninteractive credential preflight failed for {repository}: {message}")]
    CredentialPreflight {
        repository: PathBuf,
        message: String,
    },
    #[error("SSH_AUTH_SOCK is required for SSH remotes but is not set")]
    MissingSshAgent,
    #[error("SSH_AUTH_SOCK {0:?} is not a usable Unix socket")]
    InvalidSshAgent(PathBuf),
    #[error("the SSH agent has no usable identities; run ssh-add before installing the schedule")]
    EmptySshAgent,
    #[error("failed to inspect the SSH agent: {0}")]
    SshAgent(String),
    #[error("scheduled command timed out: {0}")]
    TimedOut(String),
    #[error("failed to run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("launchctl {action} failed: {message}")]
    Launchctl {
        action: &'static str,
        message: String,
    },
    #[error("failed to determine the current user id: {0}")]
    UserId(String),
    #[error("schedule is not installed; run `lop schedule install` first")]
    NotInstalled,
    #[error("neither VISUAL nor EDITOR names an editor; set one before running schedule edit")]
    MissingEditor,
    #[error("editor {editor:?} failed with status {status}")]
    EditorFailed { editor: String, status: ExitStatus },
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScheduleMode {
    Scan,
    Prune,
}

impl ScheduleMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Prune => "prune",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScheduleSettings {
    mode: ScheduleMode,
    interval_seconds: u64,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            mode: ScheduleMode::Scan,
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
        }
    }
}

impl ScheduleSettings {
    fn validate(&self) -> Result<(), String> {
        if !(MINIMUM_INTERVAL_SECONDS..=MAXIMUM_INTERVAL_SECONDS).contains(&self.interval_seconds) {
            return Err(format!(
                "interval_seconds must be between {MINIMUM_INTERVAL_SECONDS} and {MAXIMUM_INTERVAL_SECONDS}"
            ));
        }
        Ok(())
    }

    fn encode(&self) -> String {
        format!(
            "# Managed by `lop schedule`; edit with `lop schedule edit`.\nmode = \"{}\"\ninterval_seconds = {}\n",
            self.mode.label(),
            self.interval_seconds
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct SchedulePaths {
    settings: PathBuf,
    launch_agent: PathBuf,
    log_directory: PathBuf,
}

impl SchedulePaths {
    fn from_env() -> Result<Self, ScheduleError> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(ScheduleError::MissingHome)?;
        let config_home = match env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            Some(value) => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(ScheduleError::RelativeXdgPath {
                        variable: "XDG_CONFIG_HOME",
                        value: path,
                    });
                }
                path
            }
            None => home.join(".config"),
        };

        Ok(Self {
            settings: config_home.join("lop/schedule.toml"),
            launch_agent: home.join("Library/LaunchAgents/org.nixos.lop.plist"),
            log_directory: home.join("Library/Logs/org.nixos"),
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct Programs {
    lop: PathBuf,
    git: PathBuf,
    worktrunk: PathBuf,
    path: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ScheduleEnvironment {
    programs: Programs,
    ssh_auth_sock: Option<PathBuf>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct LaunchAgent {
    contents: String,
}

/// Runs a scheduling command independently of the cleanup pipeline.
///
/// # Errors
///
/// Returns an actionable error for unsupported platforms, invalid settings,
/// missing prerequisites, credential failures, or platform service failures.
pub fn run(command: ScheduleCommand, output: &mut impl Write) -> Result<(), ScheduleError> {
    ensure_supported_platform(env::consts::OS)?;

    let paths = SchedulePaths::from_env()?;
    let backend = MacOsBackend::system(&paths)?;
    match command {
        ScheduleCommand::Install => install(&paths, &backend, output),
        ScheduleCommand::Status => status(&paths, &backend, output),
        ScheduleCommand::Edit => edit(&paths, &backend, output),
        ScheduleCommand::Uninstall => uninstall(&paths, &backend, output),
    }
}

fn ensure_supported_platform(platform: &'static str) -> Result<(), ScheduleError> {
    if platform == "macos" {
        Ok(())
    } else {
        Err(ScheduleError::UnsupportedPlatform(platform))
    }
}

fn install(
    paths: &SchedulePaths,
    backend: &MacOsBackend,
    output: &mut impl Write,
) -> Result<(), ScheduleError> {
    let settings = if paths.settings.exists() {
        load_settings(&paths.settings)?
    } else {
        ScheduleSettings::default()
    };
    let environment = prepare_environment()?;
    preflight_credentials(&environment)?;
    let agent = generate_launch_agent(&settings, &environment, paths);

    let previous_settings = fs::read(&paths.settings).ok();
    write_atomic(&paths.settings, settings.encode().as_bytes(), 0o600)?;
    if let Err(error) = backend.install(&agent.contents) {
        match previous_settings {
            Some(contents) => {
                let _ = write_atomic(&paths.settings, &contents, 0o600);
            }
            None => {
                let _ = fs::remove_file(&paths.settings);
            }
        }
        return Err(error);
    }
    writeln!(
        output,
        "installed {LABEL} in {} mode every {} seconds",
        settings.mode.label(),
        settings.interval_seconds
    )
    .map_err(|source| ScheduleError::Write {
        path: PathBuf::from("stdout"),
        source,
    })?;
    Ok(())
}

fn status(
    paths: &SchedulePaths,
    backend: &MacOsBackend,
    output: &mut impl Write,
) -> Result<(), ScheduleError> {
    if !paths.launch_agent.exists() {
        writeln!(output, "{LABEL} is not installed").map_err(|source| ScheduleError::Write {
            path: PathBuf::from("stdout"),
            source,
        })?;
        return Ok(());
    }
    if !paths.settings.exists() {
        return Err(ScheduleError::InvalidSettings {
            path: paths.settings.clone(),
            message: "settings are missing; run `lop schedule install` to repair the schedule"
                .to_owned(),
        });
    }
    let settings = load_settings(&paths.settings)?;
    let loaded = backend.is_loaded()?;
    writeln!(
        output,
        "{LABEL} is installed and {}; mode={}, interval_seconds={}",
        if loaded { "loaded" } else { "not loaded" },
        settings.mode.label(),
        settings.interval_seconds
    )
    .map_err(|source| ScheduleError::Write {
        path: PathBuf::from("stdout"),
        source,
    })?;
    Ok(())
}

fn edit(
    paths: &SchedulePaths,
    backend: &MacOsBackend,
    output: &mut impl Write,
) -> Result<(), ScheduleError> {
    if !paths.settings.exists() || !paths.launch_agent.exists() {
        return Err(ScheduleError::NotInstalled);
    }
    let current = load_settings(&paths.settings)?;
    let editor = env::var_os("VISUAL")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("EDITOR").filter(|value| !value.is_empty()))
        .ok_or(ScheduleError::MissingEditor)?;
    let editor_name = editor.to_string_lossy().into_owned();
    let parent = paths
        .settings
        .parent()
        .ok_or_else(|| ScheduleError::Write {
            path: paths.settings.clone(),
            source: std::io::Error::other("settings path has no parent directory"),
        })?;
    fs::create_dir_all(parent).map_err(|source| ScheduleError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    let edit_path = parent.join(format!(".schedule.toml.edit-{}", std::process::id()));
    write_atomic(&edit_path, current.encode().as_bytes(), 0o600)?;

    let editor_status = match Command::new(&editor).arg(&edit_path).status() {
        Ok(status) => status,
        Err(source) => {
            let _ = fs::remove_file(&edit_path);
            return Err(ScheduleError::Spawn {
                program: editor_name,
                source,
            });
        }
    };
    if !editor_status.success() {
        let _ = fs::remove_file(&edit_path);
        return Err(ScheduleError::EditorFailed {
            editor: editor_name,
            status: editor_status,
        });
    }

    let edited = load_settings(&edit_path);
    let _ = fs::remove_file(&edit_path);
    let edited = edited?;
    let environment = prepare_environment()?;
    preflight_credentials(&environment)?;
    let agent = generate_launch_agent(&edited, &environment, paths);

    write_atomic(&paths.settings, edited.encode().as_bytes(), 0o600)?;
    if let Err(error) = backend.install(&agent.contents) {
        let _ = write_atomic(&paths.settings, current.encode().as_bytes(), 0o600);
        return Err(error);
    }
    writeln!(
        output,
        "updated {LABEL}; mode={}, interval_seconds={}",
        edited.mode.label(),
        edited.interval_seconds
    )
    .map_err(|source| ScheduleError::Write {
        path: PathBuf::from("stdout"),
        source,
    })?;
    Ok(())
}

fn uninstall(
    paths: &SchedulePaths,
    backend: &MacOsBackend,
    output: &mut impl Write,
) -> Result<(), ScheduleError> {
    let had_launch_agent = paths.launch_agent.exists();
    let was_installed = had_launch_agent || paths.settings.exists();
    if had_launch_agent {
        backend.uninstall()?;
    }
    remove_if_exists(&paths.settings)?;
    writeln!(
        output,
        "{LABEL} {}",
        if was_installed {
            "was uninstalled"
        } else {
            "is already uninstalled"
        }
    )
    .map_err(|source| ScheduleError::Write {
        path: PathBuf::from("stdout"),
        source,
    })?;
    Ok(())
}

fn load_settings(path: &Path) -> Result<ScheduleSettings, ScheduleError> {
    let contents = fs::read_to_string(path).map_err(|source| ScheduleError::ReadSettings {
        path: path.to_path_buf(),
        source,
    })?;
    let settings: ScheduleSettings =
        toml::from_str(&contents).map_err(|error| ScheduleError::InvalidSettings {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    settings
        .validate()
        .map_err(|message| ScheduleError::InvalidSettings {
            path: path.to_path_buf(),
            message,
        })?;
    Ok(settings)
}

fn prepare_environment() -> Result<ScheduleEnvironment, ScheduleError> {
    let lop = env::current_exe().map_err(|source| ScheduleError::Spawn {
        program: "current executable".to_owned(),
        source,
    })?;
    let git = find_executable("git")?;
    let worktrunk = find_executable("wt")?;
    let mut directories = BTreeSet::new();
    for program in [&lop, &git, &worktrunk] {
        if let Some(parent) = program.parent() {
            directories.insert(parent.to_path_buf());
        }
    }
    directories.insert(PathBuf::from("/usr/bin"));
    directories.insert(PathBuf::from("/bin"));
    let path = env::join_paths(directories)
        .map_err(|error| ScheduleError::Discovery(error.to_string()))?
        .to_string_lossy()
        .into_owned();

    Ok(ScheduleEnvironment {
        programs: Programs {
            lop,
            git,
            worktrunk,
            path,
        },
        ssh_auth_sock: env::var_os("SSH_AUTH_SOCK")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    })
}

fn find_executable(name: &'static str) -> Result<PathBuf, ScheduleError> {
    let Some(path) = env::var_os("PATH") else {
        return Err(ScheduleError::MissingExecutable { name });
    };
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(ScheduleError::MissingExecutable { name })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn preflight_credentials(environment: &ScheduleEnvironment) -> Result<(), ScheduleError> {
    let app_paths = crate::paths::AppPaths::from_env()
        .map_err(|error| ScheduleError::Discovery(error.to_string()))?;
    let config = Config::load(&app_paths.config_file)
        .map_err(|error| ScheduleError::Discovery(error.to_string()))?;
    let discovery = discover(&config.roots, config.scan_depth);
    if !discovery.issues.is_empty() {
        return Err(ScheduleError::Discovery(
            discovery
                .issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path.display(), issue.message))
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let mut requires_ssh = false;
    for repository in &discovery.repositories {
        let remotes = command_output(
            &environment.programs.git,
            &["-C", &repository.path.to_string_lossy(), "remote", "-v"],
            &[],
            COMMAND_TIMEOUT,
        )?;
        if !remotes.status.success() {
            return Err(ScheduleError::CredentialPreflight {
                repository: repository.path.clone(),
                message: stderr_message(&remotes.stderr),
            });
        }
        requires_ssh |= String::from_utf8_lossy(&remotes.stdout)
            .lines()
            .any(remote_uses_ssh);
    }

    if requires_ssh {
        validate_ssh_agent(environment.ssh_auth_sock.as_deref())?;
    } else if let Some(socket) = environment.ssh_auth_sock.as_deref() {
        validate_socket(socket)?;
    }

    let mut variables = vec![
        ("PATH", environment.programs.path.as_str()),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_SSH_COMMAND", "/usr/bin/ssh -o BatchMode=yes"),
    ];
    let socket_value = environment
        .ssh_auth_sock
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    if let Some(value) = socket_value.as_deref() {
        variables.push(("SSH_AUTH_SOCK", value));
    }

    for repository in discovery.repositories {
        let output = command_output(
            &environment.programs.git,
            &[
                "-C",
                &repository.path.to_string_lossy(),
                "fetch",
                "--all",
                "--prune",
                "--dry-run",
                "--no-recurse-submodules",
            ],
            &variables,
            Duration::from_secs(config.fetch_timeout_seconds),
        )?;
        if !output.status.success() {
            return Err(ScheduleError::CredentialPreflight {
                repository: repository.path,
                message: format!(
                    "`git fetch --dry-run` failed with prompts disabled: {}",
                    stderr_message(&output.stderr)
                ),
            });
        }
    }
    Ok(())
}

fn remote_uses_ssh(line: &str) -> bool {
    let Some(url) = line.split_whitespace().nth(1) else {
        return false;
    };
    url.starts_with("ssh://")
        || (!url.contains("://")
            && url.contains(':')
            && url.split(':').next().is_some_and(|host| host.contains('@')))
}

#[cfg(unix)]
fn validate_socket(path: &Path) -> Result<(), ScheduleError> {
    use std::os::unix::fs::FileTypeExt;

    if fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        Ok(())
    } else {
        Err(ScheduleError::InvalidSshAgent(path.to_path_buf()))
    }
}

#[cfg(not(unix))]
fn validate_socket(path: &Path) -> Result<(), ScheduleError> {
    if path.exists() {
        Ok(())
    } else {
        Err(ScheduleError::InvalidSshAgent(path.to_path_buf()))
    }
}

fn validate_ssh_agent(socket: Option<&Path>) -> Result<(), ScheduleError> {
    let socket = socket.ok_or(ScheduleError::MissingSshAgent)?;
    validate_socket(socket)?;
    let ssh_add = find_executable("ssh-add")?;
    let socket_text = socket.to_string_lossy();
    let output = command_output(
        &ssh_add,
        &["-l"],
        &[("SSH_AUTH_SOCK", &socket_text)],
        COMMAND_TIMEOUT,
    )?;
    match output.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(ScheduleError::EmptySshAgent),
        _ => Err(ScheduleError::SshAgent(stderr_message(&output.stderr))),
    }
}

fn generate_launch_agent(
    settings: &ScheduleSettings,
    environment: &ScheduleEnvironment,
    paths: &SchedulePaths,
) -> LaunchAgent {
    let mut arguments = vec![environment.programs.lop.to_string_lossy().into_owned()];
    match settings.mode {
        ScheduleMode::Scan => arguments.push("scan".to_owned()),
        ScheduleMode::Prune => {
            arguments.push("prune".to_owned());
            arguments.push("--yes".to_owned());
        }
    }
    let mut argument_xml = String::new();
    for argument in &arguments {
        fmt::write(
            &mut argument_xml,
            format_args!("        <string>{}</string>\n", xml_escape(argument)),
        )
        .expect("writing to a String cannot fail");
    }
    let ssh_auth_sock = environment
        .ssh_auth_sock
        .as_ref()
        .map_or_else(String::new, |path| {
            format!(
                "        <key>SSH_AUTH_SOCK</key>\n        <string>{}</string>\n",
                xml_escape(&path.to_string_lossy())
            )
        });
    let stdout = paths.log_directory.join("lop.out.log");
    let stderr = paths.log_directory.join("lop.err.log");

    LaunchAgent {
        contents: format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n    <key>Label</key>\n    <string>{LABEL}</string>\n    <key>ProgramArguments</key>\n    <array>\n{argument_xml}    </array>\n    <key>RunAtLoad</key>\n    <true/>\n    <key>StartInterval</key>\n    <integer>{}</integer>\n    <key>EnvironmentVariables</key>\n    <dict>\n        <key>GIT_TERMINAL_PROMPT</key>\n        <string>0</string>\n        <key>GIT_SSH_COMMAND</key>\n        <string>/usr/bin/ssh -o BatchMode=yes</string>\n        <key>PATH</key>\n        <string>{}</string>\n{ssh_auth_sock}    </dict>\n    <key>StandardOutPath</key>\n    <string>{}</string>\n    <key>StandardErrorPath</key>\n    <string>{}</string>\n</dict>\n</plist>\n",
            settings.interval_seconds,
            xml_escape(&environment.programs.path),
            xml_escape(&stdout.to_string_lossy()),
            xml_escape(&stderr.to_string_lossy())
        ),
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug)]
struct MacOsBackend {
    launchctl: PathBuf,
    domain: String,
    launch_agent: PathBuf,
    log_directory: PathBuf,
}

impl MacOsBackend {
    fn system(paths: &SchedulePaths) -> Result<Self, ScheduleError> {
        let output = command_output(Path::new("/usr/bin/id"), &["-u"], &[], LAUNCHCTL_TIMEOUT)?;
        if !output.status.success() {
            return Err(ScheduleError::UserId(stderr_message(&output.stderr)));
        }
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ScheduleError::UserId(uid));
        }
        Ok(Self {
            launchctl: PathBuf::from("/bin/launchctl"),
            domain: format!("gui/{uid}"),
            launch_agent: paths.launch_agent.clone(),
            log_directory: paths.log_directory.clone(),
        })
    }

    fn is_loaded(&self) -> Result<bool, ScheduleError> {
        let target = format!("{}/{LABEL}", self.domain);
        let output = command_output(&self.launchctl, &["print", &target], &[], LAUNCHCTL_TIMEOUT)?;
        Ok(output.status.success())
    }

    fn install(&self, contents: &str) -> Result<(), ScheduleError> {
        let previous = fs::read(&self.launch_agent).ok();
        let was_loaded = self.is_loaded()?;
        fs::create_dir_all(&self.log_directory).map_err(|source| ScheduleError::Write {
            path: self.log_directory.clone(),
            source,
        })?;
        let parent = self
            .launch_agent
            .parent()
            .ok_or_else(|| ScheduleError::Write {
                path: self.launch_agent.clone(),
                source: std::io::Error::other("LaunchAgent path has no parent directory"),
            })?;
        fs::create_dir_all(parent).map_err(|source| ScheduleError::Write {
            path: parent.to_path_buf(),
            source,
        })?;

        if was_loaded {
            self.bootout()?;
        }
        if let Err(error) = write_atomic(&self.launch_agent, contents.as_bytes(), 0o644) {
            if was_loaded {
                let _ = self.bootstrap();
            }
            return Err(error);
        }
        if let Err(error) = self.bootstrap() {
            match previous {
                Some(previous) => {
                    let _ = write_atomic(&self.launch_agent, &previous, 0o644);
                    if was_loaded {
                        let _ = self.bootstrap();
                    }
                }
                None => {
                    let _ = fs::remove_file(&self.launch_agent);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn uninstall(&self) -> Result<(), ScheduleError> {
        if self.is_loaded()? {
            self.bootout()?;
        }
        remove_if_exists(&self.launch_agent)
    }

    fn bootstrap(&self) -> Result<(), ScheduleError> {
        let path = self.launch_agent.to_string_lossy();
        let output = command_output(
            &self.launchctl,
            &["bootstrap", &self.domain, &path],
            &[],
            LAUNCHCTL_TIMEOUT,
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ScheduleError::Launchctl {
                action: "bootstrap",
                message: stderr_message(&output.stderr),
            })
        }
    }

    fn bootout(&self) -> Result<(), ScheduleError> {
        let target = format!("{}/{LABEL}", self.domain);
        let output = command_output(
            &self.launchctl,
            &["bootout", &target],
            &[],
            LAUNCHCTL_TIMEOUT,
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ScheduleError::Launchctl {
                action: "bootout",
                message: stderr_message(&output.stderr),
            })
        }
    }
}

fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<(), ScheduleError> {
    let parent = path.parent().ok_or_else(|| ScheduleError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::other("path has no parent directory"),
    })?;
    fs::create_dir_all(parent).map_err(|source| ScheduleError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options
            .open(&temporary)
            .map_err(|source| ScheduleError::Write {
                path: temporary.clone(),
                source,
            })?;
        set_permissions(&file, mode).map_err(|source| ScheduleError::Write {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|source| ScheduleError::Write {
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, path).map_err(|source| ScheduleError::Write {
            path: path.to_path_buf(),
            source,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_permissions(file: &fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_permissions(_file: &fs::File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), ScheduleError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ScheduleError::Write {
            path: path.to_path_buf(),
            source,
        }),
    }
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn command_output(
    program: &Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
    timeout: Duration,
) -> Result<CommandOutput, ScheduleError> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in environment {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|source| ScheduleError::Spawn {
        program: program.display().to_string(),
        source,
    })?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(ScheduleError::TimedOut(format!(
                    "{} {}",
                    program.display(),
                    arguments.join(" ")
                )));
            }
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(ScheduleError::Spawn {
                    program: program.display().to_string(),
                    source,
                });
            }
        }
    };
    let stdout = join_reader(stdout_reader, program)?;
    let stderr = join_reader(stderr_reader, program)?;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_all(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output)?;
    Ok(output)
}

fn join_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    program: &Path,
) -> Result<Vec<u8>, ScheduleError> {
    reader
        .join()
        .map_err(|_| ScheduleError::Spawn {
            program: program.display().to_string(),
            source: std::io::Error::other("command output reader panicked"),
        })?
        .map_err(|source| ScheduleError::Spawn {
            program: program.display().to_string(),
            source,
        })
}

fn stderr_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();
    if message.is_empty() {
        "command returned a nonzero status without an error message".to_owned()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::tempdir;

    use super::{
        LaunchAgent, MacOsBackend, Programs, ScheduleEnvironment, ScheduleMode, SchedulePaths,
        ScheduleSettings, ensure_supported_platform, generate_launch_agent, load_settings,
        remote_uses_ssh,
    };

    fn fixture() -> (ScheduleSettings, ScheduleEnvironment, SchedulePaths) {
        (
            ScheduleSettings::default(),
            ScheduleEnvironment {
                programs: Programs {
                    lop: PathBuf::from("/nix/store/lop/bin/lop"),
                    git: PathBuf::from("/nix/store/git/bin/git"),
                    worktrunk: PathBuf::from("/nix/store/worktrunk/bin/wt"),
                    path: "/bin:/nix/store/git/bin:/nix/store/lop/bin:/nix/store/worktrunk/bin:/usr/bin"
                        .to_owned(),
                },
                ssh_auth_sock: Some(PathBuf::from("/private/tmp/agent.sock")),
            },
            SchedulePaths {
                settings: PathBuf::from("/Users/test/.config/lop/schedule.toml"),
                launch_agent: PathBuf::from(
                    "/Users/test/Library/LaunchAgents/org.nixos.lop.plist",
                ),
                log_directory: PathBuf::from("/Users/test/Library/Logs/org.nixos"),
            },
        )
    }

    #[test]
    fn unsupported_platform_is_actionable() {
        assert_eq!(
            ensure_supported_platform("linux").unwrap_err().to_string(),
            "scheduling is unavailable on linux; scan and prune remain available"
        );
    }

    #[test]
    fn launch_agent_matches_snapshot() {
        let (settings, environment, paths) = fixture();
        let agent = generate_launch_agent(&settings, &environment, &paths);
        assert_eq!(
            agent.contents,
            include_str!("../tests/snapshots/launch_agent.plist")
        );
    }

    #[test]
    fn prune_mode_is_explicitly_confirmed() {
        let (mut settings, environment, paths) = fixture();
        settings.mode = ScheduleMode::Prune;
        let agent = generate_launch_agent(&settings, &environment, &paths);
        assert!(agent.contents.contains("<string>prune</string>"));
        assert!(agent.contents.contains("<string>--yes</string>"));
    }

    #[test]
    fn settings_reject_unknown_fields_and_tight_intervals() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("schedule.toml");
        fs::write(&path, "mode = 'scan'\ninterval_seconds = 30\n").unwrap();
        assert!(
            load_settings(&path)
                .unwrap_err()
                .to_string()
                .contains("between 300")
        );
        fs::write(
            &path,
            "mode = 'scan'\ninterval_seconds = 1800\nretry = true\n",
        )
        .unwrap();
        assert!(load_settings(&path).is_err());
    }

    #[test]
    fn recognizes_common_ssh_remote_syntax() {
        assert!(remote_uses_ssh(
            "origin\tgit@github.com:owner/repo.git (fetch)"
        ));
        assert!(remote_uses_ssh(
            "origin ssh://git@example.test/repo (fetch)"
        ));
        assert!(!remote_uses_ssh(
            "origin https://github.com/owner/repo.git (fetch)"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn macos_backend_install_and_uninstall_are_idempotent() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let state = directory.path().join("loaded");
        let script = directory.path().join("launchctl");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\nprint) test -f {state:?} ;;\nbootstrap) touch {state:?} ;;\nbootout) rm -f {state:?} ;;\n*) exit 2 ;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let launch_agent = directory.path().join("LaunchAgents/org.nixos.lop.plist");
        let backend = MacOsBackend {
            launchctl: script,
            domain: "gui/501".to_owned(),
            launch_agent: launch_agent.clone(),
            log_directory: directory.path().join("logs"),
        };
        let agent = LaunchAgent {
            contents: "plist-v1".to_owned(),
        };

        backend.install(&agent.contents).unwrap();
        backend.install(&agent.contents).unwrap();
        assert!(backend.is_loaded().unwrap());
        assert_eq!(fs::read_to_string(&launch_agent).unwrap(), "plist-v1");
        backend.uninstall().unwrap();
        backend.uninstall().unwrap();
        assert!(!launch_agent.exists());
        assert!(!backend.is_loaded().unwrap());
    }
}
