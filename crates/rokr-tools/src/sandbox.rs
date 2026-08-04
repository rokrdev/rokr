//! The `Sandbox` trait and macOS `SeatbeltSandbox` backend: pure profile-string
//! generation for a deny-by-default filesystem/network confinement, per
//! `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md`. This module only
//! produces the profile text -- wrapping a real subprocess in a
//! `sandbox-exec` invocation is ticket 69's job, not this one's.

use std::path::{Path, PathBuf};

use crate::ToolError;

/// The permission grants a caller wants layered on top of the sandbox's
/// deny-by-default posture. `network` is the only field today: see
/// `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md` for why this is
/// deliberately minimal rather than anticipating grants the permission
/// model can't yet express.
pub struct Grants {
    pub network: bool,
}

/// Produces a sandbox profile string confining a subprocess to
/// `workspace_root` for filesystem writes and denying network access unless
/// `grants` allows it. Implementations are pure string builders -- no
/// subprocess spawning, no `sandbox-exec` invocation (`docs/adr/0015-sandbox-trait-and-seatbelt-backend.md`).
///
/// F-006 (pre-ship review, minor): the returned profile is a COMPLETE,
/// runnable profile on its own -- including the exec-enabling rules a
/// subprocess needs to merely start running under `(deny default)` at all
/// (previously these were concatenated on at the `bash.rs` call site, so
/// `profile_for`'s own output alone was not runnable, and any future
/// consumer would either duplicate those rules or get an incomplete
/// sandbox boundary).
pub trait Sandbox {
    /// Build the profile string for `workspace_root` under `grants`.
    /// Returns `Err` (rather than panicking) if `workspace_root` contains a
    /// byte that can't be safely interpolated into the profile (F-003: a
    /// newline or NUL byte survives quote/backslash escaping and can still
    /// terminate/inject SBPL statements).
    fn profile_for(&self, workspace_root: &Path, grants: &Grants) -> Result<String, ToolError>;
}

/// A [`Sandbox`] backed by macOS Seatbelt (`sandbox-exec` profile language).
/// See `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md` for why
/// Seatbelt -- deprecated but functional -- is accepted for this phase.
pub struct SeatbeltSandbox;

/// Escapes `\` and `"` so `value` can be safely embedded inside an SBPL
/// double-quoted string literal (F-003, pre-ship review, major).
/// Backslashes MUST be escaped first: escaping `"` before `\` would
/// double-escape the backslash just inserted ahead of each quote.
fn escape_sbpl_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Sandbox for SeatbeltSandbox {
    fn profile_for(&self, workspace_root: &Path, grants: &Grants) -> Result<String, ToolError> {
        let workspace_root_str = workspace_root.to_string_lossy();
        // F-003: escaping alone doesn't neutralize a raw newline or NUL
        // byte embedded in the path (a newline can still terminate/inject
        // an SBPL statement even inside an escaped string on some
        // parsers) -- reject those outright instead, surfaced as a typed
        // error the caller can report, rather than producing a profile
        // that might not mean what it looks like.
        if workspace_root_str.contains('\n') || workspace_root_str.contains('\0') {
            return Err(ToolError::ExecutionFailed(format!(
                "workspace root path cannot be used in a sandbox profile (contains a newline \
                 or NUL byte): {workspace_root_str:?}"
            )));
        }
        let escaped_workspace_root = escape_sbpl_string(&workspace_root_str);

        let mut profile = format!(
            "(version 1)\n(deny default)\n(allow file-write* (subpath \"{escaped_workspace_root}\"))\n"
        );
        if grants.network {
            profile.push_str("(allow network*)\n");
        }
        // F-006: exec-enabling rules folded into `profile_for` itself, not
        // appended at the `bash.rs` call site -- a bare `(deny default)`
        // profile blocks `sandbox-exec` from even exec'ing `/bin/sh`
        // (file-read of the binary itself is denied), so running ANY
        // command -- sandboxed or not -- needs these three, none of which
        // weaken the confinement established above: `file-read*` (read the
        // shell/coreutils binaries and their shared libraries),
        // `process-exec*` (actually exec them), and `process-fork` (shells
        // fork for pipelines/subshells, e.g. `echo x | cat`).
        profile.push_str("(allow file-read*)\n(allow process-exec*)\n(allow process-fork)\n");
        // F-002: without these three, common, entirely legitimate
        // in-workspace commands (git, node, python3, rustc) fail even
        // though only a bare `echo` fixture was ever proven to work
        // pre-fix. `(allow sysctl-read)` -- many runtimes/tools probe
        // sysctls at startup (e.g. CPU count, page size; `git` does this).
        // `(allow file-write-data (literal "/dev/null") ...)` --
        // redirecting output to `/dev/null` or the process's own
        // stdout/stderr is an extremely ordinary shell idiom, not a
        // confinement or exfiltration concern. `(allow mach-lookup)` --
        // many tools look up macOS system services via Mach ports even for
        // otherwise-local operations. Deliberately NOT widening to TMPDIR
        // here -- that's an explicit deferred human decision, untouched by
        // this fix.
        profile.push_str(
            "(allow sysctl-read)\n\
             (allow file-write-data (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\"))\n\
             (allow mach-lookup)\n",
        );
        Ok(profile)
    }
}

