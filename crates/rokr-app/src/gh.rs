//! Ticket 80 (pr-command): a thin wrapper around the `gh` CLI's `pr create`
//! subcommand. Kept as a SEPARATE module from [`crate::git`] specifically
//! because PR creation has an isolated failure surface -- network, auth,
//! "gh not installed" -- that needs its own actionable error handling,
//! mirroring the specificity `bash.rs`'s `sandbox-exec`-not-found mapping
//! already established as the bar for this kind of external-dependency
//! failure (`crates/rokr-tools/src/bash.rs:133-148`).

use std::path::Path;
use std::process::Command;

/// Error creating a PR via `gh`. `NotInstalled` and `NotAuthenticated` are
/// deliberately distinct, actionably-worded variants (never collapsed into
/// a generic failure) -- see this module's doc comment.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GhError {
    #[error(
        "gh is not installed; install it from https://cli.github.com, then run manually:\n{0}"
    )]
    NotInstalled(String),
    #[error("gh is not authenticated; run `gh auth login`, then run manually:\n{0}")]
    NotAuthenticated(String),
    #[error("PR title/body must not contain a Co-Authored-By line or other agent attribution")]
    CoAuthorNotAllowed,
    #[error("gh pr create failed: {0}")]
    Failed(String),
}

/// `gh`'s own stderr, on a failed `pr create`, names the remediation
/// directly (e.g. "please run:  gh auth login") when the failure is an
/// auth problem -- this is real observed `gh` CLI wording, checked
/// case-insensitively so it survives minor `gh` version wording drift.
/// Anything else is a generic, still-actionable `Failed` (network error,
/// no repo on GitHub, etc.) rather than being misclassified as an auth
/// problem.
fn map_gh_pr_create_failure(stderr: &str, manual: &str) -> GhError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("gh auth login") || lower.contains("not logged") {
        GhError::NotAuthenticated(manual.to_string())
    } else {
        GhError::Failed(stderr.trim().to_string())
    }
}

/// Only `ErrorKind::NotFound` actually means "gh isn't on PATH" -- mirrors
/// `map_sandbox_exec_spawn_error`'s same distinction
/// (`crates/rokr-tools/src/bash.rs:133-148`): any other spawn error kind
/// (e.g. permission denied) means something else went wrong and must not
/// be collapsed into the "not installed" message.
fn map_gh_spawn_error(err: std::io::Error, manual: &str) -> GhError {
    if err.kind() == std::io::ErrorKind::NotFound {
        GhError::NotInstalled(manual.to_string())
    } else {
        GhError::Failed(format!("failed to spawn gh: {err}"))
    }
}

/// The exact manual `gh pr create` invocation a user can run themselves,
/// shown whenever `/pr` can't complete the flow on its own (gh missing,
/// unauthenticated, or any other failure).
pub fn manual_command(title: &str, body: &str) -> String {
    format!("gh pr create --title {title:?} --body {body:?}")
}

fn contains_co_author_line(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .to_ascii_lowercase()
            .starts_with("co-authored-by:")
    })
}

/// Runs `gh pr create --title <title> --body <body>` in `cwd`, returning the
/// PR URL `gh` prints to stdout on success.
pub fn create_pr(cwd: &Path, title: &str, body: &str) -> Result<String, GhError> {
    if contains_co_author_line(title) || contains_co_author_line(body) {
        return Err(GhError::CoAuthorNotAllowed);
    }

    let manual = manual_command(title, body);
    let output = Command::new("gh")
        .args(["pr", "create", "--title", title, "--body", body])
        .current_dir(cwd)
        .output()
        .map_err(|err| map_gh_spawn_error(err, &manual))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    Err(map_gh_pr_create_failure(
        &String::from_utf8_lossy(&output.stderr),
        &manual,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spawn error whose kind is `NotFound` (exactly what `execvp` reports
    /// when a binary isn't on `PATH`) must map to the specific, actionable
    /// `NotInstalled` variant -- not a generic `Failed`.
    #[test]
    fn gh_not_installed_produces_actionable_not_installed_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");

        let result = map_gh_spawn_error(io_err, "gh pr create --title \"t\" --body \"b\"");

        match result {
            GhError::NotInstalled(manual) => {
                assert!(
                    manual.contains("gh pr create"),
                    "expected the manual fallback command to be preserved, got: {manual}"
                );
            }
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    /// `gh`'s own not-authenticated stderr text (a real, stable substring of
    /// what `gh pr create` prints when `gh auth login` hasn't been run) must
    /// map to the specific `NotAuthenticated` variant -- and that variant
    /// must be distinguishable from `NotInstalled`, since the two require
    /// different user remediation.
    #[test]
    fn gh_not_authenticated_produces_actionable_auth_error_distinct_from_not_installed() {
        let stderr = "To get started with GitHub CLI, please run:  gh auth login\n";

        let result = map_gh_pr_create_failure(stderr, "gh pr create --title \"t\" --body \"b\"");

        match &result {
            GhError::NotAuthenticated(manual) => {
                assert!(
                    manual.contains("gh pr create"),
                    "expected the manual fallback command to be preserved, got: {manual}"
                );
            }
            other => panic!("expected NotAuthenticated, got {other:?}"),
        }

        let not_installed = map_gh_spawn_error(
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
            "gh pr create --title \"t\" --body \"b\"",
        );
        assert_ne!(
            std::mem::discriminant(&result),
            std::mem::discriminant(&not_installed),
            "not-authenticated and not-installed must be distinct error variants"
        );
    }
}
