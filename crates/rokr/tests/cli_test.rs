use assert_cmd::Command;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn version_flag_prints_version_and_exits_zero() {
    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd.arg("--version").assert().success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected stdout to contain version {}, got: {stdout:?}",
        env!("CARGO_PKG_VERSION")
    );
}

/// Creates a fresh, uniquely-named directory under the system temp dir.
/// Avoids pulling in a `tempfile` dev-dependency for this crate.
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rokr-cli-test-{label}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn first_run_creates_config_file_with_version_one() {
    let home = unique_temp_dir("home");
    let xdg_config_home = unique_temp_dir("xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    cmd.env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .assert()
        .success();

    let config_path = xdg_config_home.join("rokr").join("rokr.json");
    let contents = std::fs::read_to_string(&config_path).unwrap_or_else(|e| {
        panic!("expected config file at {config_path:?} to exist: {e}");
    });
    assert!(
        contents.contains("\"version\": 1") || contents.contains("\"version\":1"),
        "expected config file to contain version 1, got: {contents}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}
