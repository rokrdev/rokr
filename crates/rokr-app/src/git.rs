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

use std::path::{Path, PathBuf};
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

/// Error returned by [`commit`]. `CoAuthorNotAllowed` is a HARD RULE
/// (ticket 79): enforced here in code, not just in the message-drafting
/// prompt wording, so a hand-edited message smuggling a Co-Authored-By
/// trailer past drafting still can't reach a real `git commit` invocation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommitError {
    #[error("commit message must not contain a Co-Authored-By line or other agent attribution")]
    CoAuthorNotAllowed,
    #[error("`git add` failed for {0:?}")]
    Add(Vec<String>),
    #[error("`git commit` failed")]
    Commit,
}

/// True if any line of `message` is a `Co-Authored-By:` git trailer
/// (case-insensitive, leading whitespace ignored).
fn contains_co_author_line(message: &str) -> bool {
    message.lines().any(|line| {
        line.trim_start()
            .to_ascii_lowercase()
            .starts_with("co-authored-by:")
    })
}

/// Stages EXACTLY `paths` (never `-A`/`.`) and commits them with `message`,
/// via two real `git` subprocess calls: `git add -- <paths>` (so untracked
/// candidate files get indexed), then `git commit -m <message> -- <paths>`.
/// The trailing pathspec on `commit` (not just `add`) is what keeps the
/// result exact even when something else is ALREADY staged (ticket 79's
/// pre-staged-mismatch case): a pathspec-limited `git commit` only commits
/// modifications to the listed paths regardless of what else sits in the
/// index, and leaves that other staged content untouched (still staged,
/// neither committed nor reverted).
pub fn commit(cwd: &Path, paths: &[String], message: &str) -> Result<(), CommitError> {
    if contains_co_author_line(message) {
        return Err(CommitError::CoAuthorNotAllowed);
    }

    let add_status = Command::new("git")
        .arg("add")
        .arg("--")
        .args(paths)
        .current_dir(cwd)
        .status()
        .map_err(|_| CommitError::Add(paths.to_vec()))?;
    if !add_status.success() {
        return Err(CommitError::Add(paths.to_vec()));
    }

    let commit_status = Command::new("git")
        .args(["commit", "-m", message, "--"])
        .args(paths)
        .current_dir(cwd)
        .status()
        .map_err(|_| CommitError::Commit)?;
    if !commit_status.success() {
        return Err(CommitError::Commit);
    }

    Ok(())
}

/// Paths currently staged (the git index differs from `HEAD`), via `git
/// diff --cached --name-only` -- used by `/commit` to detect a pre-staged
/// mismatch against the session's candidate set. Empty (not an error) both
/// outside a git repo and when nothing is staged.
pub fn staged_paths(cwd: &Path) -> Vec<String> {
    run_git(cwd, &["diff", "--cached", "--name-only"])
        .map(|s| s.lines().map(|line| line.to_string()).collect())
        .unwrap_or_default()
}

/// The current branch name (`git rev-parse --abbrev-ref HEAD`) -- used by
/// `/pr` (ticket 80) to decide whether it's running on a protected branch
/// and to resolve which branch's commits get drafted into a PR title/body.
pub fn current_branch(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// The base branch `/pr` (ticket 80) diffs against: whichever of
/// `main`/`master` exists as a real ref in this repo, `main` preferred.
/// Falls back to `"main"` if neither resolves -- `commits_since_merge_base`
/// then simply returns `None`, which `/pr`'s caller already treats as
/// "nothing to draft from".
pub fn default_base_branch(cwd: &Path) -> String {
    for candidate in ["main", "master"] {
        if run_git(cwd, &["rev-parse", "--verify", candidate]).is_some() {
            return candidate.to_string();
        }
    }
    "main".to_string()
}

/// Commit subjects on `HEAD` since its merge-base with `base_branch`,
/// oldest first -- `/pr` (ticket 80) drafts its title/body from these.
/// `None` when the merge-base can't be determined (e.g. `base_branch`
/// doesn't exist, or the two branches share no history).
pub fn commits_since_merge_base(cwd: &Path, base_branch: &str) -> Option<Vec<String>> {
    let merge_base = run_git(cwd, &["merge-base", base_branch, "HEAD"])?;
    let range = format!("{merge_base}..HEAD");
    let log = run_git(cwd, &["log", "--reverse", "--pretty=%s", &range])?;
    Some(log.lines().map(|line| line.to_string()).collect())
}

/// The short SHA of `HEAD` (`git rev-parse --short HEAD`) -- used by `/pr`
/// (ticket 80) to build a suggested branch name when refusing to run on a
/// protected branch.
pub fn short_head_sha(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "--short", "HEAD"])
}

/// Creates and checks out a new branch named `name` (`git checkout -b`) --
/// used by `/pr` (ticket 80)'s refusal path on `main`/`master` to offer
/// branch creation as an alternative to proceeding.
pub fn create_branch(cwd: &Path, name: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args(["checkout", "-b", name])
        .current_dir(cwd)
        .status()
        .map_err(|err| format!("failed to spawn git: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git checkout -b {name} failed"))
    }
}

