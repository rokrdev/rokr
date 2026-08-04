//! Ticket 58 (eval-case-runner-and-deterministic-assertions): `rokr eval
//! <cases-dir>` reads a directory of eval case files (JSON: a prompt, setup
//! fixtures, an agent tier, a permission mode, a list of assertions -- see
//! [`case`]), and for each case runs a fresh, isolated headless agent turn
//! (fresh temp fixture dir, fresh session, pinned model/permission mode --
//! no case inherits ambient config or another case's state) then checks the
//! case's deterministic assertions ([`assertions`]) against the resulting
//! fixture directory. Depends only on `rokr-app` (not the reverse), keeping
//! eval-only dependencies off the shipped `rokr` binary's path.

pub mod assertions;
pub mod case;
pub mod judge;
pub mod report;

use std::path::PathBuf;

/// One case's full report: whether it passed overall, its fresh fixture
/// dir (proof of isolation -- see `tests/eval_test.rs`), the per-assertion
/// results, and the headless turn's own bootstrap error, if any (a
/// bootstrap failure -- e.g. no reachable provider -- is reported here
/// rather than aborting the whole run, so one broken case doesn't hide
/// every other case's result).
#[derive(Debug)]
pub struct CaseOutcome {
    pub name: String,
    pub fixture_dir: PathBuf,
    pub passed: bool,
    pub assertion_outcomes: Vec<assertions::AssertionOutcome>,
    /// Ticket 59: LLM-judge rubric scores for this case, kept entirely
    /// separate from `assertion_outcomes`/`passed` -- see `judge`'s doc
    /// comment for why a judge score is a tracked metric, never a pass/fail
    /// gate.
    pub judge_scores: Vec<judge::JudgeScore>,
    pub run_error: Option<String>,
    /// Ticket 60 (eval-report-json-and-ci-gate): this case's own headless
    /// result's `cost_usd`/`num_turns`/`usage` (ticket 55's `ResultObject`
    /// fields), threaded straight through so `report::build_report` can sum
    /// them across every case without re-deriving anything. Defaults to
    /// `0.0`/`0`/`UsageObject::default()` on any `run_error` path (bypass
    /// denied without the operator flag, or a `BootstrapError`) -- there is
    /// no headless run to read these from in that case.
    pub cost_usd: f64,
    pub num_turns: u32,
    pub usage: rokr_app::result_schema::UsageObject,
    /// F-015 (pre-ship review): set only for a FAILING case whose fixture
    /// dir was preserved on disk (rather than deleted via the normal
    /// `TempDir` drop) for post-mortem debugging -- `Some("fixture
    /// preserved at <path>")`. `None` for a passing case, which cleans up
    /// normally.
    pub fixture_note: Option<String>,
}

