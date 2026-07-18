use assert_cmd::Command;

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
