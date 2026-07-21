//! Ticket 58 (eval-case-runner-and-deterministic-assertions) acceptance
//! test: `rokr eval <cases-dir>` against a fixture directory with one
//! passing and one failing file-exists-assertion case reports them
//! correctly, each isolated in a fresh temp fixture dir, fresh session, and
//! pinned model.
//!
//! In-process (not spawned via `assert_cmd` like `crates/rokr/tests/
//! headless_test.rs`'s equivalent tests) -- `rokr-eval` cannot depend on
//! the `rokr` binary crate (that would be a dependency cycle: `rokr`
//! depends on `rokr-eval`). These tests therefore call `rokr_eval::run_eval`
//! directly and set real process env vars (`HOME`/`XDG_CONFIG_HOME`/
//! `ROKR_OPENAI_*`) the same way `headless_test.rs` sets them on its
//! spawned subprocess. Unlike `headless_test.rs`, this file now has more
//! than one test function, so every test acquires [`ENV_LOCK`] before
//! touching process env vars, serializing them against each other so
//! `cargo test --test-threads=4` (which runs test functions in this binary
//! concurrently by default) never races two tests' env var writes.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serializes every test in this file's process env var reads/writes (see
/// this module's doc comment). Held for the duration of each test function
/// (a plain `std::sync::Mutex` guard, not `tokio::sync::Mutex` -- fine to
/// hold across `.await` here since every `#[tokio::test]` in this file uses
/// the default single-threaded `current_thread` runtime, which never
/// requires the test future to be `Send`).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Creates a fresh, uniquely-named directory under the system temp dir,
/// mirroring `headless_test.rs`'s own helper of the same name.
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rokr-eval-test-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_case(cases_dir: &std::path::Path, file_name: &str, value: serde_json::Value) {
    std::fs::write(
        cases_dir.join(file_name),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

/// A cases dir with one passing and one failing `file_exists` case, run via
/// `rokr_eval::run_eval`, must report each correctly, with each case run in
/// its own fresh fixture dir -- proven two ways: (1) the two fixture dirs
/// are different paths, and (2) both cases assert the SAME filename
/// (`expected.txt`, present only in the passing case's `setup_files`); if
/// the failing case's fixture dir somehow leaked the passing case's file,
/// it would incorrectly report pass instead of fail.
#[tokio::test]
async fn eval_run_reports_passing_and_failing_file_exists_cases_from_isolated_fixture_dirs() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const MARKER: &str = "EvalCaseRunnerReplyMarker8814";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("home");
    let xdg_config_home = unique_temp_dir("xdg-config-home");
    // Safety: this test is the only test function in this binary (see this
    // file's module doc comment), so no other test races these env-var
    // writes.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("cases");
    write_case(
        &cases_dir,
        "passing-file-exists.json",
        serde_json::json!({
            "prompt": "say hi",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [{"path": "expected.txt", "contents": "present"}],
            "assertions": [{"type": "file_exists", "path": "expected.txt"}]
        }),
    );
    write_case(
        &cases_dir,
        "failing-file-exists.json",
        serde_json::json!({
            "prompt": "say hi",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [],
            "assertions": [{"type": "file_exists", "path": "expected.txt"}]
        }),
    );

    let outcomes = rokr_eval::run_eval(&cases_dir, false)
        .await
        .expect("eval run should succeed");
    assert_eq!(
        outcomes.len(),
        2,
        "expected exactly two case outcomes, got: {outcomes:?}"
    );

    let passing = outcomes
        .iter()
        .find(|outcome| outcome.name == "passing-file-exists")
        .expect("expected an outcome named passing-file-exists");
    let failing = outcomes
        .iter()
        .find(|outcome| outcome.name == "failing-file-exists")
        .expect("expected an outcome named failing-file-exists");

    assert!(
        passing.passed,
        "expected the passing case to report pass, got: {passing:?}"
    );
    assert!(
        !failing.passed,
        "expected the failing case to report fail, got: {failing:?}"
    );

    assert_ne!(
        passing.fixture_dir, failing.fixture_dir,
        "each case must run in its own fresh fixture dir"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// Team-lead review fix (follow-up to ticket 58): a case file requesting
/// `permission_mode: bypass` must NOT be able to grant itself the
/// equivalent of `--dangerously-skip-permissions` on its own -- that must
/// come from an explicit operator flag on the `rokr eval` invocation
/// itself (mirrors `rokr_app::headless::build_permission_requester`'s
/// existing precedent for the headless `-p` path). When `run_eval` is
/// called with `dangerously_skip_permissions: false` (the flag NOT
/// passed), a bypass-requesting case must come back as a failing
/// `CaseOutcome` with a `run_error` explaining the missing flag, and the
/// headless turn underneath it must never actually run -- proven here by
/// asserting the mock provider server received zero requests at all.
#[tokio::test]
async fn eval_run_bypass_case_without_operator_flag_fails_and_never_runs_headless_turn() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "should never be seen"}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("bypass-no-flag-home");
    let xdg_config_home = unique_temp_dir("bypass-no-flag-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("bypass-no-flag-cases");
    write_case(
        &cases_dir,
        "bypass-case.json",
        serde_json::json!({
            "prompt": "say hi",
            "agent": "plan",
            "permission_mode": "bypass",
            "setup_files": [],
            "assertions": []
        }),
    );

    let outcomes = rokr_eval::run_eval(&cases_dir, false)
        .await
        .expect("eval run should succeed at the whole-run level");
    assert_eq!(
        outcomes.len(),
        1,
        "expected exactly one case outcome, got: {outcomes:?}"
    );
    let outcome = &outcomes[0];

    assert!(
        !outcome.passed,
        "expected the bypass-requesting case to fail without the operator flag, got: {outcome:?}"
    );
    let run_error = outcome
        .run_error
        .as_ref()
        .expect("expected a run_error explaining the missing operator flag");
    assert!(
        run_error.contains("--dangerously-skip-permissions"),
        "expected run_error to mention the missing --dangerously-skip-permissions flag, got: \
         {run_error:?}"
    );

    let requests = mock
        .received_requests()
        .await
        .expect("mock server should support request recording");
    assert_eq!(
        requests.len(),
        0,
        "expected the headless turn to never run (zero requests against the mock provider) \
         for a bypass-requesting case with no operator flag, got {} request(s)",
        requests.len()
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// The other half of the fix above: when the operator DOES pass
/// `dangerously_skip_permissions: true` at the `run_eval` call (the `rokr
/// eval --dangerously-skip-permissions` case), a bypass-requesting case is
/// honored exactly like before this fix -- it must NOT get the "flag not
/// passed" error, and its outcome instead depends on the mocked provider
/// response, which this test controls (a plain successful reply, no tool
/// calls, no assertions) so the case is expected to pass outright.
#[tokio::test]
async fn eval_run_bypass_case_with_operator_flag_is_honored() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const MARKER: &str = "EvalCaseRunnerBypassHonoredMarker3391";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("bypass-with-flag-home");
    let xdg_config_home = unique_temp_dir("bypass-with-flag-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("bypass-with-flag-cases");
    write_case(
        &cases_dir,
        "bypass-case.json",
        serde_json::json!({
            "prompt": "say hi",
            "agent": "plan",
            "permission_mode": "bypass",
            "setup_files": [],
            "assertions": []
        }),
    );

    let outcomes = rokr_eval::run_eval(&cases_dir, true)
        .await
        .expect("eval run should succeed at the whole-run level");
    assert_eq!(
        outcomes.len(),
        1,
        "expected exactly one case outcome, got: {outcomes:?}"
    );
    let outcome = &outcomes[0];

    assert!(
        outcome.run_error.is_none(),
        "expected no run_error when the operator flag is passed, got: {outcome:?}"
    );
    assert!(
        outcome.passed,
        "expected the bypass-requesting case to be honored (pass) when the operator flag is \
         passed, got: {outcome:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}