/// Lexically collapses `.`/`..` components in `path` without touching the
/// filesystem (unlike `canonicalize`, this works even when `path` -- or any
/// prefix of it -- doesn't exist). A `ParentDir` component pops the
/// previously pushed `Normal` component if there is one; otherwise (at the
/// root, or after another unresolved `ParentDir`) it's kept as-is. This is
/// what makes `path_is_within_workspace`'s ancestor walk-up safe: without
/// normalizing first, a missing tail containing `..` (e.g.
/// `workspace/newdir/../../outside/pwned.txt` where `newdir` doesn't exist)
/// would get lexically rejoined onto the canonicalized ancestor as literal
/// `..` components, and `Path::starts_with` only compares a literal
/// component prefix -- it would wrongly report the rejoined path as "inside"
/// `workspace_root` just because `workspace_root` is a textual prefix of it,
/// even though the `..`s actually walk back out to a sibling directory.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut stack: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                _ => stack.push(component),
            },
            other => stack.push(other),
        }
    }
    stack.into_iter().collect()
}

/// Resolves `path` against `workspace_root`, returning the fully-resolved
/// (symlink-free) absolute path if it lies inside `workspace_root`, or
/// `None` otherwise. This is the single source of truth for confinement:
/// [`path_is_within_workspace`] is defined in terms of it, and callers that
/// actually touch the filesystem (`WriteTool`/`EditTool::execute`) use the
/// RESOLVED path this returns, not the caller-supplied `path` -- writing to
/// the raw input after only checking it would reopen a symlink-swap TOCTOU
/// window between the check and the actual write.
///
/// F-001 (pre-ship review, blocker): the pre-fix implementation lexically
/// collapsed `.`/`..` components in the FULL path before ever touching the
/// filesystem. That is unsound whenever an EXISTING path component is
/// itself a symlink: `<ws>/link/../evil.txt`, where `link` is a symlink
/// pointing outside `workspace_root`, lexically collapses `link/..` to
/// nothing (looking like `<ws>/evil.txt`, safely inside) -- but the kernel
/// resolves `link` to its real target FIRST, so `link/..` actually lands in
/// the PARENT of that real target, which can be anywhere. The fix: (1) try
/// `canonicalize()` on the ORIGINAL path first (resolves symlinks and `..`
/// exactly as the kernel would, when the full path already exists); (2)
/// otherwise, find the LONGEST PREFIX of `path`'s components that exists on
/// disk -- checked as literal candidate paths, so the OS resolves any
/// symlink (and any `..` immediately following one, e.g. `<ws>/link/..`)
/// exactly as it would at execution time -- and `canonicalize()` THAT real
/// prefix; (3) rejoin the remaining ("missing") components and
/// `lexically_normalize` only that rejoined result. A nonexistent path
/// component can't be a symlink, so lexically resolving `..` in the missing
/// tail against the REAL canonicalized ancestor is sound.
pub fn resolve_within_workspace(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    resolve(path).filter(|resolved| resolved.starts_with(workspace_root))
}

