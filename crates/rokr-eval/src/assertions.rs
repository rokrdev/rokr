//! Ticket 58 (eval-case-runner-and-deterministic-assertions): deterministic
//! assertion classes checked against an eval case's fixture directory after
//! its headless agent turn completes -- file exists, file contains a
//! pattern, a git diff matches, a command exits with a given code. Each
//! check is a hard pass/fail, never a partial/fuzzy score (that's the LLM-
//! judge scoring slice, out of this ticket's scope).

use std::path::Path;

/// The outcome of checking one assertion against a case's fixture
/// directory: a hard pass/fail plus a human-readable `detail` explaining
/// why, surfaced by `rokr eval`'s per-case report.
#[derive(Debug, Clone)]
pub struct AssertionOutcome {
    /// A short, stable label identifying which assertion this is (e.g.
    /// `file_exists(foo.txt)`), used in reports.
    pub description: String,
    pub passed: bool,
    pub detail: String,
}

/// Passes when `relative_path` (resolved against `fixture_dir`) exists.
pub fn check_file_exists(fixture_dir: &Path, relative_path: &str) -> AssertionOutcome {
    let target = fixture_dir.join(relative_path);
    let passed = target.exists();
    AssertionOutcome {
        description: format!("file_exists({relative_path})"),
        detail: if passed {
            format!("found {}", target.display())
        } else {
            format!("not found: {}", target.display())
        },
        passed,
    }
}

/// Passes when `git -C fixture_dir diff`'s stdout (the fixture dir's
/// uncommitted working-tree changes against its own HEAD) matches
/// `expected_diff` exactly, modulo leading/trailing whitespace (a case
/// file's expected diff is typically hand-authored/copy-pasted text, which
/// commonly picks up a stray trailing newline).
pub fn check_git_diff(fixture_dir: &Path, expected_diff: &str) -> AssertionOutcome {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(fixture_dir)
        .arg("diff")
        .output();
    match output {
        Ok(output) => {
            let actual = String::from_utf8_lossy(&output.stdout);
            let passed = actual.trim() == expected_diff.trim();
            AssertionOutcome {
                description: "git_diff".to_string(),
                detail: if passed {
                    "diff matches expected".to_string()
                } else {
                    format!("diff mismatch; actual diff was:\n{actual}")
                },
                passed,
            }
        }
        Err(err) => AssertionOutcome {
            description: "git_diff".to_string(),
            passed: false,
            detail: format!(
                "failed to run `git diff` in {}: {err}",
                fixture_dir.display()
            ),
        },
    }
}

/// Passes when running `command args...` with `fixture_dir` as its working
/// directory exits with exactly `expected_code`.
pub fn check_command_exit(
    fixture_dir: &Path,
    command: &str,
    args: &[String],
    expected_code: i32,
) -> AssertionOutcome {
    let output = std::process::Command::new(command)
        .args(args)
        .current_dir(fixture_dir)
        .output();
    match output {
        Ok(output) => {
            // A process killed by a signal has no exit code (`None`); `-1`
            // can never equal a real exit code, so that case always fails
            // rather than panicking on the `unwrap`.
            let actual_code = output.status.code().unwrap_or(-1);
            let passed = actual_code == expected_code;
            AssertionOutcome {
                description: format!("command_exit({command})"),
                detail: format!("expected exit code {expected_code}, got {actual_code}"),
                passed,
            }
        }
        Err(err) => AssertionOutcome {
            description: format!("command_exit({command})"),
            passed: false,
            detail: format!(
                "failed to run `{command}` in {}: {err}",
                fixture_dir.display()
            ),
        },
    }
}

/// Passes when `relative_path` (resolved against `fixture_dir`) exists and
/// its contents contain `pattern` as a literal substring. Named in the
/// ticket's `## Context` prose as part of the PRD's assertion set, but has
/// no dedicated named failing test in this ticket -- implemented here for
/// completeness since it's cheap and shares `check_file_exists`'s shape.
pub fn check_file_contains(
    fixture_dir: &Path,
    relative_path: &str,
    pattern: &str,
) -> AssertionOutcome {
    let target = fixture_dir.join(relative_path);
    match std::fs::read_to_string(&target) {
        Ok(contents) => {
            let passed = contents.contains(pattern);
            AssertionOutcome {
                description: format!("file_contains({relative_path}, {pattern:?})"),
                detail: if passed {
                    "pattern found".to_string()
                } else {
                    format!("pattern not found in {}", target.display())
                },
                passed,
            }
        }
        Err(err) => AssertionOutcome {
            description: format!("file_contains({relative_path}, {pattern:?})"),
            passed: false,
            detail: format!("failed to read {}: {err}", target.display()),
        },
    }
}

