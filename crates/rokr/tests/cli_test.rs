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
    let dir = std::env::temp_dir().join(format!(
        "rokr-cli-test-{label}-{}-{nanos}",
        std::process::id()
    ));
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

#[test]
fn first_run_scaffolds_agent_prompt_files() {
    let home = unique_temp_dir("home");
    let xdg_config_home = unique_temp_dir("xdg-config-home");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    cmd.env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .assert()
        .success();

    let agents_dir = xdg_config_home.join("rokr").join("agents");
    let plan_contents = std::fs::read_to_string(agents_dir.join("plan.md")).unwrap_or_else(|e| {
        panic!("expected plan.md at {agents_dir:?}: {e}");
    });
    let build_contents = std::fs::read_to_string(agents_dir.join("build.md")).unwrap_or_else(|e| {
        panic!("expected build.md at {agents_dir:?}: {e}");
    });

    assert!(
        !plan_contents.trim().is_empty(),
        "expected plan.md to have non-empty content"
    );
    assert!(
        !build_contents.trim().is_empty(),
        "expected build.md to have non-empty content"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

#[test]
fn existing_v1_config_without_compaction_fields_left_byte_identical_after_run() {
    let home = unique_temp_dir("home");
    let xdg_config_home = unique_temp_dir("xdg-config-home");

    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("rokr.json");
    let existing = "{\"version\": 1}";
    std::fs::write(&config_path, existing).unwrap();

    let before = std::fs::read(&config_path).unwrap();

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    cmd.env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .assert()
        .success();

    let after = std::fs::read(&config_path).unwrap();
    assert_eq!(
        before, after,
        "existing v1 config file lacking compaction fields must be left byte-identical after run"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}

#[test]
fn unknown_flag_exits_nonzero_with_usage_message() {
    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd.arg("--bogus").assert().failure();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Usage:"),
        "expected stderr to contain 'Usage:', got: {stderr:?}"
    );
}

/// `rokr auth login` (ticket 31): drives the real binary end to end through
/// the manual-code-paste fallback (`ROKR_AUTH_NO_BROWSER=1`) against a
/// stubbed token endpoint, with no real network, browser, or OS keychain
/// involved (`ROKR_AUTH_FORCE_FILE_STORE=1` forces the deterministic
/// `0600` file-fallback path -- see `rokr_provider::auth`'s doc comments
/// for why: a real keychain can prompt an interactive permission dialog on
/// first access, which would hang this test).
///
/// Uses raw `std::process::Command` with piped stdio (not `assert_cmd`'s
/// `.assert()`, which runs the child to completion and can't interleave
/// reads/writes) so the authorization URL can be read from stdout, the
/// `state` extracted from it, and a matching `<code>#<state>` line written
/// back to stdin before the child is allowed to exit.
#[tokio::test]
async fn auth_login_command_completes_pkce_flow_and_stores_token() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/oauth/token"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "expires_in": 3600,
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("auth-login-home");
    let xdg_config_home = unique_temp_dir("auth-login-xdg-config-home");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rokr"))
        .arg("auth")
        .arg("login")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_AUTH_NO_BROWSER", "1")
        .env("ROKR_AUTH_FORCE_FILE_STORE", "1")
        .env(
            "ROKR_OAUTH_TOKEN_URL",
            format!("{}/oauth/token", mock_server.uri()),
        )
        .env(
            "ROKR_OAUTH_AUTHORIZE_URL",
            "https://example.invalid/oauth/authorize",
        )
        .env("ROKR_OAUTH_CLIENT_ID", "test-client-id")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn rokr binary");

    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut reader = BufReader::new(stdout);

    let mut auth_url_line = String::new();
    let n = reader
        .read_line(&mut auth_url_line)
        .expect("failed to read child stdout");
    assert!(n > 0, "child exited before printing the authorization URL");
    let auth_url_line = auth_url_line.trim().to_string();
    assert!(
        auth_url_line.contains("state="),
        "expected the authorization URL to contain a state query param, got: {auth_url_line:?}"
    );

    let state = auth_url_line
        .split("state=")
        .nth(1)
        .expect("authorization URL should contain a state query param")
        .split('&')
        .next()
        .unwrap()
        .to_string();
    assert!(!state.is_empty(), "extracted state should not be empty");

    {
        let stdin = child.stdin.as_mut().expect("child stdin should be piped");
        writeln!(stdin, "test-auth-code#{state}")
            .expect("failed to write pasted code to child stdin");
    }
    // Drop stdin so the child doesn't hang waiting for more input.
    child.stdin.take();

    let output = child
        .wait_with_output()
        .expect("failed to wait for child process");
    assert!(
        output.status.success(),
        "expected `rokr auth login` to exit successfully, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let token_path = xdg_config_home.join("rokr").join("oauth_token.json");
    let contents = std::fs::read_to_string(&token_path)
        .unwrap_or_else(|e| panic!("expected token file at {token_path:?}: {e}"));
    assert!(
        contents.contains("test-access-token"),
        "expected token file to contain the exchanged access token, got: {contents}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected token file permissions to be 0600, got {mode:o}"
        );
    }

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
}
