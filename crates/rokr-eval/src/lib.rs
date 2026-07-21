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
    pub run_error: Option<String>,
}

/// Runs every case file under `cases_dir` (see `case::load_cases`) and
/// reports pass/fail per case. Each case gets its own fresh temp fixture
/// dir (created here, never reused across cases) and a fresh headless
/// session (`rokr_app::headless::run_result_object` never resumes -- see
/// that function's doc comment), with an explicit agent tier/permission
/// mode read straight off the case file rather than any ambient `Cli` --
/// "no case inherits ambient config" per the ticket's `## Context`.
///
/// "Pinned model" (the acceptance line): the provider/model is resolved
/// from the ambient env (`ROKR_OPENAI_MODEL`/`ROKR_ANTHROPIC_MODEL` etc.,
/// same vars `rokr_app::headless::run` reads) fresh on every case's call
/// into `run_result_object` -- but since nothing in this loop ever mutates
/// those env vars mid-run, every case resolves to the identical
/// provider/model, i.e. pinned for the run's whole duration. There is no
/// per-case `model` field in the case schema (deliberately -- see
/// `case::Case`'s doc comment).
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

    let mut outcomes = Vec::with_capacity(loaded_cases.len());
    for loaded_case in loaded_cases {
        let fixture_dir = tempfile::Builder::new()
            .prefix("rokr-eval-")
            .tempdir()
            .map_err(|err| format!("failed to create fixture dir: {err}"))?;

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
            std::fs::write(&target, &setup_file.contents)
                .map_err(|err| format!("failed to write setup file {}: {err}", setup_file.path))?;
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
        let run_error = if case_requests_bypass && !dangerously_skip_permissions {
            Some(
                "case requests bypass but --dangerously-skip-permissions was not passed"
                    .to_string(),
            )
        } else {
            // `dangerously_skip_permissions` is only ever forwarded as
            // `true` for a case that is actually `Bypass` mode -- a
            // `Deny`/`AcceptEdits` case never needs it, regardless of
            // whether the operator passed the run-level flag.
            let run_result = rokr_app::headless::run_result_object(
                agent,
                permission_mode,
                case_requests_bypass,
                loaded_case.case.prompt.clone(),
                Some(fixture_dir.path().to_path_buf()),
            )
            .await;

            match &run_result {
                Ok(_) => None,
                Err(rokr_app::headless::BootstrapError::CliMisuse(err)) => Some(err.clone()),
                Err(rokr_app::headless::BootstrapError::Other(err)) => Some(err.clone()),
            }
        };

        let assertion_outcomes: Vec<assertions::AssertionOutcome> = loaded_case
            .case
            .assertions
            .iter()
            .map(|assertion| assertions::check_assertion(fixture_dir.path(), assertion))
            .collect();

        let passed = run_error.is_none() && assertion_outcomes.iter().all(|o| o.passed);

        outcomes.push(CaseOutcome {
            name: loaded_case.name,
            fixture_dir: fixture_dir.path().to_path_buf(),
            passed,
            assertion_outcomes,
            run_error,
        });
        // `fixture_dir` (a `tempfile::TempDir`) drops here, at the end of
        // this case's iteration -- deleting the fresh fixture dir from disk
        // now that every assertion against it has already run. The next
        // case's `tempfile::Builder::tempdir()` call above always mints a
        // brand-new, uniquely-named directory, so there is never any
        // reuse/leakage between cases even though each one's dir is
        // short-lived.
    }

    Ok(outcomes)
}