/// Dispatches one [`case::Assertion`] to its matching `check_*` fn above --
/// the single seam `lib.rs`'s per-case loop calls for every assertion in a
/// case's list.
pub fn check_assertion(fixture_dir: &Path, assertion: &crate::case::Assertion) -> AssertionOutcome {
    match assertion {
        crate::case::Assertion::FileExists { path } => check_file_exists(fixture_dir, path),
        crate::case::Assertion::FileContains { path, pattern } => {
            check_file_contains(fixture_dir, path, pattern)
        }
        crate::case::Assertion::GitDiff { expected } => check_git_diff(fixture_dir, expected),
        crate::case::Assertion::CommandExit {
            command,
            args,
            expected_code,
        } => check_command_exit(fixture_dir, command, args, *expected_code),
        // F-010 (pre-ship review): `lib.rs`'s per-case loop routes a
        // judge-rubric assertion to `judge::score_rubric` instead of here,
        // but `check_assertion` is a `pub fn` -- nothing in the type system
        // stops ANY caller (a future dispatcher, a direct unit test, a
        // caller in another crate) from handing it a `JudgeRubric` variant
        // anyway. This arm used to be `unreachable!()`, which would panic
        // in production the moment that assumption broke. A judge rubric
        // has no deterministic pass/fail contract of its own (see `judge`'s
        // doc comment: it's a scored metric, never a gate), so the only
        // honest answer `check_assertion` can give here is "failed by
        // construction" with an explanation -- never a panic.
        crate::case::Assertion::JudgeRubric { rubric } => AssertionOutcome {
            description: format!("judge_rubric({rubric:?})"),
            passed: false,
            detail: "judge-rubric assertions are scored via judge::score_rubric and routed \
                     there by lib.rs's per-case loop before this function is ever called; \
                     check_assertion has no way to score a rubric itself, so this is reported \
                     as failed-by-construction rather than panicking"
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `check_file_exists` must pass when the named file is present under
    /// the fixture dir, and fail when it is absent -- the simplest
    /// deterministic assertion class, and this ticket's first named failing
    /// test.
    #[test]
    fn file_exists_assertion_passes_when_file_present_and_fails_when_absent() {
        let dir = tempfile::tempdir().expect("failed to create temp fixture dir");
        std::fs::write(dir.path().join("present.txt"), "hi").expect("failed to write fixture file");

        let passing = check_file_exists(dir.path(), "present.txt");
        assert!(
            passing.passed,
            "expected file_exists to pass for a present file, got: {passing:?}"
        );

        let failing = check_file_exists(dir.path(), "absent.txt");
        assert!(
            !failing.passed,
            "expected file_exists to fail for an absent file, got: {failing:?}"
        );
    }

    /// `check_git_diff` must pass when the fixture dir's real `git diff`
    /// output (uncommitted working-tree changes against a git repo's own
    /// HEAD) matches the expected diff text exactly, and fail on any
    /// mismatch.
    #[test]
    fn git_diff_assertion_matches_expected_diff_and_fails_on_mismatch() {
        let dir = tempfile::tempdir().expect("failed to create temp fixture dir");
        let repo = dir.path();

        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "rokr-eval-test")
                .env("GIT_AUTHOR_EMAIL", "rokr-eval-test@example.com")
                .env("GIT_COMMITTER_NAME", "rokr-eval-test")
                .env("GIT_COMMITTER_EMAIL", "rokr-eval-test@example.com")
                .status()
                .expect("failed to run git");
            assert!(status.success(), "git {args:?} failed");
        };

        run(&["init", "-q"]);
        std::fs::write(repo.join("tracked.txt"), "line one\n").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-q", "-m", "initial"]);

        // An uncommitted change in the working tree -- `check_git_diff`
        // compares against this.
        std::fs::write(repo.join("tracked.txt"), "line one\nline two\n").unwrap();

        let actual_diff_output = std::process::Command::new("git")
            .args(["diff"])
            .current_dir(repo)
            .output()
            .expect("failed to run git diff");
        let actual_diff = String::from_utf8_lossy(&actual_diff_output.stdout).into_owned();

        let passing = check_git_diff(repo, &actual_diff);
        assert!(
            passing.passed,
            "expected git_diff to pass when expected matches the real diff, got: {passing:?}"
        );

        let failing = check_git_diff(repo, "this is not the real diff at all");
        assert!(
            !failing.passed,
            "expected git_diff to fail on a mismatched expected diff, got: {failing:?}"
        );
    }

    /// `check_command_exit` must pass when the command run in the fixture
    /// dir exits with exactly the expected code, and fail otherwise.
    #[test]
    fn command_exit_assertion_passes_on_expected_code_and_fails_otherwise() {
        let dir = tempfile::tempdir().expect("failed to create temp fixture dir");

        let passing = check_command_exit(
            dir.path(),
            "sh",
            &["-c".to_string(), "exit 3".to_string()],
            3,
        );
        assert!(
            passing.passed,
            "expected command_exit to pass when the command's exit code matches, got: {passing:?}"
        );

        let failing = check_command_exit(
            dir.path(),
            "sh",
            &["-c".to_string(), "exit 3".to_string()],
            0,
        );
        assert!(
            !failing.passed,
            "expected command_exit to fail when the command's exit code doesn't match, got: {failing:?}"
        );
    }

    /// F-010: `check_assertion` must never panic when handed a
    /// `JudgeRubric` assertion, even though `lib.rs`'s per-case loop never
    /// actually dispatches one here in practice -- it must return a
    /// non-panicking, explanatorily-detailed failed outcome instead.
    #[test]
    fn check_assertion_reports_failed_outcome_for_judge_rubric_instead_of_panicking() {
        let dir = tempfile::tempdir().expect("failed to create temp fixture dir");
        let assertion = crate::case::Assertion::JudgeRubric {
            rubric: "did the agent do a good job?".to_string(),
        };

        let outcome = check_assertion(dir.path(), &assertion);

        assert!(
            !outcome.passed,
            "expected a JudgeRubric assertion routed through check_assertion to report failed, \
             got: {outcome:?}"
        );
        assert!(
            !outcome.detail.is_empty(),
            "expected an explanatory detail string, got an empty one"
        );
        assert!(
            outcome.detail.contains("judge") || outcome.detail.contains("score_rubric"),
            "expected the detail to explain that judge-rubric assertions are scored \
             elsewhere, got: {:?}",
            outcome.detail
        );
    }
}