/// Runs every case file under `cases_dir` (see `case::load_cases`) and
/// reports pass/fail per case. Each case gets its own fresh temp fixture
/// dir (created here, never reused across cases) and a fresh headless
/// session (`rokr_app::headless::run_result_object` never resumes -- see
/// that function's doc comment), with an explicit agent tier/permission
/// mode read straight off the case file rather than any ambient `Cli` --
/// "no case inherits ambient config" per the ticket's `## Context`.
///
/// "Pinned model" (the acceptance line): F-008 (pre-ship review) made this
/// literal -- every case file carries its OWN required `model` field (see
/// `case::Case`'s doc comment), threaded into
/// `rokr_app::headless::run_result_object` via a
/// `rokr_app::headless::HeadlessRunOverride` built fresh per case below.
/// That same override also (a) suppresses the operator's real configured
/// hooks (an empty `hooks_config`) and (b) redirects this case's session
/// log to a fresh per-case temp dir instead of the real user data
/// directory -- an eval case must never inherit ambient state on any of
/// these three axes.
///
/// Returns `Err` only for a whole-run failure (the cases dir itself can't
/// be read, or a case file fails to parse) -- an individual case's own
/// run/assertion failure is instead captured in that case's `CaseOutcome`.
///
/// `dangerously_skip_permissions` is the OPERATOR's own `--dangerously-
/// skip-permissions` flag on this `rokr eval` invocation (see
/// `rokr_app::cli::Command::Eval`'s field of the same name) -- NOT derived
/// from any case file. A case file requesting `permission_mode: bypass`
/// (`case::CasePermissionMode::Bypass`) is untrusted data and must never be
/// able to grant itself the equivalent of `--dangerously-skip-permissions`
/// on its own (mirrors `rokr_app::headless::build_permission_requester`'s
/// existing precedent for the headless `-p` path). When this parameter is
/// `false`, a bypass-requesting case never reaches
/// `rokr_app::headless::run_result_object` at all -- it's reported as a
/// failing `CaseOutcome` with a `run_error` explaining the missing flag.
/// `Deny`/`AcceptEdits` cases are unaffected either way.
pub async fn run_eval(
    cases_dir: &std::path::Path,
    dangerously_skip_permissions: bool,
) -> Result<Vec<CaseOutcome>, String> {
    let loaded_cases = case::load_cases(cases_dir)?;

    // F-003 (pre-ship review): zero cases loaded (an existing but empty
    // dir, or a dir with no `*.json` files) must be a whole-run FAILURE,
    // not a vacuous success -- `report::aggregate_pass_rate(&[])` returns
    // `1.0` (a deliberate vacuous-pass for the THRESHOLD MATH itself, see
    // that fn's own doc comment), which would otherwise let a CI gate that
    // silently found zero cases exit 0 as if everything passed. Returning
    // `Err` here (rather than `Ok(vec![])`) is the single seam both of
    // `main.rs`'s `--report json` and text output paths already share --
    // both route a `run_eval` `Err` through the exact same
    // `eprintln!("eval failed: {err}"); ExitCode::FAILURE` arm, so fixing
    // it here covers both without duplicating the check.
    if loaded_cases.is_empty() {
        return Err(format!("no eval cases found under {}", cases_dir.display()));
    }

    let mut outcomes = Vec::with_capacity(loaded_cases.len());
    for loaded_case in loaded_cases {
        let fixture_dir = tempfile::Builder::new()
            .prefix("rokr-eval-")
            .tempdir()
            .map_err(|err| format!("failed to create fixture dir: {err}"))?;
        // F-008: a fresh per-case temp dir standing in for the real user
        // data directory -- this case's session log lands here, never
        // under the operator's real `~/.local/share/rokr`. Always cleaned
        // up at the end of this iteration (unlike `fixture_dir`, which is
        // deliberately preserved on a FAILING case -- see F-015 below; a
        // case's session log isn't debugging evidence the same way its
        // fixture dir's file contents are).
        let case_data_dir = tempfile::Builder::new()
            .prefix("rokr-eval-data-")
            .tempdir()
            .map_err(|err| format!("failed to create per-case data dir: {err}"))?;

        // F-002 (pre-ship review): a `setup_file.path` containing a `..`
        // component, an absolute path, or a Windows drive prefix could
        // escape `fixture_dir` entirely and write anywhere on disk once
        // joined with `fixture_dir.path()`. Validated BEFORE any write --
        // the first invalid path fails this CASE (a clear `run_error`,
        // parallel to the bypass-without-flag failure below), not the
        // whole run, and nothing from this case's `setup_files` is ever
        // written once one is found invalid.
        let mut setup_error: Option<String> = None;
        for setup_file in &loaded_case.case.setup_files {
            if let Err(reason) = validate_setup_file_path(&setup_file.path) {
                setup_error = Some(format!(
                    "refusing to write setup file {:?}: {reason}",
                    setup_file.path
                ));
                break;
            }
        }
        if setup_error.is_none() {
            for setup_file in &loaded_case.case.setup_files {
                let target = fixture_dir.path().join(&setup_file.path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        format!(
                            "failed to create parent dirs for setup file {}: {err}",
                            setup_file.path
                        )
                    })?;
                }
                std::fs::write(&target, &setup_file.contents).map_err(|err| {
                    format!("failed to write setup file {}: {err}", setup_file.path)
                })?;
            }
        }

        let agent = loaded_case.case.agent.to_agent_tier();
        let permission_mode = loaded_case.case.permission_mode.to_permission_mode();
        let case_requests_bypass = matches!(permission_mode, rokr_app::PermissionMode::Bypass);

        // Security fix (team-lead review, follow-up to ticket 58): `Bypass`
        // requires the explicit `--dangerously-skip-permissions` flag on
        // the OPERATOR's own `rokr eval` invocation (this fn's
        // `dangerously_skip_permissions` parameter) -- mirroring
        // `rokr_app::headless::build_permission_requester`'s existing
        // precedent for the headless `-p` path. A case file is untrusted
        // data (just a JSON file in a directory that could come from
        // anywhere, e.g. a cloned repo) and must never be able to grant
        // itself the equivalent of `--dangerously-skip-permissions` on its
        // own. A bypass-requesting case with no operator flag never reaches
        // `run_result_object` at all -- it fails immediately with a clear
        // `run_error` instead.
        let (run_error, headless_result_text, cost_usd, num_turns, usage): (
            Option<String>,
            Option<String>,
            f64,
            u32,
            rokr_app::result_schema::UsageObject,
        ) = if let Some(setup_error) = setup_error {
            (
                Some(setup_error),
                None,
                0.0,
                0,
                rokr_app::result_schema::UsageObject::default(),
            )
        } else if case_requests_bypass && !dangerously_skip_permissions {
            (
                Some(
                    "case requests bypass but --dangerously-skip-permissions was not passed"
                        .to_string(),
                ),
                None,
                0.0,
                0,
                rokr_app::result_schema::UsageObject::default(),
            )
        } else {
            // `dangerously_skip_permissions` is only ever forwarded as
            // `true` for a case that is actually `Bypass` mode -- a
            // `Deny`/`AcceptEdits` case never needs it, regardless of
            // whether the operator passed the run-level flag.
            //
            // F-008: every case gets its own `HeadlessRunOverride` --
            // model/provider pinned from the case file, hooks entirely
            // suppressed (empty map), session log redirected to
            // `case_data_dir`.
            let run_override = rokr_app::headless::HeadlessRunOverride {
                model: loaded_case.case.model.clone(),
                provider: loaded_case.case.provider.clone(),
                hooks_config: std::collections::HashMap::new(),
                data_dir: case_data_dir.path().to_path_buf(),
            };
            let run_result = rokr_app::headless::run_result_object(
                agent,
                permission_mode,
                case_requests_bypass,
                // ADR 0018 decision 4's `--allow-skill` is an
                // operator-level CI flag on the `rokr eval` invocation
                // itself, not a per-case concept -- an eval case file has
                // no field for it, so this is always empty.
                &[],
                loaded_case.case.prompt.clone(),
                Some(fixture_dir.path().to_path_buf()),
                Some(run_override),
            )
            .await;

            match run_result {
                // Ticket 59: the final result text is now captured
                // (rather than discarded, as before this ticket) as the
                // "transcript" a judge-rubric assertion scores against
                // -- see `judge`'s doc comment for why this is the
                // final headless result text specifically, not the
                // full per-turn message list.
                //
                // Ticket 60: cost_usd/num_turns/usage are threaded
                // through from the SAME `ResultObject` here too, rather
                // than being discarded as before this ticket.
                Ok(outcome) => (
                    None,
                    Some(outcome.result_object.result),
                    outcome.result_object.cost_usd,
                    outcome.result_object.num_turns,
                    outcome.result_object.usage,
                ),
                Err(rokr_app::headless::BootstrapError::CliMisuse(err)) => (
                    Some(err),
                    None,
                    0.0,
                    0,
                    rokr_app::result_schema::UsageObject::default(),
                ),
                Err(rokr_app::headless::BootstrapError::Other(err)) => (
                    Some(err),
                    None,
                    0.0,
                    0,
                    rokr_app::result_schema::UsageObject::default(),
                ),
            }
        };

        // Ticket 59: a judge-rubric assertion is routed to
        // `judge::score_rubric` instead of `assertions::check_assertion` --
        // it contributes a `judge::JudgeScore` to `judge_scores`, entirely
        // separate from `assertion_outcomes`/`passed`, which stay
        // deterministic-assertions-only exactly as before this ticket.
        let mut assertion_outcomes: Vec<assertions::AssertionOutcome> = Vec::new();
        let mut judge_scores: Vec<judge::JudgeScore> = Vec::new();
        for assertion in &loaded_case.case.assertions {
            match assertion {
                case::Assertion::JudgeRubric { rubric } => {
                    let transcript = headless_result_text.clone().unwrap_or_default();
                    // A judge-call failure (missing env var, transport
                    // error, unparseable verdict) must never flip the
                    // case's own deterministic pass/fail outcome -- a
                    // judge score is a tracked metric, not a gate (mirrors
                    // `session_handle`'s "degrade gracefully" precedent in
                    // `rokr_app::headless::run_result_object`). F-009
                    // (pre-ship review): but silently dropping the failure
                    // entirely hid real judge-infra problems (bad rubric
                    // wiring, expired judge credentials, etc) from anyone
                    // running the suite -- surfaced to stderr instead,
                    // naming the case, the rubric, and the underlying
                    // error, WITHOUT touching `assertion_outcomes`/`passed`.
                    match judge::score_rubric(&transcript, rubric).await {
                        Ok(score) => judge_scores.push(score),
                        Err(err) => {
                            eprintln!(
                                "eval case {:?}: judge-rubric assertion failed (rubric: {rubric:?}): {err}",
                                loaded_case.name
                            );
                        }
                    }
                }
                other => {
                    assertion_outcomes.push(assertions::check_assertion(fixture_dir.path(), other))
                }
            }
        }

        let passed = run_error.is_none() && assertion_outcomes.iter().all(|o| o.passed);

        // F-015 (pre-ship review): a FAILING case's fixture dir is
        // debugging evidence -- deleting it via the normal `TempDir` drop
        // (below) before the report is even written destroys exactly the
        // state someone would want to inspect. `TempDir::keep` (this
        // workspace pins `tempfile = "3.27"`, which has it) consumes the
        // `TempDir` WITHOUT deleting it, returning the bare `PathBuf`; a
        // PASSING case takes no action here, so its `fixture_dir` still
        // drops (and deletes) normally at the end of this iteration,
        // exactly as before this fix.
        let fixture_path = fixture_dir.path().to_path_buf();
        let fixture_note = if passed {
            None
        } else {
            let kept_path = fixture_dir.keep();
            Some(format!("fixture preserved at {}", kept_path.display()))
        };

        outcomes.push(CaseOutcome {
            name: loaded_case.name,
            fixture_dir: fixture_path,
            passed,
            assertion_outcomes,
            judge_scores,
            run_error,
            cost_usd,
            num_turns,
            usage,
            fixture_note,
        });
        // `case_data_dir` (a `tempfile::TempDir`) always drops here,
        // regardless of `passed` -- an eval case's session log is never
        // debugging evidence in the way its fixture dir's file contents
        // are (see the F-015 comment above), so it's cleaned up
        // unconditionally, mirroring `fixture_dir`'s own pre-F-015
        // behavior.
        //
        // `fixture_dir` itself only reaches here (and only then drops,
        // deleting it) for a PASSING case -- a failing case already moved
        // it out via `.keep()` above. The next case's
        // `tempfile::Builder::tempdir()` calls above always mint
        // brand-new, uniquely-named directories, so there is never any
        // reuse/leakage between cases either way.
    }

    Ok(outcomes)
}

