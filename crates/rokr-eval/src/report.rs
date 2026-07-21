//! Ticket 60 (eval-report-json-and-ci-gate): the aggregate JSON report and
//! threshold-based CI gate behind `rokr eval --report json --pass-threshold
//! <N>`. Aggregate pass rate is computed from [`CaseOutcome::passed`]
//! (deterministic assertions only, per that field's own doc comment) --
//! judge scores are folded in separately as a reported mean
//! ([`crate::mean_judge_score`]), NEVER as a pass/fail input, matching this
//! ticket's `## Context`: an LLM-driven agent isn't byte-for-byte
//! deterministic even at low temperature, so gating must be threshold-
//! based, never exact-match on a single run. Cost/tokens/turns are rolled
//! up by summing each case's own [`CaseOutcome`] fields (ticket 55's
//! headless `ResultObject`, threaded through per-case in `lib.rs`'s
//! `run_eval`).

use crate::CaseOutcome;

/// `count(passed) / count(total)` across `outcomes`, reflecting
/// [`CaseOutcome::passed`] ONLY -- unaffected by `judge_scores` attached to
/// those outcomes (a judge score is a tracked metric, never a pass/fail
/// gate; see `crate::judge`'s doc comment). An empty slice returns `1.0`
/// (a vacuous pass), avoiding a divide-by-zero `NaN` for an empty cases
/// dir.
pub fn aggregate_pass_rate(outcomes: &[CaseOutcome]) -> f64 {
    if outcomes.is_empty() {
        return 1.0;
    }
    let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
    passed as f64 / outcomes.len() as f64
}

/// The full `--report json` payload: aggregate pass rate against the
/// configured threshold, the optional mean judge score (reported, never
/// gating -- see `aggregate_pass_rate`'s doc comment), and cost/tokens/
/// turns summed from every case's own headless result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub total_cases: usize,
    pub passed_cases: usize,
    pub pass_rate: f64,
    pub pass_threshold: f64,
    pub mean_judge_score: Option<f64>,
    pub total_cost_usd: f64,
    pub total_tokens: u64,
    pub total_turns: u32,
}

impl Report {
    /// `SUCCESS` iff `pass_rate >= pass_threshold` -- a threshold
    /// comparison, never exact-match on any single run (this ticket's
    /// `## Context`: CI stability requires a `>=` band, not a single
    /// expected number, since an LLM-driven agent isn't byte-for-byte
    /// deterministic even at low temperature).
    pub fn exit_code(&self) -> std::process::ExitCode {
        if self.pass_rate >= self.pass_threshold {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        }
    }
}

