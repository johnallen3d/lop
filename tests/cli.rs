use std::{fs, process::Command};

use serde_json::Value;
use tempfile::tempdir;

fn lop() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lop"))
}

#[test]
fn missing_configuration_is_an_operational_failure() {
    let directory = tempdir().unwrap();
    let output = lop()
        .arg("scan")
        .env("HOME", directory.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("configuration not found")
    );
}

#[test]
fn scan_emits_ndjson_and_initializes_state() {
    let directory = tempdir().unwrap();
    let config_home = directory.path().join("config");
    let state_home = directory.path().join("state");
    fs::create_dir_all(config_home.join("lop")).unwrap();
    fs::write(
        config_home.join("lop/config.toml"),
        format!("roots = [{:?}]\n", directory.path().display().to_string()),
    )
    .unwrap();

    let output = lop()
        .arg("scan")
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["record"], "run_started");
    assert_eq!(records[0]["command"], "scan");
    assert_eq!(records[1]["record"], "summary");
    assert_eq!(records[1]["operational_failures"], 0);
    assert!(state_home.join("lop/state.json").is_file());
}

#[test]
fn scan_reports_discovered_repositories() {
    let directory = tempdir().unwrap();
    let config_home = directory.path().join("config");
    let state_home = directory.path().join("state");
    let repository = directory.path().join("sources/project");
    fs::create_dir_all(config_home.join("lop")).unwrap();
    fs::create_dir_all(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        config_home.join("lop/config.toml"),
        format!(
            "roots = [{:?}]\nscan_depth = 2\n",
            directory.path().join("sources").display().to_string()
        ),
    )
    .unwrap();

    let output = lop()
        .arg("scan")
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .output()
        .unwrap();

    assert!(output.status.success());
    let records: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert_eq!(records[1]["record"], "repository");
    assert_eq!(records[1]["reason_code"], "ok");
    assert_eq!(records[2]["repositories_scanned"], 1);
}

#[test]
fn inaccessible_scan_root_is_an_operational_failure() {
    let directory = tempdir().unwrap();
    let config_home = directory.path().join("config");
    let state_home = directory.path().join("state");
    fs::create_dir_all(config_home.join("lop")).unwrap();
    fs::write(
        config_home.join("lop/config.toml"),
        format!(
            "roots = [{:?}]\n",
            directory.path().join("missing").display().to_string()
        ),
    )
    .unwrap();

    let output = lop()
        .arg("scan")
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let records: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[1]["record"], "discovery_failure");
    assert_eq!(records[1]["reason_code"], "inaccessible_root");
    assert_eq!(records[2]["operational_failures"], 1);
}

#[test]
fn prune_requires_yes_to_apply() {
    let directory = tempdir().unwrap();
    let config_home = directory.path().join("config");
    let state_home = directory.path().join("state");
    fs::create_dir_all(config_home.join("lop")).unwrap();
    fs::write(
        config_home.join("lop/config.toml"),
        format!("roots = [{:?}]\n", directory.path().display().to_string()),
    )
    .unwrap();

    for (arguments, apply) in [(&["prune"][..], false), (&["prune", "--yes"][..], true)] {
        let output = lop()
            .args(arguments)
            .env("HOME", directory.path())
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_STATE_HOME", &state_home)
            .output()
            .unwrap();
        assert!(output.status.success());
        let first: Value = serde_json::from_str(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["apply"], apply);
    }
}