fn resolve(path: &Path) -> Option<PathBuf> {
    // Fast path: if `path` fully exists, `canonicalize` resolves symlinks
    // and `..` exactly as the kernel would -- no need for the ancestor walk
    // below at all.
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }

    // `path` (or a component of it) doesn't exist yet. Find the longest
    // prefix of `path`'s components -- as literal candidate paths -- that
    // actually exists on disk.
    let components: Vec<std::path::Component> = path.components().collect();

    let mut split = components.len();
    while split > 0 {
        let candidate: PathBuf = components[..split].iter().collect();
        // R-001 (post-round-1 re-critique, blocker): `Path::exists()`
        // FOLLOWS symlinks -- it reports `false` for a DANGLING symlink
        // (one whose target doesn't exist), even though the symlink
        // itself is a real directory entry. That wrongly treated a
        // dangling symlink as a "missing" path component, walking past it
        // to its parent and lexically rejoining the symlink's own
        // in-workspace NAME onto the canonicalized parent below -- never
        // actually resolving through the symlink, so a dangling symlink
        // pointing OUTSIDE the workspace got judged "inside" it.
        // `symlink_metadata` reports "exists" for ANY symlink regardless
        // of where (or whether) its target exists, so a dangling symlink
        // correctly becomes the deepest "existing" ancestor below --
        // `canonicalize()` on it then fails closed (target absent), via
        // the `.ok()?` a few lines down, rather than silently falling
        // back to the lexical-rejoin path. Ordinary genuinely-missing
        // paths (no symlink involved) are unaffected: `symlink_metadata`
        // also returns `Err` for those, the same "doesn't exist, keep
        // walking up" signal `exists()` gave before.
        if candidate.symlink_metadata().is_ok() {
            break;
        }
        split -= 1;
    }

    // R-003 (post-round-1 re-critique, major): `split == 0` means NO
    // prefix of `path`'s components exists on disk -- which happens for
    // an entirely ordinary bare relative new filename (e.g. `README.md`)
    // when the process's cwd IS the workspace root itself: `canonicalize()`
    // on the resulting empty `PathBuf` always fails, wrongly rejecting a
    // completely ordinary new-file-at-workspace-root write as "outside the
    // workspace." The correct ancestor in that case is the cwd itself --
    // `starts_with(workspace_root)` in `resolve_within_workspace` still
    // applies normally afterward, so if cwd is NOT actually inside/equal
    // to `workspace_root`, the path is still correctly rejected. Absolute
    // paths can never hit `split == 0` (they always have at least a root
    // component as an existing ancestor), so this substitution is scoped
    // to the relative-path branch only and never changes absolute-path
    // handling.
    let existing_ancestor: PathBuf = if split == 0 && path.is_relative() {
        std::env::current_dir().ok()?
    } else {
        components[..split].iter().collect()
    };
    let canonical_ancestor = existing_ancestor.canonicalize().ok()?;

    // Rejoin the missing tail as literal components (preserving any `..`
    // tokens verbatim, rather than losing them the way `Path::file_name()`
    // would for a `ParentDir` component), then lexically resolve `..` in
    // THAT combined path only -- safe, since nothing in the missing tail
    // can be a symlink.
    let mut resolved = canonical_ancestor;
    for component in &components[split..] {
        resolved.push(component.as_os_str());
    }

    Some(lexically_normalize(&resolved))
}

/// Returns true if `path` resolves to a location inside `workspace_root`
/// (which must already be canonicalized by the caller -- see `BashTool::new`
/// / `WriteTool::new` / `EditTool::new`'s doc comments for why). Used by
/// ticket 70 (write-edit-path-confinement) as the in-process equivalent of
/// ticket 69's `sandbox-exec` confinement: `write`/`edit` call `std::fs`
/// directly rather than spawning a subprocess, so there's no profile to wrap
/// them in -- this is a plain path check instead. See
/// [`resolve_within_workspace`] for the resolution algorithm and for a
/// caller that needs the RESOLVED path, not just a bool.
pub fn path_is_within_workspace(path: &Path, workspace_root: &Path) -> bool {
    resolve_within_workspace(path, workspace_root).is_some()
}

#[cfg(test)]
mod tests {
    use super::{path_is_within_workspace, Grants, Sandbox, SeatbeltSandbox};
    use std::path::Path;