/// Builds the aggregate [`Report`] for `outcomes` against `pass_threshold`.
pub fn build_report(outcomes: &[CaseOutcome], pass_threshold: f64) -> Report {
    let total_cases = outcomes.len();
    let passed_cases = outcomes.iter().filter(|outcome| outcome.passed).count();
    let pass_rate = aggregate_pass_rate(outcomes);
    let mean_judge_score = crate::mean_judge_score(outcomes);
    let total_cost_usd = outcomes.iter().map(|outcome| outcome.cost_usd).sum();
    // `total_tokens` sums all four `UsageObject` fields per case --
    // `input_tokens`/`output_tokens`/`cache_read_tokens`/`cache_write_tokens`
    // are four DISTINCT token counts, not overlapping subsets of one
    // another (see `rokr_app::result_schema::UsageObject`'s own doc
    // comment), and `rokr_core::pricing::calculate_cost` prices all four
    // independently -- so excluding the cache fields here would undercount
    // the real token volume actually processed/billed by the provider for
    // a case that hit cache.
    let total_tokens: u64 = outcomes
        .iter()
        .map(|outcome| {
            outcome.usage.input_tokens
                + outcome.usage.output_tokens
                + outcome.usage.cache_read_tokens
                + outcome.usage.cache_write_tokens
        })
        .sum();
    let total_turns: u32 = outcomes.iter().map(|outcome| outcome.num_turns).sum();

    Report {
        total_cases,
        passed_cases,
        pass_rate,
        pass_threshold,
        mean_judge_score,
        total_cost_usd,
        total_tokens,
        total_turns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertions::AssertionOutcome;
    use crate::judge::JudgeScore;
    use rokr_app::result_schema::UsageObject;

    /// Builds a minimal `CaseOutcome` for the pass-rate test below: `passed`
    /// drives the single deterministic assertion's own outcome too (so the
    /// fixture is internally consistent), and `judge_score` optionally
    /// attaches one judge score -- deliberately set OPPOSITE to `passed`
    /// (high score on a failed case, low score on a passed case) so that if
    /// `aggregate_pass_rate` ever wrongly folded judge scores into the
    /// calculation, the resulting number would visibly shift.
    fn outcome(name: &str, passed: bool, judge_score: Option<f64>) -> CaseOutcome {
        CaseOutcome {
            name: name.to_string(),
            fixture_dir: std::path::PathBuf::from("/tmp/rokr-eval-report-test"),
            passed,
            assertion_outcomes: vec![AssertionOutcome {
                description: "file_exists(marker.txt)".to_string(),
                passed,
                detail: String::new(),
            }],
            judge_scores: judge_score
                .map(|score| {
                    vec![JudgeScore {
                        description: "rubric".to_string(),
                        score,
                        detail: String::new(),
                    }]
                })
                .unwrap_or_default(),
            run_error: None,
            cost_usd: 0.0,
            num_turns: 1,
            usage: UsageObject::default(),
        }
    }

    /// The named failing test for ticket 60: `aggregate_pass_rate` must
    /// reflect `CaseOutcome::passed` alone. Three outcomes (2 passed, 1
    /// failed) give an exact, non-degenerate fraction (2/3) to assert
    /// against; attaching judge scores that would push the number in the
    /// OPPOSITE direction if wrongly folded in (high score on the failed
    /// case, low scores on the passed ones) -- and then reversing which
    /// score is attached to which outcome, and then removing judge scores
    /// entirely -- must never change the computed rate.
    #[test]
    fn aggregate_pass_rate_computed_from_deterministic_outcomes_only_excluding_judge_scores() {
        let outcomes = vec![
            outcome("case-1", true, Some(0.1)),
            outcome("case-2", true, Some(0.2)),
            outcome("case-3", false, Some(0.9)),
        ];
        let rate = aggregate_pass_rate(&outcomes);
        assert!(
            (rate - (2.0 / 3.0)).abs() < f64::EPSILON,
            "expected pass rate 2/3 from deterministic outcomes alone, got {rate}"
        );

        // Reverse which judge score is attached to which outcome -- if
        // judge scores influenced the calculation at all, this would move
        // the number; it must not.
        let outcomes_reversed_scores = vec![
            outcome("case-1", true, Some(0.9)),
            outcome("case-2", true, Some(0.9)),
            outcome("case-3", false, Some(0.1)),
        ];
        let rate_reversed = aggregate_pass_rate(&outcomes_reversed_scores);
        assert!(
            (rate_reversed - rate).abs() < f64::EPSILON,
            "expected pass rate to be unaffected by which judge scores are attached, got \
             {rate_reversed} vs {rate}"
        );

        // No judge scores recorded at all (no case had a judge-rubric
        // assertion) must produce the identical rate too.
        let outcomes_no_scores = vec![
            outcome("case-1", true, None),
            outcome("case-2", true, None),
            outcome("case-3", false, None),
        ];
        let rate_no_scores = aggregate_pass_rate(&outcomes_no_scores);
        assert!(
            (rate_no_scores - rate).abs() < f64::EPSILON,
            "expected pass rate to be unaffected by the absence of judge scores, got \
             {rate_no_scores} vs {rate}"
        );

        // An empty slice is a vacuous pass (avoids divide-by-zero).
        assert_eq!(aggregate_pass_rate(&[]), 1.0);
    }
}