/// Resolves the work-tree root of the repo containing `cwd` (`git
/// rev-parse --show-toplevel`), canonicalized. `None` when `cwd` isn't
/// inside a git work tree.
fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    std::fs::canonicalize(root).ok()
}

/// Resolves `path` (as supplied on a `PermissionPayload::Diff`, which may be
/// absolute or relative -- see `runner.rs`'s `pre_edit_divergence_note`) to a
/// path relative to the repo's work-tree ROOT (NOT `cwd` -- rokr is commonly
/// launched from a subdirectory of a larger repo, and git's `<rev>:<path>`
/// syntax always resolves `<path>` relative to the work-tree root regardless
/// of the process's cwd), suitable for a `git show HEAD:<path>` pathspec. A
/// relative `path` is first joined onto `cwd` to make it absolute. Both
/// sides are canonicalized before stripping the prefix -- mirrors
/// `WriteTool::new`'s doc comment on why (macOS's `/var` -> `/private/var`
/// symlink resolution means a raw, non-canonical absolute path and a
/// canonical repo root otherwise fail to share a common prefix even when
/// they name the same file). `None` if `path` can't be resolved onto the
/// repo root at all (e.g. it names a file outside the repo, `cwd` isn't
/// inside a git work tree, or the file doesn't exist on disk yet).
fn to_repo_relative_pathspec(cwd: &Path, path: &str) -> Option<String> {
    let candidate = Path::new(path);
    let absolute_candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let root = repo_root(cwd)?;
    let canonical_candidate = std::fs::canonicalize(&absolute_candidate).ok()?;
    let relative = canonical_candidate.strip_prefix(&root).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// The file's content at `HEAD:<path>` in the repo at `cwd`, or `None` when
/// `cwd` isn't inside a git work tree, `path` can't be made repo-relative,
/// or the path simply doesn't exist at `HEAD` yet (e.g. a brand-new,
/// untracked file) -- all three collapse into "nothing to compare against".
///
/// Deliberately does NOT go through `run_git` here: `run_git`'s blanket
/// `.trim()` is right for the branch names/log subjects/etc. every other
/// caller uses it for, but wrong for a file's actual byte content -- it
/// would silently strip a trailing newline that `std::fs::read_to_string`
/// (the pre-image side of this comparison, see
/// `pre_image_diverges_from_head`) does NOT strip, making an untouched,
/// byte-identical file look like it diverges from `HEAD`.
///
/// Accepted limitation: a non-UTF8 (binary) file at `HEAD` also collapses
/// into this same `None`, since `String::from_utf8` rejects it -- so
/// `pre_image_diverges_from_head` treats a hand-edited binary file
/// identically to "doesn't exist at `HEAD`" and never flags it as
/// diverging. See the PRD's Further Notes for the accepted-limitation
/// writeup.
fn head_content_at_path(cwd: &Path, path: &str) -> Option<String> {
    let relative = to_repo_relative_pathspec(cwd, path)?;
    let output = Command::new("git")
        .args(["show", &format!("HEAD:{relative}")])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Ticket 81 (pre-edit-divergence-note): true if `pre_image` (the file's
/// full on-disk content just before a write/edit permission decision)
/// differs from its content at `HEAD` in the repo at `cwd` -- signals "this
/// file changed by a path rokr doesn't know about" (PRD "Divergence
/// safety") without blocking the write. Never flags when there's nothing to
/// compare against (outside a git repo, or a file that doesn't exist at
/// `HEAD` yet -- a brand-new file has nothing to diverge from).
pub fn pre_image_diverges_from_head(cwd: &Path, path: &str, pre_image: &str) -> bool {
    match head_content_at_path(cwd, path) {
        Some(head_content) => head_content != pre_image,
        None => false,
    }
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

    #[test]
    fn commit_stages_exactly_the_given_paths_and_writes_the_given_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "first commit");

        // a.txt gets a real, uncommitted modification -- it must NOT end up
        // in the new commit even though it's dirty, proving "exactly the
        // given paths" rather than "everything dirty".
        std::fs::write(dir.path().join("a.txt"), "modified after first commit")
            .expect("failed to modify a.txt");
        // b.txt is a brand new, untracked file -- the only path passed to
        // `commit`.
        std::fs::write(dir.path().join("b.txt"), "new file content")
            .expect("failed to write b.txt");

        let result = super::commit(dir.path(), &["b.txt".to_string()], "chore: add b.txt");
        assert!(
            result.is_ok(),
            "expected commit to succeed, got: {result:?}"
        );

        let subject_output = Command::new("git")
            .args(["log", "-1", "--pretty=%s"])
            .current_dir(dir.path())
            .output()
            .expect("git log should spawn");
        assert_eq!(
            String::from_utf8_lossy(&subject_output.stdout).trim(),
            "chore: add b.txt",
            "expected the new commit's subject to be exactly the given message"
        );

        let files_output = Command::new("git")
            .args(["show", "--name-only", "--pretty=format:", "HEAD"])
            .current_dir(dir.path())
            .output()
            .expect("git show should spawn");
        let files_stdout = String::from_utf8_lossy(&files_output.stdout).to_string();
        let committed_files: Vec<&str> = files_stdout
            .lines()
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            committed_files,
            vec!["b.txt"],
            "expected the new commit to contain EXACTLY the given paths, not a.txt's \
             uncommitted modification"
        );

        let status_output = Command::new("git")
            .args(["status", "--porcelain", "a.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git status should spawn");
        assert!(
            !String::from_utf8_lossy(&status_output.stdout)
                .trim()
                .is_empty(),
            "expected a.txt's modification to remain uncommitted, proving it was never staged"
        );
    }

    #[test]
    fn commit_message_never_contains_a_co_author_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "first commit");
        std::fs::write(dir.path().join("b.txt"), "new file content")
            .expect("failed to write b.txt");

        let result = super::commit(
            dir.path(),
            &["b.txt".to_string()],
            "chore: add b.txt\n\nCo-Authored-By: Some Agent <agent@example.com>",
        );

        assert_eq!(
            result,
            Err(CommitError::CoAuthorNotAllowed),
            "expected commit to refuse a message containing a Co-Authored-By trailer"
        );

        let log_output = Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(dir.path())
            .output()
            .expect("git log should spawn");
        let commit_count = String::from_utf8_lossy(&log_output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .count();
        assert_eq!(
            commit_count, 1,
            "expected no new commit to have been created when the message was refused"
        );
    }

    #[test]
    fn commits_since_merge_base_returns_subjects_oldest_first_on_feature_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "initial commit");
        assert!(Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(dir.path())
            .status()
            .expect("git checkout should spawn")
            .success());
        commit(dir.path(), "b.txt", "one", "feature commit one");
        commit(dir.path(), "c.txt", "two", "feature commit two");

        let subjects = commits_since_merge_base(dir.path(), "main")
            .expect("expected Some on a real feature branch with a real base");

        assert_eq!(
            subjects,
            vec![
                "feature commit one".to_string(),
                "feature commit two".to_string()
            ]
        );
    }

    #[test]
    fn commits_since_merge_base_is_none_when_base_branch_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "initial commit");

        let result = commits_since_merge_base(dir.path(), "nonexistent-base-branch");

        assert!(
            result.is_none(),
            "expected None when the base branch doesn't exist, got: {result:?}"
        );
    }

    #[test]
    fn create_branch_creates_and_checks_out_new_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "initial commit");

        let result = create_branch(dir.path(), "pr/abc123");

        assert!(
            result.is_ok(),
            "expected create_branch to succeed, got: {result:?}"
        );
        assert_eq!(
            current_branch(dir.path()),
            Some("pr/abc123".to_string()),
            "expected the repo to actually be checked out onto the new branch"
        );
    }

    #[test]
    fn default_base_branch_prefers_main_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello", "initial commit");

        assert_eq!(default_base_branch(dir.path()), "main");
    }

    #[test]
    fn pre_image_diverging_from_head_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello from HEAD", "initial commit");

        let diverges = pre_image_diverges_from_head(dir.path(), "a.txt", "hello from a user edit");

        assert!(
            diverges,
            "expected a pre-image differing from HEAD's content to be flagged as diverging"
        );
    }

    #[test]
    fn pre_image_matching_head_is_not_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "hello from HEAD", "initial commit");

        let diverges = pre_image_diverges_from_head(dir.path(), "a.txt", "hello from HEAD");

        assert!(
            !diverges,
            "expected a pre-image matching HEAD's content to not be flagged as diverging"
        );
    }

    /// Regression test: rokr is commonly launched from a subdirectory of a
    /// larger repo, not the repo's top-level directory -- nothing in the
    /// codebase enforces "cwd == repo root". `to_repo_relative_pathspec`
    /// must resolve the pathspec against the repo's work-tree root (`git
    /// rev-parse --show-toplevel`), not against whatever `cwd` happens to
    /// be, or `git show HEAD:<path>` is handed a path relative to the wrong
    /// directory, silently fails, and a genuinely hand-dirtied file is never
    /// flagged as diverging.
    #[test]
    fn pre_image_diverging_from_head_is_detected_when_cwd_is_a_subdirectory() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let sub_dir = dir.path().join("sub");
        std::fs::create_dir(&sub_dir).expect("failed to create subdirectory");
        commit(dir.path(), "sub/a.txt", "hello from HEAD", "initial commit");

        // `cwd` is the SUBDIRECTORY, not the repo root, and `path` is
        // relative to that subdirectory -- exactly how a real edit/write
        // permission prompt resolves paths when rokr itself was launched
        // from `sub/` rather than the repo root.
        let diverges = pre_image_diverges_from_head(&sub_dir, "a.txt", "hello from a user edit");

        assert!(
            diverges,
            "expected a pre-image differing from HEAD's content to be flagged as diverging \
             even when cwd is a subdirectory of the repo root"
        );
    }
}
