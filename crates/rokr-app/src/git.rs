//! Ticket 76 (git-context-snapshot): a size-capped, read-only snapshot of
//! the repo rokr is running in (branch, dirty flag, ahead/behind counts,
//! up to 5 recent commit subjects), folded into the system prompt at
//! session start exactly once -- following the same `load_memory`-segment
//! pattern already used for AGENTS.md/CLAUDE.md (`main.rs`, `headless.rs`).
//!
//! This module shells out to the real `git` subprocess (`std::process::
//! Command`) rather than pulling in a git library -- no new crate
//! dependency needed for read-only queries. It is also the intended future
//! home of `/commit`'s mutating `git commit` call (ticket 79): repo
//! detection and output parsing are shared, not duplicated, between the
//! read-only snapshot here and that future mutating use.

use std::path::Path;
use std::process::Command;

/// A point-in-time snapshot of the repo at `cwd`. Computed once at session
/// start and never recomputed mid-session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    pub branch: String,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub recent_subjects: Vec<String>,
}

impl GitContext {
    /// Plain-text body for the "# Git Context" system-prompt segment.
    pub fn to_prompt_text(&self) -> String {
        let mut text = String::new();
        text.push_str(&format!("Branch: {}\n", self.branch));
        text.push_str(&format!(
            "Status: {}\n",
            if self.dirty { "dirty" } else { "clean" }
        ));
        text.push_str(&format!(
            "Ahead/behind upstream: {} ahead, {} behind\n",
            self.ahead, self.behind
        ));
        if self.recent_subjects.is_empty() {
            text.push_str("Recent commits: (none)\n");
        } else {
            text.push_str("Recent commits:\n");
            for subject in &self.recent_subjects {
                text.push_str(&format!("- {subject}\n"));
            }
        }
        text
    }
}

/// Runs `git` with `args` in `cwd`, returning trimmed stdout on success
/// (exit code 0), `None` on any failure (non-zero exit, spawn failure --
/// e.g. `git` not on `PATH` -- or non-UTF8 output).
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Builds a [`GitContext`] snapshot for the repo containing `cwd`. Returns
/// `None` silently (no error, no placeholder) when `cwd` is not inside a
/// git work tree -- mirroring how an absent AGENTS.md is handled today.
pub fn snapshot(cwd: &Path) -> Option<GitContext> {
    run_git(cwd, &["rev-parse", "--is-inside-work-tree"])?;

    let branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;

    let dirty = run_git(cwd, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    // `git rev-list --left-right --count @{upstream}...HEAD` parses as
    // "<behind> <ahead>": the triple-dot symmetric-difference range puts
    // commits only in upstream (left side) first, commits only in HEAD
    // (right side) second. Fails when no upstream is configured (the
    // common case for a fresh local-only repo) -- default both to 0
    // rather than failing the whole snapshot.
    let (ahead, behind) = run_git(
        cwd,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    )
    .and_then(|s| {
        let mut parts = s.split_whitespace();
        let behind = parts.next()?.parse::<u32>().ok()?;
        let ahead = parts.next()?.parse::<u32>().ok()?;
        Some((ahead, behind))
    })
    .unwrap_or((0, 0));

    // `-5` itself caps the result at exactly 5 most-recent subjects,
    // newest first (git log's default order).
    let recent_subjects = run_git(cwd, &["log", "-5", "--pretty=%s"])
        .map(|s| s.lines().map(|line| line.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();

    Some(GitContext {
        branch,
        dirty,
        ahead,
        behind,
        recent_subjects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        assert!(Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .status()
            .expect("git init should spawn")
            .success());
        assert!(Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .status()
            .expect("git config should spawn")
            .success());
        assert!(Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .status()
            .expect("git config should spawn")
            .success());
    }

    fn commit(dir: &Path, file_name: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(file_name), contents).expect("failed to write fixture file");
        assert!(Command::new("git")
            .args(["add", file_name])
            .current_dir(dir)
            .status()
            .expect("git add should spawn")
            .success());
        assert!(Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(dir)
            .status()
            .expect("git commit should spawn")
            .success());
    }

    #[test]
    fn branch_dirty_ahead_behind_and_recent_subjects_are_read_from_real_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "first commit");
        std::fs::write(dir.path().join("untracked.txt"), "dirty").expect("write untracked file");

        let context = snapshot(dir.path()).expect("snapshot should be Some inside a git repo");

        assert_eq!(context.branch, "main");
        assert!(
            context.dirty,
            "expected dirty to be true with an untracked file present"
        );
        assert_eq!(context.ahead, 0);
        assert_eq!(context.behind, 0);
        assert_eq!(context.recent_subjects, vec!["first commit".to_string()]);
    }

    #[test]
    fn outside_a_git_repository_snapshot_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");

        let context = snapshot(dir.path());

        assert!(
            context.is_none(),
            "expected snapshot to be None outside a git repository, got: {context:?}"
        );
    }

    #[test]
    fn recent_commit_subjects_are_capped_at_five() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        for i in 1..=8 {
            commit(
                dir.path(),
                "a.txt",
                &format!("content {i}"),
                &format!("commit {i}"),
            );
        }

        let context = snapshot(dir.path()).expect("snapshot should be Some inside a git repo");

        assert_eq!(context.recent_subjects.len(), 5);
        assert_eq!(
            context.recent_subjects,
            vec![
                "commit 8".to_string(),
                "commit 7".to_string(),
                "commit 6".to_string(),
                "commit 5".to_string(),
                "commit 4".to_string(),
            ]
        );
    }
}