    #[test]
    fn profile_denies_writes_outside_workspace_root() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(temp.path(), &Grants { network: false })
            .unwrap();

        assert!(
            profile.contains("(deny default)"),
            "profile should deny-by-default so writes outside workspace_root are denied: {profile}"
        );
    }

    #[test]
    fn profile_allows_writes_inside_workspace_root() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(temp.path(), &Grants { network: false })
            .unwrap();

        let expected_rule = format!(
            "(allow file-write* (subpath \"{}\"))",
            temp.path().display()
        );
        assert!(
            profile.contains(&expected_rule),
            "profile should explicitly allow writes scoped to workspace_root: expected to find {expected_rule:?} in {profile}"
        );
    }

    #[test]
    fn profile_denies_network_by_default() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(temp.path(), &Grants { network: false })
            .unwrap();

        assert!(
            !profile.contains("(allow network*)"),
            "profile must not allow network when grants.network is false, network should stay denied via the default deny: {profile}"
        );
    }

    #[test]
    fn profile_allows_network_when_granted() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(temp.path(), &Grants { network: true })
            .unwrap();

        assert!(
            profile.contains("(allow network*)"),
            "profile should allow network when grants.network is true: {profile}"
        );
    }

    /// F-003 (pre-ship review, major): `workspace_root` is interpolated
    /// unescaped into the generated SBPL profile string -- a `"` in the
    /// path can break out of the `subpath` string literal and inject
    /// arbitrary SBPL. This constructs a workspace root string crafted so
    /// that, if interpolated unescaped, it closes the `subpath` string,
    /// injects a standalone `(allow network*)` form, then re-opens a
    /// dummy `subpath` string so the template's own trailing `"))\n` still
    /// parses -- i.e. syntactically VALID SBPL that actually grants
    /// network access even though `Grants { network: false }` was passed.
    /// No literal newline is used (SBPL is whitespace-delimited, spaces
    /// work identically) so this exercises the quote-escaping specifically,
    /// not the separate newline/NUL rejection this finding also requires.
    /// `sandbox-exec` is the real (not mocked) parser/enforcer here, per
    /// this project's PRD Testing Decisions. F-006 (already landed by the
    /// time this test runs): `profile_for`'s own output is a complete,
    /// runnable profile -- no exec-rule concatenation needed here.
    #[test]
    #[cfg(target_os = "macos")]
    fn profile_for_escapes_workspace_root_so_embedded_sbpl_cannot_inject_new_rules() {
        let malicious = "whatever\")) (allow network*) (allow file-write* (subpath \"dummy";
        let workspace_root = std::path::Path::new(malicious);

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(workspace_root, &Grants { network: false })
            .unwrap();

        let output = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("sh")
            .arg("-c")
            .arg("exec 3<>/dev/tcp/1.1.1.1/80")
            .output()
            .expect("sandbox-exec should spawn");

        assert!(
            !output.status.success(),
            "a workspace root crafted to inject `(allow network*)` via unescaped SBPL \
             interpolation must NOT actually grant network access -- profile was: {profile}\n\
             stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// F-003 (pre-ship review, major): a pure string-level assertion
    /// (no `sandbox-exec`, so this runs on every platform) that the
    /// escaped form of a workspace root containing BOTH a `"` and a `\`
    /// appears correctly in the generated profile -- backslashes escaped
    /// first, then quotes, so the escaping itself can't be
    /// mis-ordered/double-escaped.
    #[test]
    fn profile_for_escapes_quote_and_backslash_in_workspace_root() {
        let workspace_root = std::path::Path::new("/tmp/weird\"quote\\backslash");

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(workspace_root, &Grants { network: false })
            .unwrap();

        assert!(
            profile.contains("(allow file-write* (subpath \"/tmp/weird\\\"quote\\\\backslash\"))"),
            "expected the escaped workspace root (\\\" for the quote, \\\\ for the backslash) \
             to appear correctly in the generated profile: {profile}"
        );
    }

    /// F-003: a raw newline byte in the workspace root must be rejected
    /// outright (escaping alone doesn't neutralize it), surfaced as a
    /// typed error rather than a panic.
    #[test]
    fn profile_for_rejects_workspace_root_containing_newline() {
        let workspace_root = std::path::Path::new("/tmp/evil\ninjected");

        let sandbox = SeatbeltSandbox;
        let result = sandbox.profile_for(workspace_root, &Grants { network: false });

        assert!(
            matches!(result, Err(crate::ToolError::ExecutionFailed(_))),
            "a workspace root containing a newline byte must be rejected with a typed error, \
             got {result:?}"
        );
    }

    /// F-006 (pre-ship review, minor): `profile_for`'s own output must be a
    /// COMPLETE, runnable profile -- the exec-enabling rules (plus F-002's
    /// additions) must be present in its output directly, not appended by
    /// the `bash.rs` call site.
    #[test]
    fn profile_for_includes_exec_enabling_and_f002_rules_directly() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox
            .profile_for(temp.path(), &Grants { network: false })
            .unwrap();

        for expected_rule in [
            "(allow file-read*)",
            "(allow process-exec*)",
            "(allow process-fork)",
            "(allow sysctl-read)",
            "(allow file-write-data (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\"))",
            "(allow mach-lookup)",
        ] {
            assert!(
                profile.contains(expected_rule),
                "expected profile_for's own output to include {expected_rule:?}: {profile}"
            );
        }
    }

    #[test]
    fn path_is_within_workspace_true_for_existing_in_workspace_path() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let existing_file = canonical_root.join("existing.txt");
        std::fs::write(&existing_file, "content").unwrap();

        assert!(path_is_within_workspace(&existing_file, &canonical_root));
    }

    #[test]
    fn path_is_within_workspace_false_for_existing_out_of_workspace_path() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let existing_file = outside.path().join("existing.txt");
        std::fs::write(&existing_file, "content").unwrap();

        assert!(!path_is_within_workspace(&existing_file, &canonical_root));
    }

    #[test]
    fn path_is_within_workspace_true_for_not_yet_existing_in_workspace_path() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let new_file = canonical_root.join("brand-new.txt");
        assert!(!new_file.exists(), "precondition: file must not exist yet");

        assert!(path_is_within_workspace(&new_file, &canonical_root));
    }

    #[test]
    fn path_is_within_workspace_false_for_not_yet_existing_out_of_workspace_path() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canonical_outside = outside.path().canonicalize().unwrap();
        let new_file = canonical_outside.join("brand-new.txt");
        assert!(!new_file.exists(), "precondition: file must not exist yet");

        assert!(!path_is_within_workspace(&new_file, &canonical_root));
    }

    /// F-001 (pre-ship review, blocker): a REAL symlink inside the
    /// workspace pointing outside it, combined with a trailing `..`, must
    /// not be judged "inside" the workspace. Lexically,
    /// `<ws>/link/../evil.txt` collapses (by cancelling `link` against the
    /// following `..`) to `<ws>/evil.txt`, which LOOKS like it stays
    /// inside -- but the kernel resolves `link` to its real target FIRST,
    /// so `link/..` actually lands in the parent of wherever `link`
    /// points, not back in the workspace. Lexical normalization of the
    /// FULL path before any filesystem resolution (the pre-fix behavior)
    /// misses this entirely.
    #[test]
    #[cfg(unix)]
    fn path_is_within_workspace_false_for_symlink_then_dotdot_escape() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canonical_outside = outside.path().canonicalize().unwrap();

        let link = canonical_root.join("link");
        std::os::unix::fs::symlink(&canonical_outside, &link).unwrap();
        let traversal_path = link.join("..").join("evil.txt");

        assert!(
            !path_is_within_workspace(&traversal_path, &canonical_root),
            "a symlink-then-.. escape must not be judged inside the workspace"
        );
    }

    /// R-001 (post-round-1 re-critique, blocker): `Path::exists()` FOLLOWS
    /// symlinks, so a DANGLING symlink (one whose target doesn't exist)
    /// reports `false` from `exists()` even though the symlink itself is a
    /// real directory entry. Pre-fix, `resolve`'s ancestor walk used
    /// `exists()` to decide "does this candidate exist on disk," so a
    /// dangling symlink got treated as a missing path component and
    /// skipped past -- its parent became the "existing" ancestor, and the
    /// symlink's own name got lexically rejoined onto it as an ordinary
    /// `Normal` component, never actually resolving through the symlink at
    /// all. That let a path through a dangling symlink pointing OUTSIDE the
    /// workspace be judged "inside" it, because the lexical rejoin only
    /// ever sees the symlink's in-workspace NAME, never its real (absent)
    /// target.
    #[test]
    #[cfg(unix)]
    fn path_is_within_workspace_false_for_dangling_symlink() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canonical_outside = outside.path().canonicalize().unwrap();

        // The symlink's target does NOT exist -- "dangling" -- and lives
        // outside the workspace root.
        let dangling_target = canonical_outside.join("newfile.txt");
        assert!(
            !dangling_target.exists(),
            "precondition: the symlink target must not exist"
        );
        let dangling_link = canonical_root.join("dangling");
        std::os::unix::fs::symlink(&dangling_target, &dangling_link).unwrap();

        assert!(
            !path_is_within_workspace(&dangling_link, &canonical_root),
            "a path through a dangling symlink pointing outside the workspace must not be \
             judged inside it"
        );
    }

    /// R-003 (post-round-1 re-critique, major): serializes tests that
    /// mutate the process-wide cwd, so this test's `set_current_dir` can't
    /// interleave with another test doing the same on a concurrently
    /// running thread within this same test binary.
    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard restoring the process cwd on drop -- so a panicking
    /// assertion mid-test still leaves the process cwd sane for whichever
    /// test runs next, rather than leaking the temp-dir override past this
    /// test's own scope.
    struct CwdGuard(std::path::PathBuf);

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    /// R-003 (post-round-1 re-critique, major): when the ancestor walk
    /// yields an EMPTY existing ancestor -- which happens for a bare
    /// relative new filename (`README.md`) when the process's cwd IS the
    /// workspace root itself and the file doesn't exist yet -- the pre-fix
    /// code canonicalized the empty `PathBuf`, which always fails, wrongly
    /// rejecting this completely ordinary write as "outside the
    /// workspace." Reproduced here by actually changing the process cwd to
    /// the workspace root (rather than just constructing an
    /// already-workspace-rooted absolute path), since that's the real
    /// condition production code hits: `WriteTool`/`EditTool` resolve
    /// whatever relative path the caller supplied against the process's
    /// real cwd.
    #[test]
    fn path_is_within_workspace_true_for_bare_relative_new_filename_when_cwd_is_workspace_root() {
        let _lock = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&canonical_root).unwrap();
        let _restore = CwdGuard(original_cwd);

        let bare_relative = Path::new("README.md");
        assert!(
            !canonical_root.join("README.md").exists(),
            "precondition: the file must not exist yet"
        );

        assert!(
            path_is_within_workspace(bare_relative, &canonical_root),
            "a bare relative new filename must resolve inside the workspace when cwd IS the \
             workspace root"
        );
    }

    /// R-003: a relative path that walks ABOVE cwd via `..` must still be
    /// correctly rejected -- this fix only widens what counts as "the
    /// existing ancestor" when nothing at all exists on disk yet, it must
    /// not weaken the `starts_with(workspace_root)` check that runs
    /// afterward.
    #[test]
    fn path_is_within_workspace_false_for_relative_dotdot_above_cwd_at_workspace_root() {
        let _lock = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&canonical_root).unwrap();
        let _restore = CwdGuard(original_cwd);

        let traversal_path = Path::new("../escaped.txt");

        assert!(
            !path_is_within_workspace(traversal_path, &canonical_root),
            "a relative path walking above cwd via .. must not be judged inside the workspace"
        );
    }

    #[test]
    fn path_is_within_workspace_false_for_dotdot_traversal_via_nonexistent_dir() {
        let workspace_root = tempfile::tempdir().unwrap();
        let canonical_root = workspace_root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canonical_outside = outside.path().canonicalize().unwrap();

        // `newdir` doesn't exist, so the walk-up must land past it, resolve
        // the `..` components lexically, and correctly determine that this
        // path actually escapes to a sibling directory outside the
        // workspace root -- not merely check a literal string prefix.
        let traversal_path = canonical_root
            .join("newdir")
            .join("..")
            .join("..")
            .join(canonical_outside.file_name().unwrap())
            .join("brand-new.txt");

        assert!(!path_is_within_workspace(&traversal_path, &canonical_root));
    }
}
