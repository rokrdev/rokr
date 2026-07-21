//! Pre-ship code review fixes (F-003, F-009) for `rokr eval`, exercised at
//! the real CLI level (spawning the actual `rokr` binary via `assert_cmd`,
//! mirroring `headless_test.rs`'s own convention) rather than in-process --
//! both findings are about what actually reaches stderr/the exit code for a
//! real operator running `rokr eval` from a shell.

use assert_cmd::Command;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Creates a fresh, uniquely-named directory under the system temp dir,
/// mirroring `headless_test.rs`'s own helper of the same name.
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rokr-eval-cli-test-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// F-003 (pre-ship review): `aggregate_pass_rate(&[])` returning `1.0` (a
/// deliberate vacuous pass for the THRESHOLD MATH -- see that fn's own doc
/// comment) must never let `rokr eval` on a cases dir that loaded ZERO
/// cases exit 0 as if everything passed. `rokr eval <existing-empty-dir>`
/// (the default, `--report text`, path) must exit nonzero with a clear
/// "no cases" message on stderr.
#[tokio::test]
async fn eval_command_on_empty_cases_dir_exits_nonzero_with_no_cases_message_text_report() {
    let cases_dir = unique_temp_dir("empty-text");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd.arg("eval").arg(&cases_dir).assert().failure();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no eval cases found"),
        "expected stderr to report that no eval cases were found under the empty dir, got: \
         {stderr:?}"
    );
    assert!(
        stderr.contains(&cases_dir.display().to_string()),
        "expected stderr to name the empty cases dir itself, got: {stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// The other half of F-003's Done-when: the SAME empty-cases-dir failure,
/// but on the `--report json` path -- both output-format paths must inherit
/// the fix from the same seam (`rokr_eval::run_eval` returning `Err`),
/// rather than the json path having its own separate (and possibly missed)
/// check.
#[tokio::test]
async fn eval_command_on_empty_cases_dir_exits_nonzero_with_no_cases_message_json_report() {
    let cases_dir = unique_temp_dir("empty-json");

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("eval")
        .arg(&cases_dir)
        .arg("--report")
        .arg("json")
        .assert()
        .failure();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no eval cases found"),
        "expected stderr to report that no eval cases were found under the empty dir on the \
         --report json path too, got: {stderr:?}"
    );

    // The json path must NOT have printed a (vacuously-passing) aggregate
    // report to stdout for zero loaded cases.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().is_empty(),
        "expected no aggregate report on stdout when zero cases were loaded, got: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// F-009 (pre-ship review): when an LLM-judge assertion's provider call
/// fails, the failure used to be dropped silently. It must now be logged to
/// stderr (naming the case, the rubric, and the underlying error) WITHOUT
/// flipping the case's own deterministic pass/fail outcome -- this case has
/// no deterministic assertions at all, so it must still report PASS (exit
/// 0) even though its judge-rubric assertion's own provider call fails.
#[tokio::test]
async fn eval_command_judge_rubric_failure_logged_to_stderr_without_affecting_case_pass() {
    const PROMPT: &str = "say hi for judge failure logging EvalCliJudgeFailurePromptMarker7734";
    const AGENT_MARKER: &str = "EvalCliJudgeFailureAgentReplyMarker7734";
    const RUBRIC: &str = "did the agent do a good job EvalCliJudgeFailureRubricMarker7734";

    let mock = MockServer::start().await;
    // The main headless turn succeeds normally.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(PROMPT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": AGENT_MARKER}}]
        })))
        .mount(&mock)
        .await;
    // The judge's own scoring call fails (500) -- this is the failure F-009
    // requires to be logged rather than silently dropped.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(RUBRIC))
        .respond_with(ResponseTemplate::new(500).set_body_string("judge backend unavailable"))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("judge-fail-home");
    let xdg_config_home = unique_temp_dir("judge-fail-xdg-config-home");
    let cases_dir = unique_temp_dir("judge-fail-cases");

    std::fs::write(
        cases_dir.join("judge-failure-case.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "prompt": PROMPT,
            "model": "gpt-4o-mini",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [],
            "assertions": [{"type": "judge_rubric", "rubric": RUBRIC}]
        }))
        .unwrap(),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("rokr").unwrap();
    let assert = cmd
        .arg("eval")
        .arg(&cases_dir)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("ROKR_OPENAI_BASE_URL", mock.uri())
        .env("ROKR_OPENAI_MODEL", "gpt-4o-mini")
        .env("ROKR_OPENAI_API_KEY", "test-key")
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("PASS judge-failure-case"),
        "expected the case to report PASS despite its judge-rubric assertion's own provider \
         call failing (a judge score is a tracked metric, never a pass/fail gate), got stdout: \
         {stdout:?}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("judge-failure-case"),
        "expected stderr to name the case, got: {stderr:?}"
    );
    assert!(
        stderr.contains(RUBRIC),
        "expected stderr to include the rubric text, got: {stderr:?}"
    );
    assert!(
        stderr.contains('5') && stderr.to_lowercase().contains("judge"),
        "expected stderr to include the underlying judge error (a 500 status), got: {stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}
