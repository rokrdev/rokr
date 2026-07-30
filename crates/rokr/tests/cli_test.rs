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

/// Ticket 32 (provider-factory-seam) RED: proves the *active* provider
/// construction path in `main.rs` is (or isn't yet) resilience-wrapped, by
/// driving the real running `rokr` binary end to end through a genuine PTY
/// -- exactly as a human would type into the TUI -- rather than unit-testing
/// `rokr_provider::factory::build_provider` in isolation (that's already
/// GREEN in `crates/rokr-provider/src/factory.rs`; what's NOT yet proven is
/// that `main.rs` actually calls it).
///
/// A plain piped subprocess can't exercise this: `rokr_tui::run` checks
/// `io::stdout().is_terminal()` and immediately errors out with "not a tty"
/// if stdout isn't a real terminal, so the interactive submit path is only
/// reachable via a real PTY (`portable_pty`).
///
/// The mock server deliberately returns a retryable 503 once, then 200: if
/// the active path wraps the provider in the resilience decorator, the turn
/// succeeds and the mock records exactly 2 requests; if it doesn't (today's
/// state -- `main.rs`'s `construct_provider` has no retry wrapping), the
/// turn fails after the single 503, "Error: ..." is what appears in the
/// TUI's scrollback instead of the reply, and the mock records only 1
/// request.
#[tokio::test]
async fn startup_uses_factory_constructed_provider_for_submit() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MARKER: &str = "acceptancetestreply4471";

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock_server)
        .await;

    let home = unique_temp_dir("factory-seam-home");
    let xdg_config_home = unique_temp_dir("factory-seam-xdg-config-home");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_rokr"));
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", &xdg_config_home);
    cmd.env("ROKR_OPENAI_BASE_URL", mock_server.uri());
    cmd.env("ROKR_OPENAI_MODEL", "gpt-4o-mini");
    cmd.env("ROKR_OPENAI_API_KEY", "test-key");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("failed to spawn rokr binary under pty");
    // Drop our handle to the slave so the master's reader observes EOF once
    // the child exits, and so we're not holding the slave open ourselves.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone pty reader");
    let mut writer = pair
        .master
        .take_writer()
        .expect("failed to take pty writer");

    // Blocking reads happen on a plain thread so the async test body can
    // poll the accumulated output without blocking the tokio runtime.
    let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let reader_output = Arc::clone(&output);
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => reader_output.lock().unwrap().extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });

    // Wait for the TUI to actually render its panes (proving raw mode is
    // enabled and the event loop is reading stdin) before typing anything --
    // a fixed sleep here is a race: on a slow/cold start the child may not
    // yet be reading stdin, so keystrokes sent too early can be dropped or
    // delivered to the terminal's canonical-mode line buffer instead of the
    // app. This mirrors tui_test.rs's proven readiness wait.
    let ready_timeout = std::time::Duration::from_secs(10);
    let ready_start = std::time::Instant::now();
    loop {
        let snapshot = String::from_utf8_lossy(&output.lock().unwrap()).to_string();
        if snapshot.contains("Header") && snapshot.contains("View") && snapshot.contains("Prompt")
        {
            break;
        }
        assert!(
            ready_start.elapsed() < ready_timeout,
            "TUI did not render its panes within {ready_timeout:?}; output so far: {snapshot:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // crossterm's raw-mode Enter event is conventionally carriage return
    // (0x0D), not line feed -- send the prompt and `\r` in a single write, as
    // tui_test.rs's proven working tests do, to avoid any race between two
    // separate writes.
    writer
        .write_all(b"say hello\r")
        .expect("failed to write prompt text to pty");
    let _ = writer.flush();

    let timeout = std::time::Duration::from_secs(15);
    let start = std::time::Instant::now();
    let mut found = false;
    let mut last_snapshot = String::new();
    while start.elapsed() < timeout {
        last_snapshot = String::from_utf8_lossy(&output.lock().unwrap()).to_string();
        if last_snapshot.contains(MARKER) {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let requests_so_far = mock_server
        .received_requests()
        .await
        .map(|r| r.len())
        .unwrap_or(0);

    // Clean up the child/pty/thread unconditionally, before asserting, so a
    // failing assertion below can never leave the child process or reader
    // thread hanging around.
    let _ = writer.write_all(b"q");
    let _ = writer.flush();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    drop(writer);
    let _ = reader_thread.join();

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);

    let tail: String = {
        let chars: Vec<char> = last_snapshot.chars().collect();
        let start_idx = chars.len().saturating_sub(500);
        chars[start_idx..].iter().collect()
    };
    assert!(
        found,
        "expected marker {MARKER:?} to appear in the TUI's scrollback output within {timeout:?} \
         (proving the submit path reached the mock and got a reply back), but it never did; \
         {requests_so_far} request(s) were recorded against the mock server so far; \
         last ~500 chars of captured output: {tail:?}"
    );

    assert_eq!(
        mock_server.received_requests().await.unwrap().len(),
        2,
        "expected exactly 2 requests against the mock (1 retried 503 + 1 successful 200), \
         proving the active provider construction path in main.rs retries a retryable failure \
         end-to-end through the real running binary -- if this is 1, main.rs is still using \
         construct_provider (no resilience wrapping) instead of \
         rokr_provider::factory::build_provider"
    );
}


/// Ticket 52 (clap-and-sessionrunner-extraction) acceptance test: the
/// clap-generated `--help` must enumerate the same surface today's
/// hand-rolled `USAGE` string documents -- `--agent`, `--resume`,
/// `--continue`, and the `auth` (login) subcommand -- printed to stdout and
/// exiting 0. RED before this ticket: the pre-clap binary had no `--help`
/// handling at all; `--help` fell through the `--version`/`auth login`
/// match to `parse_agent_tier`, which rejected it, printed `USAGE` to
/// STDERR, and exited nonzero -- so `.assert().success()` fails today and
/// only passes once clap owns argument parsing.
#[test]
fn rokr_help_lists_agent_resume_continue_and_auth_login_flags() {
    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd.arg("--help").assert().success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in ["--agent", "--resume", "--continue", "auth"] {
        assert!(
            stdout.contains(needle),
            "expected clap-generated `rokr --help` stdout to enumerate {needle:?} \
             (matching today's USAGE string), got: {stdout}"
        );
    }
}

/// Ticket 53 (shell-completions-subcommand) acceptance test: `rokr
/// completions zsh` must print a valid zsh completion script to stdout and
/// exit 0. RED before this ticket: `completions` isn't a recognized
/// subcommand yet, so clap rejects it as unknown and exits nonzero.
#[test]
fn rokr_completions_zsh_prints_valid_completion_script_to_stdout() {
    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd.arg("completions").arg("zsh").assert().success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("#compdef"),
        "expected `rokr completions zsh` stdout to look like a zsh completion script \
         (contain '#compdef'), got: {stdout}"
    );
    for needle in ["auth", "completions"] {
        assert!(
            stdout.contains(needle),
            "expected `rokr completions zsh` stdout to mention the {needle:?} subcommand, \
             got: {stdout}"
        );
    }
}

/// Ticket 67 (self-update-rokr-upgrade) acceptance test: `rokr upgrade`
/// against a Homebrew-managed install (its own binary path resolving under
/// a Homebrew Cellar prefix -- forced here via the test-only
/// ROKR_UPGRADE_EXE_PATH_OVERRIDE env var, since the actual compiled test
/// binary never really lives under /Cellar/) prints guidance directing the
/// user to `brew upgrade` and exits 0, performing no axoupdater update
/// check at all -- proven by never setting ROKR_UPGRADE_MOCK_CHECK_OUTCOME:
/// if the Homebrew branch didn't short-circuit before constructing a
/// checker, there would be nothing configured for it to fall back to and
/// the run would fail or hang instead of cleanly succeeding. RED before
/// this ticket: `upgrade` isn't a recognized subcommand yet, so clap
/// rejects it as unknown and exits nonzero.
#[test]
fn rokr_upgrade_declines_and_prints_brew_upgrade_guidance_for_homebrew_managed_install() {
    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("upgrade")
        .env(
            "ROKR_UPGRADE_EXE_PATH_OVERRIDE",
            "/opt/homebrew/Cellar/rokr/1.2.3/bin/rokr",
        )
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("brew upgrade"),
        "expected stdout to direct the user to `brew upgrade`, got: {stdout:?}"
    );
}

