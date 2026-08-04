//! Ticket 77 (read-only-git-carveout): a pure, no-I/O classifier answering
//! "is this bash command an unambiguously read-only git invocation" --
//! `status`, `diff`, `log`, `show`, or `blame`, with no shell
//! metacharacters, no global options between `git` and the subcommand, and
//! every flag on a fixed allowlist. `crate::permission_policy::
//! PermissionPolicy::resolve` consults [`is_read_only_git`] to convert what
//! would otherwise be `Resolution::Prompt` into `Resolution::Allow` for the
//! `bash` tool -- never anything else, per ADR 0019
//! (`docs/adr/0019-read-only-git-permission-carveout.md`), which also
//! records this classifier's exact conservatism spec and the residual risks
//! accepted, not mitigated, by keeping it this narrow.
//!
//! This module does no I/O and has no side effects: it never spawns a
//! process (unlike `crate::git`, which shells out to real `git`) and never
//! consults the filesystem or `PATH`. It answers a purely syntactic
//! question about the command string the model proposed to run.

/// Shell metacharacters that disqualify a command outright, wherever they
/// appear. `=` is deliberately included: it rejects `--flag=value` forms at
/// this step, before the flag allowlist is ever consulted, so an
/// allowlisted flag must appear as its own whitespace-split token.
const DISQUALIFYING_METACHARACTERS: &[char] = &[
    ';', '&', '|', '<', '>', '\n', '(', ')', '{', '}', '`', '$', '*', '?', '[', ']', '\'', '"',
    '\\', '#', '~', '=',
];

/// The five subcommands this carve-out recognizes as unambiguously
/// read-only, per ADR 0019.
const READ_ONLY_SUBCOMMANDS: &[&str] = &["status", "diff", "log", "show", "blame"];

/// The fixed flag allowlist, per ADR 0019. `-<digits>` (e.g. `-5`, `-10`) is
/// handled separately below since it isn't a fixed string.
const ALLOWED_FLAGS: &[&str] = &[
    "-p",
    "-s",
    "--stat",
    "--numstat",
    "--name-only",
    "--name-status",
    "--oneline",
    "--graph",
    "--decorate",
    "--no-color",
    "--short",
    "--porcelain",
    "-n",
    "--max-count",
    "--author",
    "--since",
    "--until",
    "--grep",
];

/// Returns true if every char of `flag` after a leading run of `-` is an
/// ASCII digit, and at least one digit is present -- i.e. `flag` matches
/// `-<digits>` (e.g. `-5`, `-10`), the numeric shorthand for `--max-count`.
fn is_numeric_flag(flag: &str) -> bool {
    match flag.strip_prefix('-') {
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Returns true if `command` is an unambiguously read-only git invocation,
/// per ADR 0019's fixed conservatism spec. Any ambiguity fails closed to
/// `false` (which `PermissionPolicy::resolve` turns into `Resolution::
/// Prompt`, never `Resolution::Allow`).
///
pub fn is_read_only_git(command: &str) -> bool {
    if command.contains(DISQUALIFYING_METACHARACTERS) {
        return false;
    }

    let mut tokens = command.split_whitespace();

    if tokens.next() != Some("git") {
        return false;
    }

    match tokens.next() {
        Some(subcommand) if READ_ONLY_SUBCOMMANDS.contains(&subcommand) => {}
        _ => return false,
    }

    for token in tokens {
        if !token.starts_with('-') {
            // Non-dash positional tokens (paths, revisions, flag values)
            // pass through unchecked.
            continue;
        }
        if !ALLOWED_FLAGS.contains(&token) && !is_numeric_flag(token) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_diff_log_show_blame_with_allowlisted_flags_classify_as_read_only() {
        for command in [
            "git status",
            "git status -s",
            "git diff",
            "git diff --stat",
            "git log",
            "git log -n -5 --oneline",
            "git show",
            // Note: `HEAD~1` is deliberately NOT used here -- `~` is one of
            // the disqualifying shell metacharacters per ADR 0019's spec
            // (and the PRD it transcribes), so a revision containing it
            // would correctly fail classification. `HEAD^` (first-parent
            // syntax) exercises the same "positional revision arg passes
            // through unchecked" behavior without hitting that rule.
            "git show --no-color HEAD^",
            "git blame",
            "git blame -p file.rs",
        ] {
            assert!(
                is_read_only_git(command),
                "expected {command:?} to classify as read-only git"
            );
        }
    }

    #[test]
    fn shell_metacharacters_disqualify_regardless_of_subcommand() {
        for command in [
            "git status && rm -rf x",
            "git log | less",
            "git diff; rm x",
            "git log --author=me",
        ] {
            assert!(
                !is_read_only_git(command),
                "expected {command:?} to be disqualified by a shell metacharacter"
            );
        }
    }

    #[test]
    fn global_options_between_git_and_subcommand_disqualify() {
        for command in [
            "git -C .. log",
            "git -c core.pager=cat status",
            "git --git-dir=/tmp/x status",
        ] {
            assert!(
                !is_read_only_git(command),
                "expected {command:?} to be disqualified by a global option before the subcommand"
            );
        }
    }

    #[test]
    fn unrecognized_flag_fails_closed_to_prompt() {
        assert!(
            !is_read_only_git("git log --follow"),
            "expected an unrecognized bare flag to fail closed"
        );
    }

    #[test]
    fn empty_or_subcommand_less_or_unrecognized_subcommand_commands_fail_closed() {
        for command in [
            "",
            "git",
            "git ",
            "git commit",
            "git commit -m x",
            "gitstatus",
            "not-git status",
        ] {
            assert!(
                !is_read_only_git(command),
                "expected {command:?} to fail closed (not classify as read-only git)"
            );
        }
    }

    #[test]
    fn max_count_numeric_shorthand_and_positional_paths_pass_through() {
        for command in [
            "git log -5",
            "git log -10 --oneline",
            "git diff --stat src/main.rs",
            "git blame src/lib.rs",
            "git log --author me --since yesterday",
        ] {
            assert!(
                is_read_only_git(command),
                "expected {command:?} to classify as read-only git"
            );
        }
    }
}
