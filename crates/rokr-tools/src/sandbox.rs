//! The `Sandbox` trait and macOS `SeatbeltSandbox` backend: pure profile-string
//! generation for a deny-by-default filesystem/network confinement, per
//! `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md`. This module only
//! produces the profile text -- wrapping a real subprocess in a
//! `sandbox-exec` invocation is ticket 69's job, not this one's.

use std::path::{Path, PathBuf};

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
pub trait Sandbox {
    /// Build the profile string for `workspace_root` under `grants`.
    fn profile_for(&self, workspace_root: &Path, grants: &Grants) -> String;
}

/// A [`Sandbox`] backed by macOS Seatbelt (`sandbox-exec` profile language).
/// See `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md` for why
/// Seatbelt -- deprecated but functional -- is accepted for this phase.
pub struct SeatbeltSandbox;

impl Sandbox for SeatbeltSandbox {
    fn profile_for(&self, workspace_root: &Path, grants: &Grants) -> String {
        let mut profile = format!(
            "(version 1)\n(deny default)\n(allow file-write* (subpath \"{}\"))\n",
            workspace_root.display()
        );
        if grants.network {
            profile.push_str("(allow network*)\n");
        }
        profile
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

/// Returns true if `path` resolves to a location inside `workspace_root`
/// (which must already be canonicalized by the caller -- see `BashTool::new`
/// / `WriteTool::new` / `EditTool::new`'s doc comments for why). Used by
/// ticket 70 (write-edit-path-confinement) as the in-process equivalent of
/// ticket 69's `sandbox-exec` confinement: `write`/`edit` call `std::fs`
/// directly rather than spawning a subprocess, so there's no profile to wrap
/// them in -- this is a plain path check instead.
///
/// Handles paths that don't exist yet (e.g. a `write` target about to be
/// created) by first lexically normalizing `path` (see
/// `lexically_normalize`), then walking up to the nearest existing ancestor
/// directory, canonicalizing THAT (which resolves any symlinks in the
/// existing portion), then rejoining the remaining non-existent path
/// components and checking the result against `workspace_root`.
///
/// Fails closed: if even the topmost ancestor can't be canonicalized (should
/// only happen in pathological environments), this returns `false` rather
/// than erroring, since the caller only needs a bool.
pub fn path_is_within_workspace(path: &Path, workspace_root: &Path) -> bool {
    let normalized = lexically_normalize(path);

    if let Ok(canonical) = normalized.canonicalize() {
        return canonical.starts_with(workspace_root);
    }

    // `normalized` doesn't exist yet. Walk up to the nearest existing
    // ancestor, canonicalize it, then rejoin the non-existent tail.
    let mut existing_ancestor: &Path = normalized.as_path();
    let mut missing_tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        match existing_ancestor.parent() {
            Some(parent) => {
                missing_tail.push(existing_ancestor.file_name().unwrap_or_default());
                existing_ancestor = parent;
                if existing_ancestor.exists() {
                    break;
                }
            }
            None => break,
        }
    }

    let Ok(canonical_ancestor) = existing_ancestor.canonicalize() else {
        return false;
    };

    let resolved = missing_tail
        .into_iter()
        .rev()
        .fold(canonical_ancestor, |acc, component| acc.join(component));

    resolved.starts_with(workspace_root)
}

#[cfg(test)]
mod tests {
    use super::{path_is_within_workspace, Grants, Sandbox, SeatbeltSandbox};

    #[test]
    fn profile_denies_writes_outside_workspace_root() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox.profile_for(temp.path(), &Grants { network: false });

        assert!(
            profile.contains("(deny default)"),
            "profile should deny-by-default so writes outside workspace_root are denied: {profile}"
        );
    }

    #[test]
    fn profile_allows_writes_inside_workspace_root() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox.profile_for(temp.path(), &Grants { network: false });

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
        let profile = sandbox.profile_for(temp.path(), &Grants { network: false });

        assert!(
            !profile.contains("(allow network*)"),
            "profile must not allow network when grants.network is false, network should stay denied via the default deny: {profile}"
        );
    }

    #[test]
    fn profile_allows_network_when_granted() {
        let temp = tempfile::tempdir().unwrap();

        let sandbox = SeatbeltSandbox;
        let profile = sandbox.profile_for(temp.path(), &Grants { network: true });

        assert!(
            profile.contains("(allow network*)"),
            "profile should allow network when grants.network is true: {profile}"
        );
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