/// F-002 (pre-ship review): rejects any `setup_file.path` that could escape
/// `fixture_dir` once joined onto it -- an absolute path (`Component::
/// RootDir`/`Component::Prefix`, the latter Windows-only e.g. `C:\`) or any
/// `..` component (`Component::ParentDir`) anywhere in the path, not just
/// a leading one (`foo/../../bar` is just as much an escape as `../bar`).
/// `Component::CurDir` (`./`) and `Component::Normal` (an ordinary segment)
/// are always fine.
fn validate_setup_file_path(path: &str) -> Result<(), String> {
    let candidate = std::path::Path::new(path);
    for component in candidate.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err("path must not contain a '..' component".to_string());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err("path must be relative to the fixture dir, not absolute".to_string());
            }
            std::path::Component::CurDir | std::path::Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Ticket 59: the mean of every judge score recorded across `outcomes`
/// (flattened over each case's own `judge_scores`) -- today's stand-in for
/// the aggregate report's `mean-judge-score` field ahead of ticket 60
/// (`eval-report-json-and-ci-gate`), which formalizes a full `report.rs`
/// aggregate on top of this. Returns `None` when no case in `outcomes`
/// recorded any judge score at all (avoids a divide-by-zero `NaN`).
pub fn mean_judge_score(outcomes: &[CaseOutcome]) -> Option<f64> {
    let scores: Vec<f64> = outcomes
        .iter()
        .flat_map(|outcome| outcome.judge_scores.iter().map(|score| score.score))
        .collect();
    if scores.is_empty() {
        return None;
    }
    Some(scores.iter().sum::<f64>() / scores.len() as f64)
}
