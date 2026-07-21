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

use wiremock::matchers::{body_string_contains, method, path};
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
            "model": "gpt-4o-mini",
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
            "model": "gpt-4o-mini",
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
            "model": "gpt-4o-mini",
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
            "model": "gpt-4o-mini",
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

/// Ticket 59 (eval-llm-judge-scoring) acceptance test: a case with one
/// passing deterministic assertion (`file_exists`) and one LLM-judge
/// rubric assertion (scripted/mocked verdict) reports the case as PASSED
/// on the deterministic assertion alone, and the judge's score surfaces
/// separately via `rokr_eval::mean_judge_score` -- proving a judge score
/// never folds into `CaseOutcome::passed`/`assertion_outcomes`.
#[tokio::test]
async fn case_with_scripted_judge_response_contributes_score_without_affecting_deterministic_pass_fail(
) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const AGENT_MARKER: &str = "EvalJudgeScoringAgentReplyMarker5521";
    const PROMPT: &str = "say hi for judge scoring EvalJudgeScoringPromptMarker5521";
    const RUBRIC: &str = "Did the agent politely acknowledge the task? EvalJudgeScoringRubricMarker5521";

    let mock = MockServer::start().await;
    // The main headless turn's response -- matched by the case's own
    // prompt text, mutually exclusive with the judge call's rubric-bearing
    // body below so the two mocks never ambiguously both match the same
    // request.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(PROMPT))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": AGENT_MARKER}}]
        })))
        .mount(&mock)
        .await;
    // The judge's own scoring call -- matched by the rubric text, which
    // `judge::score_rubric` embeds verbatim in its request body.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(RUBRIC))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "{\"score\": 0.6}"}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("judge-home");
    let xdg_config_home = unique_temp_dir("judge-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("judge-cases");
    write_case(
        &cases_dir,
        "judge-and-deterministic.json",
        serde_json::json!({
            "prompt": PROMPT,
            "model": "gpt-4o-mini",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [{"path": "expected.txt", "contents": "present"}],
            "assertions": [
                {"type": "file_exists", "path": "expected.txt"},
                {"type": "judge_rubric", "rubric": RUBRIC}
            ]
        }),
    );

    let outcomes = rokr_eval::run_eval(&cases_dir, false)
        .await
        .expect("eval run should succeed");
    assert_eq!(outcomes.len(), 1, "expected exactly one case outcome, got: {outcomes:?}");
    let outcome = &outcomes[0];

    assert!(
        outcome.passed,
        "expected the case to pass on its deterministic file_exists assertion alone, got: {outcome:?}"
    );
    assert_eq!(
        outcome.assertion_outcomes.len(),
        1,
        "expected only the deterministic assertion in assertion_outcomes (judge-rubric routed \
         separately), got: {:?}",
        outcome.assertion_outcomes
    );

    assert_eq!(
        outcome.judge_scores.len(),
        1,
        "expected exactly one judge score recorded for the case, got: {:?}",
        outcome.judge_scores
    );
    assert!(
        (outcome.judge_scores[0].score - 0.6).abs() < f64::EPSILON,
        "expected the scripted judge score to be recorded, got: {:?}",
        outcome.judge_scores[0]
    );

    let mean = rokr_eval::mean_judge_score(&outcomes)
        .expect("expected a mean judge score when at least one judge score was recorded");
    assert!(
        (mean - 0.6).abs() < f64::EPSILON,
        "expected mean_judge_score to equal the single recorded score 0.6, got: {mean}"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// Ticket 60 (eval-report-json-and-ci-gate) acceptance test: a cases dir
/// with one passing and one failing `file_exists` case (a known, exact 0.5
/// deterministic pass rate) run via `run_eval`, then folded through
/// `rokr_eval::report::build_report` at three different `pass_threshold`
/// values against that SAME outcome set -- proving the threshold
/// comparison is `>=` (exit 0 exactly AT the threshold, not just strictly
/// above it), and that both directions (below -> nonzero, above -> zero)
/// work off the identical computed pass rate rather than any single case's
/// exact-match result (this ticket's `## Context`: gating must be
/// threshold-based, never exact-match, since an LLM-driven agent isn't
/// byte-for-byte deterministic).
#[tokio::test]
async fn report_json_exits_nonzero_when_pass_rate_below_configured_threshold_and_zero_when_at_or_above(
) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const MARKER: &str = "EvalReportThresholdReplyMarker4471";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("threshold-home");
    let xdg_config_home = unique_temp_dir("threshold-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("threshold-cases");
    write_case(
        &cases_dir,
        "passing-file-exists.json",
        serde_json::json!({
            "prompt": "say hi",
            "model": "gpt-4o-mini",
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
            "model": "gpt-4o-mini",
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
    // Exactly one of the two cases passed -- pass rate is exactly 0.5,
    // an exact fraction the three threshold comparisons below rely on.

    let report_at_threshold = rokr_eval::report::build_report(&outcomes, 0.5);
    assert_eq!(
        report_at_threshold.exit_code(),
        std::process::ExitCode::SUCCESS,
        "expected exit 0 when pass rate (0.5) is AT the configured threshold (0.5) -- the \
         comparison must be >=, not strict >"
    );

    let report_below_threshold = rokr_eval::report::build_report(&outcomes, 0.51);
    assert_eq!(
        report_below_threshold.exit_code(),
        std::process::ExitCode::FAILURE,
        "expected exit nonzero when pass rate (0.5) is BELOW the configured threshold (0.51)"
    );

    let report_above_threshold = rokr_eval::report::build_report(&outcomes, 0.1);
    assert_eq!(
        report_above_threshold.exit_code(),
        std::process::ExitCode::SUCCESS,
        "expected exit 0 when pass rate (0.5) is ABOVE the configured threshold (0.1)"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// Ticket 60 (eval-report-json-and-ci-gate) acceptance test: the aggregate
/// report's cost/tokens/turns must equal the sum of each case's own
/// headless result fields. Two cases with distinct, scripted mock
/// responses (matched by each case's own unique prompt text, mirroring
/// `case_with_scripted_judge_response_contributes_score_without_affecting_deterministic_pass_fail`'s
/// two-mock pattern above) each report a known, non-zero `usage`/`cost_usd`
/// -- `gpt-4o-mini` has a real non-zero entry in
/// `rokr_config::default_model_pricing`, so `cost_usd` is only non-zero if
/// the real pricing math actually ran (mirrors
/// `crates/rokr/tests/headless_test.rs::headless_json_result_cost_usd_matches_pricing_math_for_run_usage`'s
/// reasoning). Case A additionally scripts a non-zero `cached_tokens` so
/// the rollup's `total_tokens` sum (documented in `report.rs` to include
/// all four `UsageObject` fields) is proven to actually include cache
/// tokens, not just input/output.
#[tokio::test]
async fn report_json_rolls_up_cost_tokens_turns_from_each_cases_headless_result() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const PROMPT_A: &str = "case-a prompt EvalReportRollupMarkerA7231";
    const MARKER_A: &str = "EvalReportRollupReplyMarkerA7231";
    const PROMPT_B: &str = "case-b prompt EvalReportRollupMarkerB7231";
    const MARKER_B: &str = "EvalReportRollupReplyMarkerB7231";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(PROMPT_A))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER_A}}],
            "usage": {
                "prompt_tokens": 800000,
                "completion_tokens": 500000,
                "prompt_tokens_details": { "cached_tokens": 200000 }
            }
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(PROMPT_B))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER_B}}],
            "usage": {
                "prompt_tokens": 100000,
                "completion_tokens": 50000
            }
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("rollup-home");
    let xdg_config_home = unique_temp_dir("rollup-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("rollup-cases");
    write_case(
        &cases_dir,
        "case-a.json",
        serde_json::json!({
            "prompt": PROMPT_A,
            "model": "gpt-4o-mini",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [],
            "assertions": []
        }),
    );
    write_case(
        &cases_dir,
        "case-b.json",
        serde_json::json!({
            "prompt": PROMPT_B,
            "model": "gpt-4o-mini",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [],
            "assertions": []
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

    // The same gpt-4o-mini pricing rates
    // `headless_json_result_cost_usd_matches_pricing_math_for_run_usage`
    // duplicates from `rokr_config::default_model_pricing` (that function
    // is private to `rokr-config`, so both tests literal-duplicate its
    // per-token rates rather than calling it).
    const INPUT_RATE: f64 = 0.000_000_15;
    const OUTPUT_RATE: f64 = 0.000_000_6;
    const CACHE_READ_RATE: f64 = 0.000_000_075;

    let expected_cost_a =
        800_000.0 * INPUT_RATE + 500_000.0 * OUTPUT_RATE + 200_000.0 * CACHE_READ_RATE;
    let expected_cost_b = 100_000.0 * INPUT_RATE + 50_000.0 * OUTPUT_RATE;

    let outcome_a = outcomes
        .iter()
        .find(|outcome| outcome.name == "case-a")
        .expect("expected an outcome named case-a");
    let outcome_b = outcomes
        .iter()
        .find(|outcome| outcome.name == "case-b")
        .expect("expected an outcome named case-b");

    assert!(
        outcome_a.run_error.is_none(),
        "expected no run_error for case-a, got: {:?}",
        outcome_a.run_error
    );
    assert!(
        outcome_b.run_error.is_none(),
        "expected no run_error for case-b, got: {:?}",
        outcome_b.run_error
    );

    assert_eq!(outcome_a.usage.input_tokens, 800_000);
    assert_eq!(outcome_a.usage.output_tokens, 500_000);
    assert_eq!(outcome_a.usage.cache_read_tokens, 200_000);
    assert_eq!(outcome_a.usage.cache_write_tokens, 0);
    assert_eq!(outcome_a.num_turns, 1);
    assert!(
        (outcome_a.cost_usd - expected_cost_a).abs() < 1e-9,
        "expected case-a's cost_usd ({}) to match calculate_cost's own math ({expected_cost_a})",
        outcome_a.cost_usd
    );

    assert_eq!(outcome_b.usage.input_tokens, 100_000);
    assert_eq!(outcome_b.usage.output_tokens, 50_000);
    assert_eq!(outcome_b.usage.cache_read_tokens, 0);
    assert_eq!(outcome_b.usage.cache_write_tokens, 0);
    assert_eq!(outcome_b.num_turns, 1);
    assert!(
        (outcome_b.cost_usd - expected_cost_b).abs() < 1e-9,
        "expected case-b's cost_usd ({}) to match calculate_cost's own math ({expected_cost_b})",
        outcome_b.cost_usd
    );

    let report = rokr_eval::report::build_report(&outcomes, 0.0);

    assert_eq!(report.total_cases, 2);
    assert_eq!(
        report.total_turns, 2,
        "expected total_turns to equal the sum of each case's own num_turns (1 + 1)"
    );

    let expected_total_cost = expected_cost_a + expected_cost_b;
    assert!(
        (report.total_cost_usd - expected_total_cost).abs() < 1e-9,
        "expected report.total_cost_usd ({}) to equal the sum of each case's own cost_usd \
         ({expected_total_cost})",
        report.total_cost_usd
    );

    // case-a: 800_000 + 500_000 + 200_000 (cache) + 0 = 1_500_000
    // case-b: 100_000 + 50_000 + 0 + 0 = 150_000
    let expected_total_tokens: u64 = 1_500_000 + 150_000;
    assert_eq!(
        report.total_tokens, expected_total_tokens,
        "expected report.total_tokens to equal the sum of each case's own usage token fields \
         (including cache tokens)"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// F-002 (pre-ship review): a `setup_files` entry whose `path` tries to
/// escape the fixture dir (here, a leading `../`) must fail THAT case with
/// a clear `run_error` -- and, above all, must never actually write the
/// file outside the fixture dir it was supposed to be confined to. No mock
/// provider/env vars are configured here at all: path validation happens
/// before any headless turn is attempted, so an invalid case never reaches
/// that code at all.
#[tokio::test]
async fn eval_run_rejects_setup_file_path_that_escapes_fixture_dir() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let cases_dir = unique_temp_dir("traversal-cases");

    // Every `rokr-eval-` fixture dir is minted directly under the system
    // temp dir (`tempfile::Builder::new().prefix("rokr-eval-").tempdir()`
    // in `lib.rs`), so `"../<name>"` from inside one would land directly in
    // the system temp dir if the traversal weren't blocked.
    let escape_target =
        std::env::temp_dir().join("rokr-eval-traversal-escape-marker-8231.txt");
    let _ = std::fs::remove_file(&escape_target); // in case a prior failed run left it

    write_case(
        &cases_dir,
        "traversal-case.json",
        serde_json::json!({
            "prompt": "say hi",
            "model": "gpt-4o-mini",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [
                {"path": "../rokr-eval-traversal-escape-marker-8231.txt", "contents": "should never land here"}
            ],
            "assertions": []
        }),
    );

    let outcomes = rokr_eval::run_eval(&cases_dir, false)
        .await
        .expect("eval run should succeed at the whole-run level -- only this ONE case fails");
    assert_eq!(
        outcomes.len(),
        1,
        "expected exactly one case outcome, got: {outcomes:?}"
    );
    let outcome = &outcomes[0];

    assert!(
        !outcome.passed,
        "expected the case with an escaping setup_file path to fail, got: {outcome:?}"
    );
    let run_error = outcome
        .run_error
        .as_ref()
        .expect("expected a run_error reporting the path-traversal problem");
    assert!(
        run_error.to_lowercase().contains("..") || run_error.to_lowercase().contains("relative"),
        "expected run_error to clearly explain the rejected path, got: {run_error:?}"
    );

    assert!(
        !escape_target.exists(),
        "expected the setup file to NEVER be written outside the fixture dir, but found it at {}",
        escape_target.display()
    );

    let _ = std::fs::remove_file(&escape_target);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// F-008 (pre-ship review), items (2) and (3): an eval case run must not
/// leak into ambient operator state on either axis -- the operator's real
/// configured `UserPromptSubmit` hook must never fire during an eval case
/// (proven by the hook's own marker-file side effect never appearing), and
/// the case's session log must never land under the real/ambient data dir
/// (proven by no `sessions/` directory appearing under it) -- both checked
/// against the SAME single case run.
#[tokio::test]
async fn eval_run_case_never_fires_operators_hooks_or_writes_into_ambient_data_dir() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const MARKER: &str = "EvalAmbientIsolationReplyMarker2287";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("ambient-home");
    let xdg_config_home = unique_temp_dir("ambient-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    // A real `UserPromptSubmit` hook, configured exactly the way the
    // TUI/headless paths pick one up
    // (`rokr_config::load_or_init_default()` reads
    // `$XDG_CONFIG_HOME/rokr/rokr.json`) -- its command touches a marker
    // file. If it ever fired during an eval case, the marker would exist
    // after the run.
    let hook_marker = xdg_config_home.join("hook-fired-marker-4471.txt");
    let config_dir = xdg_config_home.join("rokr");
    std::fs::create_dir_all(&config_dir).expect("failed to create fake config dir");
    std::fs::write(
        config_dir.join("rokr.json"),
        serde_json::json!({
            "version": 1,
            "hooks": {
                "UserPromptSubmit": [{"command": format!("touch {}", hook_marker.display())}]
            }
        })
        .to_string(),
    )
    .expect("failed to write fake hooks config");

    let cases_dir = unique_temp_dir("ambient-cases");
    write_case(
        &cases_dir,
        "ambient-case.json",
        serde_json::json!({
            "prompt": "say hi",
            "model": "gpt-4o-mini",
            "agent": "plan",
            "permission_mode": "deny",
            "setup_files": [],
            "assertions": []
        }),
    );

    let outcomes = rokr_eval::run_eval(&cases_dir, false)
        .await
        .expect("eval run should succeed");
    assert_eq!(
        outcomes.len(),
        1,
        "expected exactly one case outcome, got: {outcomes:?}"
    );
    assert!(
        outcomes[0].run_error.is_none(),
        "expected no run_error, got: {:?}",
        outcomes[0].run_error
    );

    assert!(
        !hook_marker.exists(),
        "expected the operator's configured UserPromptSubmit hook to NEVER fire during an eval \
         case run, but its marker file was created at {}",
        hook_marker.display()
    );

    let ambient_sessions_dir = home.join(".local/share/rokr/sessions");
    assert!(
        !ambient_sessions_dir.exists(),
        "expected no session directory under the ambient/real data dir ({}) after an eval case \
         run -- finding one means the eval run leaked a session into ambient state",
        ambient_sessions_dir.display()
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}

/// F-015 (pre-ship review): a FAILING case's fixture dir must survive the
/// run (preserved on disk for debugging), with a `fixture_note` pointing at
/// it; a PASSING case's fixture dir must still be cleaned up exactly as
/// before this fix.
#[tokio::test]
async fn eval_run_preserves_failing_cases_fixture_dir_and_cleans_up_passing_cases() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    const MARKER: &str = "EvalFixturePreservationReplyMarker6612";

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": MARKER}}]
        })))
        .mount(&mock)
        .await;

    let home = unique_temp_dir("fixture-preserve-home");
    let xdg_config_home = unique_temp_dir("fixture-preserve-xdg-config-home");
    // Safety: serialized against every other test in this file via
    // `ENV_LOCK` (see this file's module doc comment).
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config_home);
        std::env::set_var("ROKR_OPENAI_BASE_URL", mock.uri());
        std::env::set_var("ROKR_OPENAI_MODEL", "gpt-4o-mini");
        std::env::set_var("ROKR_OPENAI_API_KEY", "test-key");
    }

    let cases_dir = unique_temp_dir("fixture-preserve-cases");
    write_case(
        &cases_dir,
        "passing-file-exists.json",
        serde_json::json!({
            "prompt": "say hi",
            "model": "gpt-4o-mini",
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
            "model": "gpt-4o-mini",
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

    assert!(
        !passing.fixture_dir.exists(),
        "expected the PASSING case's fixture dir to be cleaned up after the run, but it still \
         exists at {}",
        passing.fixture_dir.display()
    );
    assert!(
        passing.fixture_note.is_none(),
        "expected no fixture_note for a passing case, got: {:?}",
        passing.fixture_note
    );

    assert!(
        failing.fixture_dir.exists(),
        "expected the FAILING case's fixture dir to be PRESERVED after the run for debugging, \
         but it was deleted (path: {})",
        failing.fixture_dir.display()
    );
    let note = failing.fixture_note.as_ref().expect(
        "expected a fixture_note pointing at the preserved fixture dir for the failing case",
    );
    assert!(
        note.contains(&failing.fixture_dir.display().to_string()),
        "expected the fixture_note to mention the preserved fixture path, got: {note:?}"
    );

    let _ = std::fs::remove_dir_all(&failing.fixture_dir);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&xdg_config_home);
    let _ = std::fs::remove_dir_all(&cases_dir);
}