/// Ticket 67 (self-update-rokr-upgrade) acceptance test: `rokr upgrade`
/// against a non-Homebrew install invokes the axoupdater-backed update
/// check -- proven here via the test-only ROKR_UPGRADE_MOCK_CHECK_OUTCOME
/// seam (no real network call is made in tests; see upgrade.rs's
/// UpdateChecker trait), which stands in for the real AxoUpdateChecker.
/// Forcing a specific "update available" outcome and asserting the printed
/// version string flows straight through to stdout proves the non-Homebrew
/// branch actually calls into the checker and acts on its result, rather
/// than e.g. always taking the Homebrew short-circuit or silently no-op'ing.
/// RED before this ticket: `upgrade` isn't a recognized subcommand at all
/// yet; even once it is (see the Homebrew-managed test above), this stays
/// RED until the mock-outcome env var is actually read and acted on.
#[test]
fn rokr_upgrade_invokes_axoupdater_check_for_non_homebrew_install() {
    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("upgrade")
        .env(
            "ROKR_UPGRADE_EXE_PATH_OVERRIDE",
            "/Users/bharat/.cargo/bin/rokr",
        )
        .env("ROKR_UPGRADE_MOCK_CHECK_OUTCOME", "update-available:9.9.9")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("9.9.9"),
        "expected stdout to report the mocked new version 9.9.9 (proving the non-Homebrew \
         branch invoked and acted on the update checker), got: {stdout:?}"
    );
}
